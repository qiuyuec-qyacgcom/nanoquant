"""曾经"只存不读"的配置项，现在必须真的生效.

这类问题的共性：设置页给出了说明、写进了数据库、界面上还能调，
但后端没有任何一行代码读它。跑测试不会红，只有真出事那天才发现按钮没用。
本文件逐项钉住这些开关的实际效果，防止以后重构又把它悄悄断掉。
"""
from __future__ import annotations

import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.exchange import broker
from nanoquant.exchange.precision import SymbolFilters
from nanoquant.models import NewsItem
from nanoquant.services.settings import update_group


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


# --------------------------------------------------------- min_notional_buffer
def _risk(**kw) -> SimpleNamespace:
    base = {"risk_per_trade_pct": 5.26, "margin_pct": 50.0, "min_notional_buffer": 1.05}
    base.update(kw)
    return SimpleNamespace(**base)


def _filters() -> SymbolFilters:
    # 100 USDT 的标的，交易所下限 5 USDT → 裸最小量 0.05，1.05x 缓冲后 0.053
    return SymbolFilters(
        symbol="BUFTEST", price_tick=0.01, qty_step=0.001, min_qty=0.001, min_notional=5.0,
        last_price=100.0,
    )


def test_min_notional_buffer_bumps_to_buffered_floor():
    """预算 5.26 USDT：裸下限 0.05 能过，但缓冲后 0.053 才是目标量——应抬到 0.053."""
    qty, why = broker.estimate_qty(100.0, 100.0, 10, _risk(), 1.0, _filters())

    assert why == "ok"
    assert qty == pytest.approx(0.053)
    assert qty * 100.0 >= 5.0 * 1.05 - 1e-9


def test_min_notional_buffer_rejects_instead_of_hugging_exchange_floor():
    """缓冲 3x 要求 15 USDT，预算只有 5.26——必须拒绝，而不是贴着 5 USDT 下限下单."""
    qty, why = broker.estimate_qty(
        100.0, 100.0, 10, _risk(min_notional_buffer=3.0), 1.0, _filters()
    )

    assert qty == 0.0
    assert "缓冲" in why


def test_min_notional_buffer_never_overrides_margin_cap():
    """保证金不够时不能为了过门槛把量放大."""
    qty, why = broker.estimate_qty(
        100.0, 100.0, 10, _risk(), 1.0, _filters(), available=0.4
    )

    assert qty == 0.0
    assert "保证金" in why or "不足" in why


def test_buffer_defaults_to_one_when_missing():
    assert broker.min_notional_buffer(SimpleNamespace()) == 1.0  # noqa: SLF001
    assert broker.min_notional_buffer(SimpleNamespace(min_notional_buffer=9)) == 3.0  # noqa: SLF001


# ------------------------------------------------------------ AI 议会一致度
def test_agreement_gate_blocks_split_votes():
    from nanoquant.engine.worker import AGREEMENT_MIN, ai_agreement_reject

    assert ai_agreement_reject(True, AGREEMENT_MIN - 0.01)
    assert ai_agreement_reject(True, 1.0) == ""
    # 开关关闭时无论多分歧都放行（保持老行为）
    assert ai_agreement_reject(False, 0.1) == ""


# --------------------------------------------------------------- 入场腿费率
def test_entry_fee_rate_follows_order_type():
    risk = SimpleNamespace(fee_rate_taker=0.0005, fee_rate_maker=0.0002)
    old = update_group("engine", {"order_type": "market"})
    try:
        assert broker._entry_fee_rate(risk) == pytest.approx(0.0005)  # noqa: SLF001
        update_group("engine", {"order_type": "limit_post_only"})
        assert broker._entry_fee_rate(risk) == pytest.approx(0.0002)  # noqa: SLF001
    finally:
        update_group("engine", {"order_type": old["order_type"]})


# ------------------------------------------------------------------ 取数根数
def test_scan_bars_follows_kline_limit():
    from nanoquant.engine.worker import _scan_bars

    old = update_group("data", {"kline_limit": 200})
    try:
        assert _scan_bars() == 200
        update_group("data", {"kline_limit": 50})
        assert _scan_bars() == 60, "低于策略指标所需根数时不能照单全收"
        update_group("data", {"kline_limit": 1500})
        assert _scan_bars() == 500
    finally:
        update_group("data", {"kline_limit": old["kline_limit"]})


# ---------------------------------------------------------------- 情绪阈值
def _news(sentiment: float, title: str) -> NewsItem:
    return NewsItem(
        ts=int(time.time()), source="cointelegraph", title=title,
        summary="", url="", sentiment=sentiment, symbols=[],
        dedupe=f"dead-switch-{title}-{time.time_ns()}",
    )


def test_digest_puts_significant_news_first():
    from nanoquant.news import service as news_service

    old = update_group("news", {"sentiment_threshold": 0.6})
    try:
        with session_scope() as s:
            s.add(_news(0.02, "DIGEST-平淡"))
            s.add(_news(0.8, "DIGEST-大额流入"))
            s.add(_news(-0.7, "DIGEST-巨鲸砸盘"))

        items = news_service.digest(3)
        titles = [i["title"] for i in items]
        assert "DIGEST-大额流入" in titles[:2] and "DIGEST-巨鲸砸盘" in titles[:2], titles
        assert items[0]["significant"] is True
        assert any(i["significant"] is False for i in items), "低于阈值的仍要保留，只是排后面"
    finally:
        update_group("news", {"sentiment_threshold": old["sentiment_threshold"]})


