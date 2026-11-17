"""开仓决策链（worker._do_open）——AI 闸门、点差闸门、仓位规划的拒绝路径.

这条链路以前 0 覆盖，但它每一步都在拒绝或放行真金白银：
点差太宽要放弃、AI 否决要放弃、议会分歧要放弃、仓位不够要放弃。
任何一个分支写反，表现都是"策略看起来在跑，但永远不开仓/乱开仓"。
"""
from __future__ import annotations

import asyncio
import time
from types import SimpleNamespace

import pytest

from nanoquant.ai.decision import AiDecision, DecisionOutcome
from nanoquant.db import init_db, session_scope
from nanoquant.engine import worker as worker_mod
from nanoquant.exchange.broker import ExecResult
from nanoquant.exchange.precision import SymbolFilters
from nanoquant.models import Signal
from nanoquant.services.settings import get_ai, get_engine, get_risk, get_strategy, update_group

SYMBOL = "ENTRYTESTUSDT"


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


def _signal_row() -> int:
    with session_scope() as s:
        row = Signal(symbol=SYMBOL, ts=int(time.time()), direction="long", score=0.9, decision="open", mode="demo")
        s.add(row)
        s.flush()
        return row.id


def _reject_reason(signal_id: int) -> str:
    with session_scope() as s:
        return s.get(Signal, signal_id).reject_reason or ""


def _sig():
    return SimpleNamespace(
        symbol=SYMBOL,
        direction="long",
        score=0.82,
        sl_pct=1.2,
        tp_pct=2.4,
        features={"atr_pct": 0.8},
    )


def _ticker(price: float = 100.0):
    return SimpleNamespace(last=price, quote_volume=5e7, change_pct=1.0)


def _filters() -> SymbolFilters:
    return SymbolFilters(
        symbol=SYMBOL, price_tick=0.01, qty_step=0.001, min_qty=0.001,
        min_notional=5.0, last_price=100.0, quote_volume_24h=5e7,
    )


class _Broker:
    mode = "demo"

    def __init__(self, ok: bool = True, message: str = "") -> None:
        self.ok = ok
        self.message = message
        self.calls: list[dict] = []

    async def open_position(self, **kwargs):
        self.calls.append(kwargs)
        return ExecResult(self.ok, price=kwargs["filters"].last_price, qty=kwargs["qty"], message=self.message)


def _run(eng, broker, *, available: float = 100.0):
    """跑一次 _do_open；先给保证金台账一个预算（真实链路里由 cycle 的 begin_cycle 提供）."""
    eng._margin.begin_cycle(available)
    signal_id = _signal_row()
    try:
        asyncio.run(
            eng._do_open(
                broker, _sig(), _ticker(), SimpleNamespace(equity=100.0, available=100.0, unrealized=0.0),
                get_engine(), get_risk(), get_strategy(), signal_id,
            )
        )
        return signal_id
    finally:
        pass


@pytest.fixture(autouse=True)
def _no_ai():
    """默认关闭 AI，避免用例之间互相影响；退出时恢复原值（update_group 返回的是新值，不能当旧值用）."""
    prev = get_ai().enabled
    update_group("ai", {"enabled": False})
    yield
    update_group("ai", {"enabled": prev})


def test_missing_symbol_filters_is_rejected(monkeypatch):
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: None)
    eng = worker_mod.TradingEngine()
    broker = _Broker()

    sid = _run(eng, broker)

    assert "不在标的池" in _reject_reason(sid)
    assert not broker.calls


def test_wide_spread_is_rejected(monkeypatch):
    """超短线利润只有千分之几，点差 30bps 做一次就白干."""
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    prev_spread = get_strategy().max_spread_bps
    update_group("strategy", {"max_spread_bps": 10.0})

    async def _books(_symbols):
        return {SYMBOL: (99.9, 100.2)}  # 30bps

    monkeypatch.setattr(worker_mod.binance_client, "book_tickers", _books)
    eng = worker_mod.TradingEngine()
    broker = _Broker()
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": prev_spread})

    assert "点差" in _reject_reason(sid)
    assert not broker.calls


def test_ai_hold_vetoes_entry(monkeypatch):
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    prev_spread = get_strategy().max_spread_bps
    update_group("strategy", {"max_spread_bps": 0.0})
    update_group("ai", {"enabled": True})
    monkeypatch.setattr(worker_mod, "providers_available", lambda: 1, raising=False)
    monkeypatch.setattr("nanoquant.ai.client.providers_available", lambda: 1)

    async def _decide(_ctx):
        return DecisionOutcome(
            decision=AiDecision(action="hold", confidence=0.9, reason="结构不清晰"),
            ok=True, agreement=1.0,
        )

    monkeypatch.setattr(worker_mod, "decide", _decide)
    eng = worker_mod.TradingEngine()
    eng._build_ai_context = lambda *a, **k: {"injected_mantra_ids": []}
    broker = _Broker()
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": prev_spread})

    assert "AI 否决" in _reject_reason(sid)
    assert not broker.calls


