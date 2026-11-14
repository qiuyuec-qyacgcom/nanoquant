"""外部信号跟单的处理链（worker._follow_signal_locked 及其生命周期分支）.

这是系统唯一"由外部文本决定开仓"的入口，也是重复开仓/漏平仓最容易发生的地方：
同一条消息被文件轮询和 HTTP 推送各推一次、重启后补开旧信号、开仓后 90s 内收到平仓……
每一条都是真金白银。以前这条链路只有 0 覆盖。
"""
from __future__ import annotations

import asyncio
import json
import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.engine import worker as worker_mod
from nanoquant.exchange.broker import ExecResult
from nanoquant.exchange.precision import SymbolFilters
from nanoquant.models import PositionRecord, SignalFollow
from nanoquant.services.settings import get_engine, get_signal_follow, update_group

SYMBOL = "FOLLOWUSDT"
MODE = "demo"


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


@pytest.fixture(autouse=True)
def _clean():
    from nanoquant.services import state

    prev_follow = get_signal_follow()
    prev_engine = get_engine()
    state.set_many({"halted": "", "day_anchor": "", "day_start_equity": "", "equity_peak": ""})
    with session_scope() as s:
        s.query(SignalFollow).delete(synchronize_session=False)
        s.query(PositionRecord).filter(PositionRecord.symbol == SYMBOL).delete(synchronize_session=False)
    yield
    update_group("signal_follow", prev_follow.model_dump())
    update_group("engine", {"enabled": prev_engine.enabled})
    with session_scope() as s:
        s.query(SignalFollow).delete(synchronize_session=False)
        s.query(PositionRecord).filter(PositionRecord.symbol == SYMBOL).delete(synchronize_session=False)


def _parsed(**over) -> dict:
    base = {
        "symbol": SYMBOL,
        "side": "long",
        "event": "open",
        "ts": int(time.time()),
        "leverage": 5.0,
        "entry": 100.0,
        "stop": 98.0,
        "tp": 104.0,
        "raw": {"text": "开多"},
        "signal_id": "",
    }
    base.update(over)
    return base


def _filters() -> SymbolFilters:
    return SymbolFilters(
        symbol=SYMBOL, price_tick=0.01, qty_step=0.001, min_qty=0.001,
        min_notional=5.0, last_price=100.0, quote_volume_24h=5e7,
    )


class _Broker:
    mode = MODE

    def __init__(self, ok: bool = True, explode: bool = False) -> None:
        self.ok = ok
        self.explode = explode
        self.orders: list[dict] = []
        self.closes: list[dict] = []

    async def account(self):
        return SimpleNamespace(
            equity=100.0, wallet=100.0, available=100.0, unrealized=0.0, margin_used=0.0,
            as_dict=lambda: {"equity": 100.0},
        )

    async def open_position(self, **kw):
        if self.explode:
            raise worker_mod.BinanceError(-2019, "Margin is insufficient")
        self.orders.append(kw)
        if not self.ok:
            return ExecResult(False, message="-1013 最小名义额")
        return ExecResult(True, price=kw["filters"].last_price, qty=kw["qty"])

    async def close_position(self, *, position, reason, qty=None):
        self.closes.append({"symbol": position.symbol, "reason": reason, "qty": qty})
        return ExecResult(True, price=position.mark_price, qty=position.qty)


def _patch_market(monkeypatch, price: float = 100.0, in_pool: bool = True, enabled: bool = True):
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda s: _filters())
    monkeypatch.setattr(worker_mod.symbol_service, "known_symbols", lambda: [SYMBOL] if in_pool else [])
    monkeypatch.setattr(
        worker_mod.symbol_service, "ordered_symbols",
        lambda enabled_only=True: [SYMBOL] if enabled else [],
    )
    monkeypatch.setattr(worker_mod.risk_mod, "trading_window_open", lambda cfg: (True, ""))
    monkeypatch.setattr(
        worker_mod.risk_mod, "global_gates", lambda a, e, r: worker_mod.risk_mod.GateDecision(True, "")
    )
    monkeypatch.setattr(
        worker_mod.risk_mod, "symbol_gates",
        lambda *a, **k: worker_mod.risk_mod.GateDecision(True, ""),
    )

    async def _tickers(_symbols):
        return {SYMBOL: SimpleNamespace(last=price, quote_volume=5e7)}

    monkeypatch.setattr(worker_mod.binance_client, "tickers", _tickers)


