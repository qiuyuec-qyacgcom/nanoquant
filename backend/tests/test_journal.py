"""决策日志 → 打标 → 经验库 闭环的单元测试."""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

from nanoquant.db import session_scope
from nanoquant.exchange import client as binance_client
from nanoquant.journal.experience import (
    archive_labeled,
    derive_scenario,
    derive_tags,
    experience_stats,
    injection,
    retrieve_top_k,
)
from nanoquant.journal.label import label_due, outcome_of, signed_actual_pct
from nanoquant.journal.record import journal_stats, record_decision
from nanoquant.models import DecisionJournal, Experience
from nanoquant.services.settings import get_engine, update_group


def _clear() -> None:
    with session_scope() as s:
        s.query(Experience).delete()
        s.query(DecisionJournal).delete()


def _add_labeled(
    outcome: str, symbol: str = "BTCUSDT", actual_pct: float = -2.0, direction: str = "long"
) -> None:
    now = int(time.time())
    with session_scope() as s:
        s.add(
            DecisionJournal(
                ts=now,
                symbol=symbol,
                direction=direction,
                confidence=0.7,
                entry_price=100.0,
                target_pct=3.0,
                reason="RSI 超卖做多",
                snapshot={"rsi": 20.0, "adx": 15.0, "atr_pct": 1.5},
                decision="open_long",
                status="labeled",
                outcome=outcome,
                actual_price=98.0,
                actual_pct=actual_pct,
                labeled_at=now,
            )
        )


def test_signed_actual_pct_aligns_direction():
    assert signed_actual_pct("long", 100.0, 103.0) == 3.0
    assert signed_actual_pct("short", 100.0, 97.0) == 3.0  # 做空跌 3% = 盈利 3%
    assert signed_actual_pct("short", 100.0, 103.0) == -3.0
    assert signed_actual_pct("long", 0.0, 103.0) == 0.0


def test_outcome_of_threshold():
    assert outcome_of(3.0, 0.15) == "correct"
    assert outcome_of(-3.0, 0.15) == "wrong"
    assert outcome_of(0.05, 0.15) == "partial"


def test_record_decision_gates_and_direction():
    _clear()
    jid = record_decision(
        symbol="BTCUSDT", action="open_long", confidence=0.8, entry_price=100.0,
        target_pct=3.0, reason="RSI 超卖", snapshot={"rsi": 20.0, "adx": 30.0}, mode="demo",
    )
    assert jid
    # 低于记录门槛 / hold 都不记
    assert record_decision(
        symbol="ETHUSDT", action="open_long", confidence=0.1, entry_price=10.0,
        target_pct=1.0, reason="", snapshot={}, mode="demo",
    ) is None
    assert record_decision(
        symbol="ETHUSDT", action="hold", confidence=0.8, entry_price=10.0,
        target_pct=0.0, reason="", snapshot={}, mode="demo",
    ) is None
    assert journal_stats()["total"] == 1


def test_derive_tags_and_scenario():
    tags = derive_tags("BTCUSDT", {"rsi": 20.0, "adx": 30.0}, "long")
    assert "sym:BTCUSDT" in tags and "rsi_oversold" in tags and "adx_trend" in tags and "dir:long" in tags
    sc = derive_scenario({"rsi": 20, "adx": 30, "atr_pct": 1.5, "funding": 0.0001, "junk": "x"})
    assert sc == {"rsi": 20.0, "adx": 30.0, "atr_pct": 1.5, "funding": 0.0001}


def test_archive_and_retrieve_roundtrip():
    _clear()
    _add_labeled("wrong")
    res = archive_labeled()
    assert res["archived"] == 1
    assert experience_stats()["total"] == 1
    items = retrieve_top_k(
        "BTCUSDT", {"rsi": 22.0, "adx": 16.0}, derive_tags("BTCUSDT", {"rsi": 22.0, "adx": 16.0})
    )
    assert items and items[0]["symbol"] == "BTCUSDT"
    assert items[0]["outcome"] == "loss"


def test_partial_outcome_is_not_archived():
    _clear()
    _add_labeled("partial", actual_pct=0.1)
    res = archive_labeled()
    assert res["archived"] == 0
    assert experience_stats()["total"] == 0