def test_ai_low_agreement_vetoes_entry(monkeypatch):
    """`ai.require_agreement` 打开后，议会分歧必须真的拦住下单."""
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    prev_spread = get_strategy().max_spread_bps
    update_group("strategy", {"max_spread_bps": 0.0})
    prev_ai = get_ai()
    update_group("ai", {"enabled": True, "require_agreement": True})
    monkeypatch.setattr("nanoquant.ai.client.providers_available", lambda: 1)

    async def _decide(_ctx):
        return DecisionOutcome(
            decision=AiDecision(action="open_long", confidence=0.9, reason="看多"),
            ok=True, agreement=0.45,
        )

    monkeypatch.setattr(worker_mod, "decide", _decide)
    eng = worker_mod.TradingEngine()
    eng._build_ai_context = lambda *a, **k: {"injected_mantra_ids": []}
    broker = _Broker()
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": prev_spread})
        update_group(
            "ai", {"enabled": prev_ai.enabled, "require_agreement": prev_ai.require_agreement}
        )

    assert "议会分歧" in _reject_reason(sid)
    assert not broker.calls


def test_insufficient_capital_is_rejected(monkeypatch):
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    prev_spread = get_strategy().max_spread_bps
    update_group("strategy", {"max_spread_bps": 0.0})
    eng = worker_mod.TradingEngine()
    broker = _Broker()
    eng._margin.begin_cycle(0.3)
    signal_id = _signal_row()
    try:
        asyncio.run(
            eng._do_open(
                broker, _sig(), _ticker(price=0.00002),
                SimpleNamespace(equity=0.4, available=0.3, unrealized=0.0),
                get_engine(), get_risk(), get_strategy(), signal_id,
            )
        )
    finally:
        update_group("strategy", {"max_spread_bps": prev_spread})

    assert "仓位不足" in _reject_reason(signal_id)
    assert not broker.calls


def test_successful_entry_passes_stop_and_target(monkeypatch):
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    prev_spread = get_strategy().max_spread_bps
    update_group("strategy", {"max_spread_bps": 0.0})
    eng = worker_mod.TradingEngine()
    broker = _Broker()
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": prev_spread})

    assert _reject_reason(sid) == ""
    assert len(broker.calls) == 1
    call = broker.calls[0]
    assert call["side"] == "long"
    # 止损在下方、止盈在上方，且按 tick 对齐
    assert call["sl_price"] < call["filters"].last_price < call["tp_price"]
    assert round(call["sl_price"], 2) == call["sl_price"]
    assert call["qty"] > 0


def test_failed_exchange_order_releases_margin(monkeypatch):
    """下单失败必须把预留额度还回去，否则后面几轮都开不出仓."""
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    prev_spread = get_strategy().max_spread_bps
    update_group("strategy", {"max_spread_bps": 0.0})
    eng = worker_mod.TradingEngine()
    broker = _Broker(ok=False, message="-2019 保证金不足")
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": prev_spread})

    assert "保证金" in _reject_reason(sid)
    assert eng._margin.remaining >= 99.0, "失败单占用的额度没有归还"


# ---------------------------------------------- 量化无方向候选（AI 定性）
# 事故：量化 flat 时 _scan 直接 continue，AI 从未被调用；线上连续多轮"AI 已开启但 0 调用"。
# 候选模式放开后，_do_open 必须能处理 direction="flat"：方向由 AI 给，止损按 ATR 兜底。


def _flat_sig():
    """量化无方向：只剩 ATR 特征可用，sl_pct/tp_pct 都是 0（它没算出进场位）."""
    return SimpleNamespace(
        symbol=SYMBOL, direction="flat", score=0.0, sl_pct=0.0, tp_pct=0.0,
        features={"atr_pct": 0.8, "rsi": 50.0, "vol_ratio": 1.2},
        reasons=["量化无方向"],
    )


def _run_flat(eng, broker, *, decision: AiDecision, ai_ready: bool = True):
    eng._margin.begin_cycle(100.0)
    signal_id = _signal_row()
    prev_ai = get_ai()
    prev_spread = get_strategy().max_spread_bps
    update_group("strategy", {"max_spread_bps": 0.0})
    update_group("ai", {"enabled": True, "allow_ai_override": False})
    try:
        asyncio.run(
            eng._do_open(
                broker, _flat_sig(), _ticker(),
                SimpleNamespace(equity=100.0, available=100.0, unrealized=0.0),
                get_engine(), get_risk(), get_strategy(), signal_id,
            )
        )
    finally:
        update_group("strategy", {"max_spread_bps": prev_spread})
        update_group(
            "ai", {"enabled": prev_ai.enabled, "allow_ai_override": prev_ai.allow_ai_override}
        )
    return signal_id