def _enable(**over):
    patch = {
        "enabled": True, "dry_run": False, "max_age_sec": 180, "max_per_hour": 6,
        "auto_add_symbol": False, "sizing_mode": "margin_pct", "leverage_bonus": 10,
    }
    patch.update(over)
    update_group("signal_follow", patch)
    update_group("engine", {"enabled": True})


def _run(eng, parsed):
    return asyncio.run(eng._follow_signal(parsed))


def _follow_row(key: str) -> SignalFollow | None:
    with session_scope() as s:
        return s.query(SignalFollow).filter(SignalFollow.signal_id == key).first()


# ------------------------------------------------------------ 前置闸门
def test_disabled_switch_skips_everything(monkeypatch):
    update_group("signal_follow", {"enabled": False})
    eng = worker_mod.TradingEngine()

    out = asyncio.run(eng._follow_signal(_parsed()))

    assert out["status"] == "skipped" and "未开启" in out["reason"]


def test_duplicate_signal_is_claimed_once(monkeypatch):
    """文件轮询与 HTTP 推送同时到达时，只允许一条进下单链路."""
    _enable()
    _patch_market(monkeypatch)
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _Broker())
    eng = worker_mod.TradingEngine()
    parsed = _parsed(entry=101.0, ts=int(time.time()))

    first = _run(eng, parsed)
    second = _run(eng, parsed)

    assert first["status"] == "opened"
    assert second["status"] == "duplicate"


def test_exit_event_is_not_followed(monkeypatch):
    _enable()
    _patch_market(monkeypatch)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="exit"))

    assert out["status"] == "skipped" and "非开仓事件" in out["reason"]


def test_stale_signal_is_rejected(monkeypatch):
    _enable(max_age_sec=60)
    _patch_market(monkeypatch)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(ts=int(time.time()) - 600))

    assert out["status"] == "skipped" and "过期" in out["reason"]


def test_hourly_cap_blocks_further_follows(monkeypatch):
    _enable(max_per_hour=1)
    _patch_market(monkeypatch)
    with session_scope() as s:
        s.add(SignalFollow(signal_id="cap-x", symbol=SYMBOL, side="long", status="opened", mode=MODE))
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed())

    assert out["status"] == "skipped" and "上限" in out["reason"]


def test_engine_stopped_blocks_follow(monkeypatch):
    _enable()
    update_group("engine", {"enabled": False})
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed())

    assert out["status"] == "skipped" and "未启动" in out["reason"]


def test_symbol_outside_pool_is_skipped_without_auto_add(monkeypatch):
    _enable(auto_add_symbol=False)
    _patch_market(monkeypatch, in_pool=False)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed())

    assert out["status"] == "skipped" and "不在池内" in out["reason"]


def test_disabled_symbol_in_pool_is_not_auto_enabled(monkeypatch):
    """池里有但被停用，说明是人工停的；跟单不该擅自打开."""
    _enable(auto_add_symbol=True)
    _patch_market(monkeypatch, enabled=False)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed())

    assert out["status"] == "skipped" and "已停用" in out["reason"]


# ------------------------------------------------------------ 执行
def test_dry_run_records_without_ordering(monkeypatch):
    _enable(dry_run=True)
    _patch_market(monkeypatch)
    broker = _Broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()
    parsed = _parsed()

    out = _run(eng, parsed)

    assert out["status"] == "dry_run"
    assert not broker.orders, "演练模式绝不能下单"
    row = _follow_row(worker_mod.signal_feed.signal_identity(parsed))
    assert row is not None and row.status == "dry_run"
    assert row.qty > 0 and row.leverage >= 1


def test_successful_follow_opens_with_signal_stop(monkeypatch):
    _enable()
    _patch_market(monkeypatch)
    broker = _Broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(entry=100.0, stop=98.0, tp=104.0))

    assert out["status"] == "opened"
    assert len(broker.orders) == 1
    call = broker.orders[0]
    assert call["side"] == "long"
    # 信号给了止损止盈就照用，不能自己另算一套
    assert call["sl_price"] == pytest.approx(98.0)
    assert call["tp_price"] == pytest.approx(104.0)
    assert call["leverage"] >= 11, "跟单杠杆 = 信号 5x + 加成 10x，且不低于风控下限"
    # 成交后同标的忙碌标记要保留，避免 90s 内重复开同一标的
    assert eng._symbol_acquire(SYMBOL) is False


