"""运维服务：历史数据保留、开仓准入体检、运行时抖动探针.

这三块以前是 0% 覆盖：保留策略删错数据会"删了查不到"，准入体检是新手唯一的
自助排查入口，探针则决定 p99 抖动有没有数据。
"""
from __future__ import annotations

import asyncio
import gzip
import json
import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.models import EngineEvent, Signal, TradingSymbol
from nanoquant.services import admission, retention, runtime_probe
from nanoquant.services.settings import get_engine, update_group


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


# --------------------------------------------------------------- retention
def test_purge_expired_archives_before_delete():
    now = int(time.time())
    old = now - 400 * 86400  # 远超默认保留天数
    with session_scope() as s:
        s.add(Signal(symbol="RETTEST", ts=old, direction="long", score=0.5, decision="open", mode="demo"))
        s.add(EngineEvent(ts=old, level="info", category="retention-test", message="待归档"))

    stats = retention.purge_expired(now=now)

    assert stats["signals"] >= 1
    assert stats["events"] >= 1
    with session_scope() as s:
        assert s.query(Signal).filter(Signal.symbol == "RETTEST").count() == 0

    archives = sorted(retention.ARCHIVE_DIR.glob("signals-*.jsonl.gz"))
    assert archives, "删除前必须先落归档，否则历史查不回来"
    with gzip.open(archives[-1], "rt", encoding="utf-8") as fh:
        rows = [json.loads(line) for line in fh if line.strip()]
    assert any(r.get("symbol") == "RETTEST" for r in rows)


# --------------------------------------------------------------- runtime_probe
def test_probe_summary_percentiles_and_blocked_counts():
    runtime_probe._LAG.clear()  # noqa: SLF001 - 测试直接喂样本
    runtime_probe._GC.clear()  # noqa: SLF001
    runtime_probe._LAG.extend([1.0, 2.0, 3.0, 100.0])  # noqa: SLF001

    out = runtime_probe.summary()

    assert out["samples"] == 4
    assert out["loop_lag_ms"]["max"] == 100.0
    assert out["blocked_over_50ms"] == 1
    assert out["blocked_over_20ms"] == 1


def test_probe_start_stop_is_idempotent():
    async def run() -> None:
        runtime_probe.start()
        runtime_probe.start()  # 第二次不应再起一个任务
        await asyncio.sleep(0.12)
        runtime_probe.stop()
        runtime_probe.stop()  # 重复 stop 不应抛

    asyncio.run(run())
    assert runtime_probe.summary()["samples"] >= 1