def test_flat_candidate_takes_ai_direction_even_without_override(monkeypatch):
    """allow_ai_override=False 也不该拦：量化本来没方向，谈不上"反向覆盖".

    同时验证止损兜底：AI 只给方向不给止损时，绝不能带着 0 止损进场
    （0 止损会让定仓按"零风险"把仓位放到保证金极限）。
    """
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    monkeypatch.setattr("nanoquant.ai.client.providers_available", lambda: 1)

    async def _decide(_ctx):
        return DecisionOutcome(
            decision=AiDecision(action="open_long", confidence=0.7, reason="结构偏多"),
            ok=True, agreement=1.0,
        )

    monkeypatch.setattr(worker_mod, "decide", _decide)
    eng = worker_mod.TradingEngine()
    eng._build_ai_context = lambda *a, **k: {"injected_mantra_ids": []}
    broker = _Broker()

    sid = _run_flat(eng, broker, decision=AiDecision(action="open_long"))

    assert _reject_reason(sid) == ""
    assert len(broker.calls) == 1
    call = broker.calls[0]
    assert call["side"] == "long"
    # ATR 0.8% × sl_atr_mult(1.2) ≈ 0.96% 兜底止损
    assert call["sl_price"] < call["filters"].last_price < call["tp_price"]
    assert call["qty"] > 0


def test_flat_candidate_rejected_when_ai_unavailable(monkeypatch):
    """无方向候选的全部依据是 AI；AI 不在线时不能"回退纯量化"硬开一单."""
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    monkeypatch.setattr("nanoquant.ai.client.providers_available", lambda: 0)
    eng = worker_mod.TradingEngine()
    broker = _Broker()

    sid = _run_flat(eng, broker, decision=AiDecision(action="open_long"))

    assert "AI 不可用" in _reject_reason(sid)
    assert not broker.calls


def test_flat_candidate_hold_is_reported_as_ai_veto(monkeypatch):
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    monkeypatch.setattr("nanoquant.ai.client.providers_available", lambda: 1)

    async def _decide(_ctx):
        return DecisionOutcome(
            decision=AiDecision(action="hold", confidence=0.8, reason="没有优势"),
            ok=True, agreement=1.0,
        )

    monkeypatch.setattr(worker_mod, "decide", _decide)
    eng = worker_mod.TradingEngine()
    eng._build_ai_context = lambda *a, **k: {"injected_mantra_ids": []}
    broker = _Broker()

    sid = _run_flat(eng, broker, decision=AiDecision(action="hold"))

    assert "AI 否决" in _reject_reason(sid)
    assert not broker.calls