def test_exchange_rejection_releases_reserved_margin(monkeypatch):
    """下单被拒必须把预留额度还回去，否则后面几笔全被"占用"卡死."""
    _enable()
    _patch_market(monkeypatch)
    broker = _Broker(ok=False)
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed())

    assert out["status"] == "error" and "下单失败" in out["reason"]
    assert eng._margin.inflight == pytest.approx(0.0)
    assert eng._margin.remaining == pytest.approx(100.0)


def test_exchange_exception_is_recorded_not_raised(monkeypatch):
    _enable()
    _patch_market(monkeypatch)
    broker = _Broker(explode=True)
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed())

    assert out["status"] == "error" and "下单异常" in out["reason"]
    assert eng._margin.inflight == pytest.approx(0.0)


# ------------------------------------------------------------ 生命周期
def _open_local(side: str = "long", qty: float = 1.0, entry: float = 100.0) -> int:
    with session_scope() as s:
        row = PositionRecord(
            symbol=SYMBOL, side=side, qty=qty, entry_price=entry, mark_price=entry,
            leverage=10.0, margin=qty * entry / 10.0, status="open", mode=MODE,
            sl_price=98.0, tp_price=104.0, initial_sl_price=98.0, peak_price=entry,
            opened_at=int(time.time()) - 30, features={},
        )
        s.add(row)
        s.flush()
        return row.id


def test_lifecycle_without_local_position_is_skipped(monkeypatch):
    _enable()
    _patch_market(monkeypatch)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="close"))

    assert out["status"] == "skipped" and "本地无" in out["reason"]


def test_lifecycle_close_in_dry_run_does_not_touch_broker(monkeypatch):
    _enable(dry_run=True)
    _patch_market(monkeypatch)
    _open_local()
    broker = _Broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="close"))

    assert out["status"] == "dry_run"
    assert broker.closes == []


def test_lifecycle_close_actually_closes_position(monkeypatch):
    _enable()
    _patch_market(monkeypatch)
    pid = _open_local()
    broker = _Broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="close"))

    assert out["status"] in ("opened", "ok", "closed")
    assert broker.closes and broker.closes[0]["reason"].startswith("signal")
    with session_scope() as s:
        assert s.get(PositionRecord, pid) is not None


# ------------------------------------------------------------ 文件游标（不回放历史）
def test_jsonl_file_is_tailed_from_the_end(monkeypatch, tmp_path):
    """首次接入只从文件**尾部**开始：重启补读历史会把半小时前的信号当新单开出去."""
    path = tmp_path / "signals.jsonl"
    path.write_text('{"symbol": "OLDUSDT"}\n', encoding="utf-8")
    _enable(jsonl_path=str(path))
    handles: list[dict] = []
    eng = worker_mod.TradingEngine()

    async def _handle(row):
        handles.append(row)
        return {"ok": True}

    eng._handle_signal_row = _handle

    first = asyncio.run(eng._poll_signal_feed_once())
    assert first == 0 and handles == [], "接入前的历史行不能被当成本轮信号"

    with path.open("a", encoding="utf-8") as fh:
        fh.write('{"symbol": "NEWUSDT"}\n')
    second = asyncio.run(eng._poll_signal_feed_once())
    assert second == 1 and len(handles) == 1
    assert json.dumps(handles[0]).find("NEWUSDT") >= 0

    # 没有新内容时不再重复处理
    third = asyncio.run(eng._poll_signal_feed_once())
    assert third == 0 and len(handles) == 1


def test_poll_clears_error_when_disabled():
    update_group("signal_follow", {"enabled": False})
    eng = worker_mod.TradingEngine()
    eng._follow_last_error = "上次的错误"

    assert asyncio.run(eng._poll_signal_feed_once()) == 0
    assert eng._follow_last_error == ""


def test_poll_reports_missing_file(monkeypatch, tmp_path):
    missing = tmp_path / "nope" / "signals.jsonl"
    _enable(jsonl_path=str(missing))
    eng = worker_mod.TradingEngine()

    assert asyncio.run(eng._poll_signal_feed_once()) == 0
    assert "等待信号文件" in eng._follow_last_error