# --------------------------------------------------------------- admission
def test_admission_blocks_when_credentials_missing(monkeypatch):
    class _Broker:
        async def account(self):
            return SimpleNamespace(equity=0.0, available=0.0)

    monkeypatch.setattr(admission, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(admission, "decrypt_secret", lambda cipher: "")

    out = asyncio.run(admission.evaluate())

    assert out["ok"] is False
    assert "channel" in out["blocking"]
    assert out["can_open"] is False
    assert any(step["key"] == "symbols" for step in out["steps"])


def test_admission_reports_symbol_level_tradeability(monkeypatch):
    symbol = "ADMTESTUSDT"
    old_enabled = get_engine().enabled
    update_group("engine", {"enabled": True})
    with session_scope() as s:
        if s.get(TradingSymbol, symbol) is None:
            s.add(
                TradingSymbol(
                    symbol=symbol, enabled=True, max_leverage=50, precision=3,
                    tick_sz="0.01", min_sz="0.001", min_notional=5.0, sort_order=900,
                )
            )

    class _Broker:
        async def account(self):
            return SimpleNamespace(equity=100.0, available=100.0)

    class _Filters:
        max_leverage = 50

        def effective_min_notional(self, price: float) -> float:
            return 5.0

        def effective_min_qty(self, price: float) -> float:
            return 0.001

    async def _tickers(symbols):
        return {s: SimpleNamespace(last=10.0, change_pct=0.0, quote_volume=1e8) for s in symbols}

    monkeypatch.setattr(admission, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(admission, "decrypt_secret", lambda cipher: "configured")
    monkeypatch.setattr(admission.binance_client, "tickers", _tickers)
    monkeypatch.setattr(admission.symbol_service, "filters_for", lambda s: _Filters())
    monkeypatch.setattr(admission, "estimate_qty", lambda *a, **k: (1.0, ""))

    try:
        out = asyncio.run(admission.evaluate())
    finally:
        update_group("engine", {"enabled": old_enabled})
        with session_scope() as s:
            row = s.get(TradingSymbol, symbol)
            if row is not None:
                s.delete(row)

    row = next(item for item in out["symbols"] if item["symbol"] == symbol)
    assert row["tradeable"] is True
    assert row["max_leverage"] == 50
    # 体检门槛与引擎下单门槛一致：交易所下限 5 U × 默认 1.05 缓冲
    from nanoquant.services.settings import get_risk

    assert row["min_notional"] == pytest.approx(5.0 * get_risk().min_notional_buffer)
    assert out["tradeable_count"] >= 1


def test_admission_survives_symbol_without_trading_rules(monkeypatch):
    """池里有已下架/拿不到规则的标的时，体检不能 500（曾经 KeyError: min_notional）。"""
    symbol = "ADMNORULEUSDT"
    with session_scope() as s:
        if s.get(TradingSymbol, symbol) is None:
            s.add(
                TradingSymbol(
                    symbol=symbol, enabled=True, max_leverage=50, precision=3,
                    tick_sz="0.01", min_sz="0.001", min_notional=5.0, sort_order=901,
                )
            )

    class _Broker:
        async def account(self):
            return SimpleNamespace(equity=100.0, available=100.0)

    async def _tickers(symbols):
        return {}  # 行情也拿不到

    monkeypatch.setattr(admission, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(admission, "decrypt_secret", lambda cipher: "configured")
    monkeypatch.setattr(admission.binance_client, "tickers", _tickers)
    monkeypatch.setattr(admission.symbol_service, "filters_for", lambda s: None)

    try:
        out = asyncio.run(admission.evaluate())
    finally:
        with session_scope() as s:
            row = s.get(TradingSymbol, symbol)
            if row is not None:
                s.delete(row)

    item = next(x for x in out["symbols"] if x["symbol"] == symbol)
    assert item["tradeable"] is False
    assert item["reason"] == "缺少交易规则或行情"
    assert item["min_notional"] is None


def _patch_admission_cfg(monkeypatch, *, risk, engine=None):
    """admission.evaluate() 直接读全局配置，测试用 monkeypatch 固定口径。"""
    monkeypatch.setattr(
        admission, "get_risk",
        lambda: SimpleNamespace(
            margin_pct=risk["margin_pct"], default_leverage=risk["default_leverage"],
            max_leverage=risk["max_leverage"], risk_per_trade_pct=risk["risk_per_trade_pct"],
            min_equity_usdt=0.0, min_notional_buffer=1.05, consecutive_loss_limit=5,
        ),
    )
    monkeypatch.setattr(
        admission, "get_engine",
        lambda: engine or SimpleNamespace(mode="demo", enabled=True, max_positions=3, trading_windows=[]),
    )
    monkeypatch.setattr(
        admission, "get_data",
        lambda: SimpleNamespace(
            demo_api_key_enc="x", demo_api_secret_enc="y",
            live_api_key_enc="", live_api_secret_enc="",
        ),
    )
    monkeypatch.setattr(admission, "get_ai", lambda: SimpleNamespace(enabled=False))
    monkeypatch.setattr(
        admission, "get_strategy",
        lambda: SimpleNamespace(timeframe="5m", entry_threshold=60),
    )


def test_admission_suggests_higher_leverage_when_default_too_small(monkeypatch):
    """复现真实场景: 权益 14.78U、默认 1x、可分配 5.47，ICP 类标的需 7.73 名义额。

    旧逻辑按默认杠杆判一次就输出"不可开仓"；但 20x 下保证金只要 0.39 U，
    真实开仓链路允许把杠杆提到全局上限——体检必须把这个可行路径算出来。
    """
    symbol = "ADMLEVUSDT"
    with session_scope() as s:
        if s.get(TradingSymbol, symbol) is None:
            s.add(
                TradingSymbol(
                    symbol=symbol, enabled=True, max_leverage=20, precision=4,
                    tick_sz="0.0001", min_sz="0.1", min_notional=7.73, sort_order=902,
                )
            )
    _patch_admission_cfg(
        monkeypatch,
        risk=dict(margin_pct=37.0, default_leverage=1, max_leverage=20, risk_per_trade_pct=2.0),
    )

    class _Broker:
        async def account(self):
            return SimpleNamespace(equity=14.78, available=14.78)

    class _Filters:
        max_leverage = 20
        qty_step = 0.1

        def effective_min_notional(self, price: float) -> float:
            return 7.73

        def effective_min_qty(self, price: float) -> float:
            return 4.0

        def validate(self, qty: float, price: float):
            return True, ""

    async def _tickers(symbols):
        return {s: SimpleNamespace(last=2.455, change_pct=0.0, quote_volume=1e8) for s in symbols}

    monkeypatch.setattr(admission, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(admission, "decrypt_secret", lambda cipher: "configured")
    monkeypatch.setattr(admission.binance_client, "tickers", _tickers)
    monkeypatch.setattr(admission.symbol_service, "filters_for", lambda s: _Filters())

    try:
        out = asyncio.run(admission.evaluate())
    finally:
        with session_scope() as s:
            row = s.get(TradingSymbol, symbol)
            if row is not None:
                s.delete(row)
    row = next(x for x in out["symbols"] if x["symbol"] == symbol)
    assert row["tradeable"] is True, row["reason"]
    assert row["leverage_used"] == 1            # 默认杠杆没动
    assert 2 <= row["suggested_leverage"] <= 20  # 给出可达的建议杠杆
    assert row["qty_if_open"] > 0


def test_admission_flags_risk_budget_too_small_when_leverage_cannot_help(monkeypatch):
    """风险预算（与杠杆无关）低于最小名义额时，提示必须指向风险参数而非杠杆。"""
    symbol = "ADMRISKUSDT"
    with session_scope() as s:
        if s.get(TradingSymbol, symbol) is None:
            s.add(
                TradingSymbol(
                    symbol=symbol, enabled=True, max_leverage=20, precision=4,
                    tick_sz="0.0001", min_sz="0.1", min_notional=7.73, sort_order=903,
                )
            )
    _patch_admission_cfg(
        monkeypatch,
        risk=dict(margin_pct=37.0, default_leverage=1, max_leverage=20, risk_per_trade_pct=0.1),
    )

    class _Broker:
        async def account(self):
            return SimpleNamespace(equity=14.78, available=14.78)

    class _Filters:
        max_leverage = 20
        qty_step = 0.1

        def effective_min_notional(self, price: float) -> float:
            return 7.73

        def effective_min_qty(self, price: float) -> float:
            return 4.0

        def validate(self, qty: float, price: float):
            return True, ""

    async def _tickers(symbols):
        return {s: SimpleNamespace(last=2.455, change_pct=0.0, quote_volume=1e8) for s in symbols}

    monkeypatch.setattr(admission, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(admission, "decrypt_secret", lambda cipher: "configured")
    monkeypatch.setattr(admission.binance_client, "tickers", _tickers)
    monkeypatch.setattr(admission.symbol_service, "filters_for", lambda s: _Filters())

    try:
        out = asyncio.run(admission.evaluate())
    finally:
        with session_scope() as s:
            row = s.get(TradingSymbol, symbol)
            if row is not None:
                s.delete(row)
    row = next(x for x in out["symbols"] if x["symbol"] == symbol)
    assert row["tradeable"] is False
    assert "风险预算不足" in row["reason"]
    assert row["suggested_leverage"] == 0
