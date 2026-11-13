"""紧急全平：一个动作停引擎 + 撤挂单 + 平掉全部仓位.

事故处置里最怕"要平 8 个仓位就得点 8 次"。这里锁住顺序与覆盖范围：
先停引擎（不再开新仓）→ 撤挂单 → 逐仓市价平；交易所侧未落库的仓位也要被平掉。
"""
from __future__ import annotations

import asyncio
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.engine import worker as worker_mod
from nanoquant.models import PositionRecord
from nanoquant.services import state
from nanoquant.services.settings import get_engine, update_group


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


def _seed(symbol: str, side: str = "long", qty: float = 2.0, entry: float = 100.0) -> int:
    with session_scope() as s:
        s.query(PositionRecord).filter(PositionRecord.symbol == symbol).delete()
        s.add(
            PositionRecord(
                symbol=symbol, side=side, qty=qty, entry_price=entry, mark_price=entry,
                leverage=4.0, margin=qty * entry / 4.0, opened_at=0, mode="demo", status="open",
                ai_reason="test",
            )
        )
    with session_scope() as s:
        row = (
            s.query(PositionRecord)
            .filter(PositionRecord.symbol == symbol, PositionRecord.status == "open")
            .first()
        )
        return int(row.id)


def _cleanup(symbols: list[str], enabled: bool) -> None:
    update_group("engine", {"enabled": enabled})
    state.set("enabled", str(enabled).lower())
    with session_scope() as s:
        for symbol in symbols:
            s.query(PositionRecord).filter(PositionRecord.symbol == symbol).delete()


def test_flatten_stops_engine_cancels_orders_and_closes(monkeypatch):
    symbol = "FLATUSDT"
    pid = _seed(symbol)
    calls: dict[str, list] = {"cancel": [], "closed": []}

    class _Broker:
        async def positions(self):
            return []

        async def close_position(self, *, position, reason, qty=None, **_):
            calls["closed"].append((position.symbol, reason))
            with session_scope() as s:
                row = s.get(PositionRecord, position.id)
                if row is not None:
                    row.status = "closed"
                    row.exit_price = 99.0
            return SimpleNamespace(ok=True, message="")

    async def _cancel_all(sym):
        calls["cancel"].append(("all", sym))
        return {}

    async def _cancel_algo(sym=None):
        calls["cancel"].append(("algo", sym))
        return 0

    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(worker_mod.binance_client, "cancel_all", _cancel_all)
    monkeypatch.setattr(worker_mod.binance_client, "cancel_all_algo", _cancel_algo)

    old_enabled = get_engine().enabled
    try:
        out = asyncio.run(worker_mod.engine.flatten_all(reason="测试全平"))
        assert out["ok"] is True
        assert out["engine_stopped"] is True
        assert [c["symbol"] for c in out["closed"]] == [symbol]
        # 挂单与条件单都要撤：只撤一种会留下会触发的老保护单
        assert {kind for kind, _ in calls["cancel"]} == {"all", "algo"}
        assert get_engine().enabled is False
        with session_scope() as s:
            row = s.get(PositionRecord, pid)
            assert row.status == "closed"
    finally:
        _cleanup([symbol], old_enabled)


def test_flatten_adopts_venue_positions_unknown_to_db(monkeypatch):
    symbol = "FLATVUSDT"
    calls: list[str] = []

    class _Broker:
        async def positions(self):
            return [
                SimpleNamespace(
                    symbol=symbol, side="long", qty=1.0, entry_price=50.0,
                    mark_price=50.0, leverage=3.0, unrealized=0.0,
                )
            ]

        async def close_position(self, *, position, reason, qty=None, **_):
            calls.append(position.symbol)
            with session_scope() as s:
                row = s.get(PositionRecord, position.id)
                if row is not None:
                    row.status = "closed"
            return SimpleNamespace(ok=True, message="")

    async def _noop(*args, **kwargs):
        return 0

    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(worker_mod.binance_client, "cancel_all", _noop)
    monkeypatch.setattr(worker_mod.binance_client, "cancel_all_algo", _noop)

    old_enabled = get_engine().enabled
    try:
        out = asyncio.run(worker_mod.engine.flatten_all(reason="测试全平", stop_engine=False))
        assert out["ok"] is True
        assert calls == [symbol], "交易所侧未落库的仓位也必须被平掉"
        assert out["engine_stopped"] is False
    finally:
        _cleanup([symbol], old_enabled)


def test_flatten_reports_partial_failure(monkeypatch):
    symbol = "FLATFAILUSDT"
    _seed(symbol)

    class _Broker:
        async def positions(self):
            return []

        async def close_position(self, *, position, reason, qty=None, **_):
            return SimpleNamespace(ok=False, message="交易所拒绝")

    async def _noop(*args, **kwargs):
        return 0

    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _Broker())
    monkeypatch.setattr(worker_mod.binance_client, "cancel_all", _noop)
    monkeypatch.setattr(worker_mod.binance_client, "cancel_all_algo", _noop)

    old_enabled = get_engine().enabled
    try:
        out = asyncio.run(worker_mod.engine.flatten_all(reason="测试全平", stop_engine=False))
        assert out["ok"] is False
        assert out["failed"] and out["failed"][0]["symbol"] == symbol
        assert out["failed"][0]["error"] == "交易所拒绝"
    finally:
        _cleanup([symbol], old_enabled)
