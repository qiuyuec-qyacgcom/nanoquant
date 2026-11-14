"""信号落库的体积：flat 信号不该存整份指标快照.

实测生产库：signals 表 29,590 行里有 27,558 行是 flat（93%），每行 features 平均 1.3KB，
合计 34MB —— 占整个 56MB 数据库的六成。这些快照只在"为什么没开仓"的审计里有用，
不需要逐指标原样留档；而且去重命中时还会把整块 JSON 重写一次，纯放大 WAL。
"""
from __future__ import annotations

import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.engine import worker as worker_mod
from nanoquant.models import Signal

SYMBOL = "SIGFMTUSDT"


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


@pytest.fixture(autouse=True)
def _clean():
    with session_scope() as s:
        s.query(Signal).filter(Signal.symbol == SYMBOL).delete(synchronize_session=False)
    yield
    with session_scope() as s:
        s.query(Signal).filter(Signal.symbol == SYMBOL).delete(synchronize_session=False)


def _sig(direction: str = "flat", score: float = 0.05):
    return SimpleNamespace(
        symbol=SYMBOL,
        direction=direction,
        score=score,
        features={
            "bars": 59, "close": 100.0, "ema_fast": 100.1, "ema_slow": 99.8,
            "rsi": 48.2, "atr": 0.9, "macd_hist": -0.002, "ret_1": 0.1,
            "ret_5": -0.2, "ret_15": 0.3, "vol_ratio": 0.9, "funding": 0.0001,
            "open_interest": 2.1e10, "oi_change_pct": None, "change_24h": -1.2,
            "quote_volume": 6.1e8, "atr_pct": 0.9, "ema_gap_pct": 0.3,
            "regime": "trend",
        },
        blocked="分数不足",
    )


def _rows():
    with session_scope() as s:
        return [
            {"id": r.id, "features": dict(r.features or {}), "score": r.score, "repeats": r.repeats, "ts": r.ts}
            for r in s.query(Signal).filter(Signal.symbol == SYMBOL).order_by(Signal.id).all()
        ]


def _ticker(price: float = 100.0):
    return SimpleNamespace(last=price, quote_volume=6e8, change_pct=-1.2)


def test_flat_signal_keeps_only_audit_fields():
    eng = worker_mod.TradingEngine()
    eng._record_signal(_sig("flat"), _ticker(), "scan")

    row = _rows()[0]
    keys = set(row["features"])
    # 丢掉逐指标快照
    assert "ema_fast" not in keys and "macd_hist" not in keys and "open_interest" not in keys
    # 保留"为什么没开"需要的
    for key in ("price", "source", "atr_pct", "vol_ratio", "rsi", "first_ts"):
        assert key in keys, key


def test_pending_signal_keeps_full_features():
    """有方向的信号要进 AI 复核，特征必须完整（否则复核少看指标）."""
    eng = worker_mod.TradingEngine()
    eng._record_signal(_sig("long", 0.7), _ticker(), "scan")

    keys = set(_rows()[0]["features"])
    assert {"ema_fast", "macd_hist", "open_interest", "regime"} <= keys


def test_dedupe_hit_does_not_rewrite_features():
    """60s 内同一状态只合并计数：以前每轮都把 1.3KB JSON 重写一遍."""
    eng = worker_mod.TradingEngine()
    eng._record_signal(_sig("flat", 0.05), _ticker(), "scan")
    first = _rows()
    assert len(first) == 1

    eng._record_signal(_sig("flat", 0.06), _ticker(101.0), "scan")
    second = _rows()

    assert len(second) == 1, "同状态应合并成一行"
    assert second[0]["repeats"] == 2
    assert second[0]["score"] == pytest.approx(0.06)
    assert second[0]["features"] == first[0]["features"], "特征快照不该被重写（价格变化不影响审计结论）"


def test_flat_row_is_much_smaller():
    import json

    eng = worker_mod.TradingEngine()
    eng._record_signal(_sig("flat"), _ticker(), "scan")

    size = len(json.dumps(_rows()[0]["features"], ensure_ascii=False))
    assert size < 400, f"flat 快照应压到 400B 以内（实测 {size}B），全量时约 1300B"


def test_dedupe_window_expires(monkeypatch):
    """超过窗口要新起一行，否则审计页看不到"这段时间一直在扫"的粒度."""
    eng = worker_mod.TradingEngine()
    eng._record_signal(_sig("flat"), _ticker(), "scan")
    key = next(iter(eng._signal_dedup))
    with session_scope() as s:
        row = s.get(Signal, eng._signal_dedup[key])
        row.features = {**(row.features or {}), "first_ts": int(time.time()) - 300}
    eng._signal_dedup = {}

    eng._record_signal(_sig("flat"), _ticker(), "scan")

    assert len(_rows()) == 2