def test_same_scenario_merges_instead_of_duplicating():
    _clear()
    _add_labeled("wrong")
    _add_labeled("wrong")
    res = archive_labeled()
    # 同 symbol + 同 outcome + 同标签 → 第二条合并进第一条
    assert res["archived"] == 1
    assert res["merged"] == 1
    assert experience_stats()["total"] == 1


def test_injection_returns_text_and_ids():
    _clear()
    _add_labeled("wrong")
    archive_labeled()
    text, ids, detail = injection("BTCUSDT", {"rsi": 20.0, "adx": 15.0})
    assert text and "◈" in text and "Δ" in text
    assert ids and detail


def test_lesson_is_compact_symbol_form():
    """经验正文已符号化压缩：固定装饰文字用特殊符号代替，变量值保留."""
    _clear()
    _add_labeled("wrong")
    archive_labeled()
    with session_scope() as s:
        exp = s.query(Experience).first()
    assert exp is not None
    assert exp.lesson.startswith("BTCUSDT|")
    assert "L✗" in exp.lesson          # 做多 + 判断错误
    assert "Δ-2.00%" in exp.lesson     # 实际方向收益
    assert "→+3.00%" in exp.lesson     # 目标收益
    assert "c0.70" in exp.lesson       # 信心


# ------------------------------------------------------------- 接线级别的回归
# 事故：经验库长期为 0，被误判成"模块没接进链路"。实测链路是通的（record→打标→
# 归档→注入→prompt），真正的问题是①AI 从未被调用②打标口径有 bug。这里锁住口径。

def test_label_only_labels_current_environment(monkeypatch):
    """demo 的决策不能用 live 的价格打标：两个环境价格差几个点，会把经验库污染成噪声."""
    _clear()
    now = int(time.time())
    with session_scope() as s:
        for mode in ("live", "demo"):
            s.add(
                DecisionJournal(
                    ts=now - 2 * 86400, symbol="BTCUSDT", direction="long", confidence=0.7,
                    entry_price=100.0, target_pct=3.0, snapshot={}, decision="open_long",
                    status="pending", mode=mode,
                )
            )

    async def _tickers(symbols):
        return {sym: SimpleNamespace(last=103.0) for sym in symbols}

    monkeypatch.setattr(binance_client.client, "tickers", _tickers)
    prev_mode = get_engine().mode
    update_group("engine", {"mode": "live"})
    try:
        res = asyncio.run(label_due())
    finally:
        update_group("engine", {"mode": prev_mode})

    assert res["labeled"] == 1
    assert res["deferred"] == 1, "另一环境的到期决策应保持 pending，而不是被错标"
    with session_scope() as s:
        status = {r.mode: r.status for r in s.query(DecisionJournal).all()}
    assert status["live"] == "labeled"
    assert status["demo"] == "pending"


def test_injection_prefers_same_direction_experience():
    """同 symbol 同场景、方向相反的两条经验：查询方向决定谁是 Top-1."""
    _clear()
    _add_labeled("correct", direction="long", actual_pct=3.0)
    _add_labeled("wrong", direction="short", actual_pct=3.0)
    archive_labeled()
    snap = {"rsi": 20.0, "adx": 15.0, "atr_pct": 1.5}

    long_first, _, long_detail = injection("BTCUSDT", snap, "long")
    assert long_detail[0]["tags"], "检索结果应带标签"
    assert "L✓" in long_detail[0]["lesson"], f"做多查询应优先命中做多经验: {long_detail[0]['lesson']}"

    short_first, _, short_detail = injection("BTCUSDT", snap, "short")
    assert "S✗" in short_detail[0]["lesson"], f"做空查询应优先命中做空经验: {short_detail[0]['lesson']}"


def test_injection_without_direction_still_works():
    """量化无方向（候选模式）时也要能注入，不能因为缺方向就整体失效."""
    _clear()
    _add_labeled("wrong")
    archive_labeled()
    text, ids, detail = injection("BTCUSDT", {"rsi": 20.0, "adx": 15.0})
    assert text and ids and detail