def test_flat_signal_goes_scan_to_ai_to_order(monkeypatch):
    """端到端：量化 flat 的标的 → 扫描派发 → 调 AI → 下单。

    这是「AI 一整晚 0 调用」的回归用例：只要 _scan 又把 flat 提前 continue 掉，
    或者 _do_open 拒绝 flat 方向，这条链路就会断。
    """
    from nanoquant.db import session_scope
    from nanoquant.models import TradingSymbol

    symbol = "E2EAIUSDT"
    with session_scope() as s:
        s.query(TradingSymbol).filter(TradingSymbol.symbol == symbol).delete()
        s.add(
            TradingSymbol(symbol=symbol, enabled=True, max_leverage=20, precision=3,
                          tick_sz="0.01", min_sz="0.001", min_notional=5.0, sort_order=999)
        )
        muted = [
            (row.symbol, row.enabled) for row in s.query(TradingSymbol).all()
            if row.symbol != symbol and row.enabled
        ]
        for row in s.query(TradingSymbol).all():
            if row.symbol != symbol:
                row.enabled = False

    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda _s: _filters())
    monkeypatch.setattr("nanoquant.ai.client.providers_available", lambda: 1)

    async def _tickers(symbols):
        return {s: SimpleNamespace(last=100.0, change_pct=1.0, quote_volume=5e7, source="ws") for s in symbols}

    async def _funding(symbols):
        return {s: 0.0001 for s in symbols}

    async def _oi(symbols):
        return {s: 1_000_000.0 for s in symbols}

    async def _klines(*_a, **_k):
        # 扫描侧只要求 K 线根数够（>=30）并且能取到 o/h/l/c；指标本身被 build_features 替换
        return [
            SimpleNamespace(open=100.0, high=100.5, low=99.5, close=100.0, volume=1000.0,
                            open_time=int(time.time() * 1000) - i * 60_000)
            for i in range(60)
        ]

    features = {
        "close": 100.0, "price": 100.0, "atr_pct": 0.8, "vol_ratio": 1.3,
        "band_z": 2.0, "quote_volume": 5e7, "rsi": 50.0, "ema_gap_pct": 0.1,
        "data_ok": True, "ok": {"trend": True, "momentum": True, "volume": True},
        "_bars": [],
    }
    monkeypatch.setattr(worker_mod.binance_client, "tickers", _tickers)
    monkeypatch.setattr(worker_mod.binance_client, "funding_rates", _funding)
    monkeypatch.setattr(worker_mod.binance_client, "open_interest", _oi)
    monkeypatch.setattr(worker_mod.binance_client, "klines", _klines)
    monkeypatch.setattr(worker_mod.binance_client, "book_tickers", lambda *_a, **_k: _no_book())
    monkeypatch.setattr(worker_mod, "build_features", lambda *a, **k: dict(features))

    from nanoquant.engine.strategy import SignalResult

    def _flat_score(sym, feat, cfg):
        sig = SignalResult(symbol=sym, direction="flat", score=0.0, features=dict(feat))
        sig.blocked = "偏离不足"
        return sig

    monkeypatch.setattr(worker_mod, "score_signal", _flat_score)

    calls: list[str] = []

    async def _decide(_ctx):
        calls.append("ai")
        return DecisionOutcome(
            decision=AiDecision(action="open_long", confidence=0.7, reason="结构偏多"),
            ok=True, agreement=1.0,
        )

    monkeypatch.setattr(worker_mod, "decide", _decide)

    eng = worker_mod.TradingEngine()
    eng._build_ai_context = lambda *a, **k: {"injected_mantra_ids": []}
    broker = _Broker()
    prev_ai = get_ai()
    prev_strat = get_strategy()
    try:
        update_group("ai", {"enabled": True})
        update_group("strategy", {"auto_entry": True, "ai_review_flat": True,
                                  "ai_review_max_per_cycle": 1, "max_spread_bps": 0.0})

        async def _run():
            await eng._scan(
                broker, SimpleNamespace(equity=100.0, available=100.0, unrealized=0.0),
                get_engine(), get_risk(), get_strategy(),
            )
            await asyncio.sleep(0.2)  # 等 create_task 里的开仓任务跑完

        asyncio.run(_run())
    finally:
        update_group("ai", {"enabled": prev_ai.enabled})
        update_group("strategy", {
            "auto_entry": prev_strat.auto_entry,
            "ai_review_flat": prev_strat.ai_review_flat,
            "ai_review_max_per_cycle": prev_strat.ai_review_max_per_cycle,
            "max_spread_bps": prev_strat.max_spread_bps,
        })
        with session_scope() as s:
            for sym, was in muted:
                row = s.get(TradingSymbol, sym)
                if row is not None:
                    row.enabled = was

    assert calls == ["ai"], "量化 flat 的标的必须真的调用到 AI"
    assert len(broker.calls) == 1, "AI 给出方向后应下单"
    assert broker.calls[0]["side"] == "long"


async def _no_book():
    return {}


def test_open_records_experience_usage_for_audit(monkeypatch):
    """开仓决策必须留痕「这次看了哪些经验、有没有被记进决策日志」。

    以前 ai_meta 只带心法 id，经验注入既不进事件也不进持仓 features，
    于是"经验模块到底有没有参与"在面板上完全看不出来 —— 只能靠猜。
    """
    monkeypatch.setattr(worker_mod.symbol_service, "filters_for", lambda symbol: _filters())
    monkeypatch.setattr("nanoquant.ai.client.providers_available", lambda: 1)
    prev_spread = get_strategy().max_spread_bps
    prev_ai = get_ai()
    update_group("strategy", {"max_spread_bps": 0.0})
    update_group("ai", {"enabled": True})
    update_group("journal", {"enabled": True, "min_record_confidence": 0.45})

    async def _decide(_ctx):
        return DecisionOutcome(
            decision=AiDecision(action="open_long", confidence=0.7, reason="结构偏多"),
            ok=True, agreement=1.0,
        )

    monkeypatch.setattr(worker_mod, "decide", _decide)
    eng = worker_mod.TradingEngine()
    eng._build_ai_context = lambda *a, **k: {
        "injected_mantra_ids": [], "injected_experience_ids": [11, 12],
    }
    broker = _Broker()
    try:
        sid = _run(eng, broker)
    finally:
        update_group("strategy", {"max_spread_bps": prev_spread})
        update_group("ai", {"enabled": prev_ai.enabled})

    assert _reject_reason(sid) == ""
    assert len(broker.calls) == 1
    ai_meta = broker.calls[0]["features"]["ai"]
    assert ai_meta["experience_hits"] == 2
    assert ai_meta["injected_experience_ids"] == [11, 12]
    assert ai_meta["journal_id"], "方向性决策必须写入 decision_journal（闭环第一步）"