def test_overview_threshold_changes_counts():
    from nanoquant.news import service as news_service

    with session_scope() as s:
        s.add(_news(0.2, "OVW-轻微正面"))
    old = update_group("news", {"sentiment_threshold": 0.1})
    try:
        loose = news_service.overview()
        update_group("news", {"sentiment_threshold": 0.9})
        tight = news_service.overview()
    finally:
        update_group("news", {"sentiment_threshold": old["sentiment_threshold"]})

    assert tight["positive"] < loose["positive"]
    assert tight["sentiment_threshold"] == pytest.approx(0.9)


# ------------------------------------------------------------- 熔断即离场
def test_flatten_on_halt_runs_once_per_window(monkeypatch):
    """`engine.close_on_halt` 的实现：熔断时平仓，但同一次事故只打一次."""
    from nanoquant.engine.worker import TradingEngine

    eng = TradingEngine()
    calls: list[dict] = []

    async def fake_flatten(*, reason: str, stop_engine: bool):
        calls.append({"reason": reason, "stop_engine": stop_engine})
        return {"closed": [{"symbol": "X"}], "failed": []}

    monkeypatch.setattr(eng, "flatten_all", fake_flatten)

    import asyncio

    asyncio.run(eng._flatten_on_halt("最大回撤 60.0% 超过上限 50.0%"))  # noqa: SLF001
    asyncio.run(eng._flatten_on_halt("最大回撤 61.0% 超过上限 50.0%"))  # noqa: SLF001

    assert len(calls) == 1, "平仓失败/连续熔断时不能每轮都全平一次"
    assert calls[0]["stop_engine"] is False, "熔断离场不该顺手把引擎停掉（还得继续管账）"
    assert "熔断离场" in calls[0]["reason"]


# ------------------------------------------------- 负向心法自动降权
def test_auto_disable_negative_mantra(monkeypatch):
    """实测弱于对照组的心法，开关打开时必须真的被停用（而不是只写个标记）."""
    from nanoquant.evolution import effectiveness, health, mantra
    from nanoquant.models import Mantra

    update_group("evolution", {"auto_disable_negative": True})
    with session_scope() as s:
        row = Mantra(title="DEADSWITCH-心法", rule="追高不加仓", category="risk", confidence=0.5)
        s.add(row)
        s.flush()
        mid = row.id

    monkeypatch.setattr(
        effectiveness,
        "evaluate",
        lambda limit=500: {"control": {"avg_pnl": 1.0}, "per_mantra": [{"id": mid, "trades": 999, "avg_pnl": -5.0}]},
    )
    monkeypatch.setattr(
        health,
        "refresh_lesson_health",
        lambda lessons, **kw: (
            [dict(item, health=0.4, health_scored=True, enabled=True, evidence={}) for item in lessons],
            [],
        ),
    )

    out = mantra.refresh_health_from_evidence()

    assert out["disabled"] == 1
    with session_scope() as s:
        row = s.get(Mantra, mid)
        assert row.enabled is False
        assert "auto_disable_negative" in (row.evidence or {}).get("disabled_reason", "")
        s.delete(row)
    update_group("evolution", {"auto_disable_negative": False})


# ------------------------------------------------- 防回归：不允许再出现死开关
#: 只在前端消费的字段（后端读不到是正常的），新增时必须在这里登记并写清原因，
#: 否则测试会红——这道闸门存在的意义就是"加了设置就必须有人读"。
FRONTEND_ONLY = {
    "ui_refresh_sec": "前端轮询间隔，后端不参与",
    "risk_disclaimer_accepted": "前端风险提示勾选状态",
    "max_position_leverage": "已停用：开仓杠杆统一由风控上下限钳制，保留字段兼容老配置",
}


def test_no_settings_field_is_defined_but_unread():
    """扫描 settings.py 的每个字段，在后端代码里必须至少被读一次.

    这条不是洁癖：`min_notional_buffer` / `close_on_halt` / `require_agreement`
    就是这样在设置页躺了几个月，而引擎从来没读过它们。
    """
    import re
    from pathlib import Path

    root = Path(__file__).resolve().parents[1] / "nanoquant"
    src = (root / "services" / "settings.py").read_text(encoding="utf-8")

    classes: dict[str, list[str]] = {}
    cur: str | None = None
    for line in src.splitlines():
        head = re.match(r"class (\w+)\(BaseModel\):", line)
        if head:
            cur = head.group(1)
            classes[cur] = []
            continue
        if cur and line.startswith("    ") and not line.startswith("        "):
            field = re.match(r"    (\w+):\s*[\w\[\]|. ]+=", line)
            if field:
                classes[cur].append(field.group(1))
        elif line and not line.startswith(" "):
            cur = None

    assert classes, "没有解析到任何设置类，说明解析规则已失效"

    body = "\n".join(
        p.read_text(encoding="utf-8", errors="ignore")
        for p in root.rglob("*.py")
        if p.name != "settings.py" and "__pycache__" not in p.parts
    )

    unread = [
        f"{cls}.{name}"
        for cls, names in classes.items()
        for name in names
        if re.search(rf"\b{re.escape(name)}\b", body) is None
        and name not in FRONTEND_ONLY
    ]

    assert not unread, (
        "这些设置项在界面上可调、后端从不读取："
        f"{sorted(unread)}；要么接上，要么删掉，要么登记进 FRONTEND_ONLY 并写清原因"
    )