# ------------------------------------------------------------ 生命周期：减仓 / 移止损 / 加仓
def _lifecycle_broker(**over):
    broker = _Broker(**over)
    broker.amends = []

    async def _amend(symbol, side, stop, qty, tp_price=0.0):
        broker.amends.append({"symbol": symbol, "side": side, "stop": stop, "qty": qty, "tp": tp_price})
        return ExecResult(True, price=stop)

    async def _add(*, position, qty, filters, sl_price, tp_price, reason=""):
        broker.adds = getattr(broker, "adds", []) + [
            {"symbol": position.symbol, "qty": qty, "sl": sl_price, "tp": tp_price}
        ]
        return ExecResult(True, price=filters.last_price, qty=qty)

    broker.amend_stop = _amend
    broker.add_to_position = _add
    return broker


def test_lifecycle_reduce_uses_signal_pct(monkeypatch):
    _enable()
    _patch_market(monkeypatch)
    _open_local(qty=2.0)
    broker = _lifecycle_broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="reduce", reduce_pct=25.0))

    assert out["status"] == "reduced"
    assert broker.closes[0]["qty"] == pytest.approx(0.5)
    assert broker.closes[0]["reason"].startswith("signal_reduce")


def test_lifecycle_reduce_defaults_to_half(monkeypatch):
    """频道只说"减仓"没给比例：按一半处理（比全平保守，比不动有意义）."""
    _enable()
    _patch_market(monkeypatch)
    _open_local(qty=2.0)
    broker = _lifecycle_broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="reduce"))

    assert out["status"] == "reduced"
    assert broker.closes[0]["qty"] == pytest.approx(1.0)


def test_lifecycle_reduce_in_dry_run_does_not_touch_broker(monkeypatch):
    _enable(dry_run=True)
    _patch_market(monkeypatch)
    _open_local(qty=2.0)
    broker = _lifecycle_broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="reduce", reduce_pct=50.0))

    assert out["status"] == "dry_run"
    assert broker.closes == []


def test_lifecycle_move_stop_to_breakeven_keeps_take_profit(monkeypatch):
    _enable()
    _patch_market(monkeypatch, price=102.0)
    _open_local(entry=100.0)
    broker = _lifecycle_broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="move_stop", stop_to_breakeven=True))

    assert out["status"] == "stop_moved"
    call = broker.amends[0]
    assert call["stop"] == pytest.approx(100.0)
    assert call["tp"] == pytest.approx(104.0), "移止损必须把止盈一起重挂"


def test_lifecycle_move_stop_on_wrong_side_is_rejected(monkeypatch):
    """多单把止损设到现价之上，挂上去立刻市价平仓 —— 必须拒绝."""
    _enable()
    _patch_market(monkeypatch, price=100.0)
    _open_local(entry=100.0)
    broker = _lifecycle_broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="move_stop", stop=101.0))

    assert out["status"] == "skipped" and "立即触发" in out["reason"]
    assert broker.amends == []


def test_lifecycle_move_stop_without_target_is_skipped(monkeypatch):
    _enable()
    _patch_market(monkeypatch)
    _open_local()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: _lifecycle_broker())
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="move_stop", stop=0.0))

    assert out["status"] == "skipped" and "目标价" in out["reason"]


def test_lifecycle_add_uses_existing_stop_when_signal_omits_it(monkeypatch):
    _enable()
    _patch_market(monkeypatch, price=101.0)
    _open_local(entry=100.0)
    broker = _lifecycle_broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="add", stop=0.0, tp=0.0))

    assert out["status"] == "added"
    call = broker.adds[0]
    assert call["sl"] == pytest.approx(98.0), "没给新止损就沿用原仓位的，绝不留裸仓"
    assert call["tp"] == pytest.approx(104.0)


def test_lifecycle_add_opposite_side_is_ignored(monkeypatch):
    _enable()
    _patch_market(monkeypatch)
    _open_local(side="long")
    broker = _lifecycle_broker()
    monkeypatch.setattr(worker_mod, "get_broker", lambda mode: broker)
    eng = worker_mod.TradingEngine()

    out = _run(eng, _parsed(event="add", side="short"))

    assert out["status"] == "skipped" and "相反" in out["reason"]
    assert getattr(broker, "adds", []) == []
