"""语义契约测试: 把"改了不能坏"的规则固化成回归用例.

覆盖:
- 部分止盈的 R 计算 (含缺失止损的防御);
- 时间阶梯止盈的档位选择;
- 出场判定的优先级 (止损 / 止盈 / 阶梯止盈 / 移动止损 / 超时);
- 配置校验 (周期白名单、阶梯升序、部分止盈比例);
- 分段耗时的分位数计算;
- 回测引擎能跑通且不产生负数仓位.

这些用例**不联网、不碰数据库**, 适合放进 CI。
"""
from __future__ import annotations

from types import SimpleNamespace

import pytest
from pydantic import ValidationError

from nanoquant.backtest.engine import run_backtest
from nanoquant.engine.strategy import detect_exit, partial_tp_r, roi_target
from nanoquant.services.exec_metrics import ExecMetrics
from nanoquant.services.settings import StrategySettings


def bar(o: float, h: float, l: float, c: float, v: float = 1.0, t: int = 0):
    return SimpleNamespace(open=o, high=h, low=l, close=c, volume=v, open_time=t)


# ------------------------------------------------------------------ 部分止盈
def test_partial_tp_r_long_and_short():
    assert partial_tp_r("long", 100, 98, 102) == pytest.approx(1.0)
    assert partial_tp_r("long", 100, 98, 101) == pytest.approx(0.5)
    assert partial_tp_r("short", 100, 102, 98) == pytest.approx(1.0)
    assert partial_tp_r("short", 100, 102, 101) == pytest.approx(-0.5)


def test_partial_tp_r_guards_missing_stop():
    # 老数据没有初始止损时, 不能把 0 当成止损价算出假 R
    assert partial_tp_r("long", 100, 0, 110) == 0.0
    assert partial_tp_r("long", 0, 98, 110) == 0.0
    assert partial_tp_r("long", 100, 100, 110) == 0.0


# ------------------------------------------------------------ 时间阶梯止盈
def test_roi_target_picks_highest_reached_tier():
    table = [[0, 0.8], [15, 0.6], [30, 0.45], [60, 0.3], [120, 0.2]]
    assert roi_target(table, 0) == 0.8
    assert roi_target(table, 14.9) == 0.8
    assert roi_target(table, 15) == 0.6
    assert roi_target(table, 200) == 0.2
    assert roi_target([], 10) is None


# -------------------------------------------------------------- 出场判定优先级
def _cfg(**kwargs) -> StrategySettings:
    return StrategySettings(**kwargs)


def test_detect_exit_stop_loss_long():
    cfg = _cfg()
    hit, reason = detect_exit(
        side="long", entry_price=100, mark_price=97, peak_price=100,
        sl_price=98, tp_price=105, opened_at=0, now_ts=60, cfg=cfg, score=0.0,
    )
    assert hit and reason == "stop_loss"


def test_detect_exit_take_profit_long():
    cfg = _cfg()
    hit, reason = detect_exit(
        side="long", entry_price=100, mark_price=104, peak_price=104,
        sl_price=98, tp_price=103, opened_at=0, now_ts=60, cfg=cfg, score=0.0,
    )
    assert hit and reason == "take_profit"


def test_detect_exit_roi_tier():
    cfg = _cfg(roi_table=[[0, 0.5], [10, 0.2]])
    hit, reason = detect_exit(
        side="long", entry_price=100, mark_price=100.6, peak_price=100.6,
        sl_price=90, tp_price=120, opened_at=0, now_ts=60, cfg=cfg, score=0.0,
    )
    assert hit and reason == "roi"


def test_detect_exit_trailing_stop():
    cfg = _cfg(roi_table=[], trailing_pct=0.8)
    hit, reason = detect_exit(
        side="long", entry_price=100, mark_price=103.1, peak_price=104,
        sl_price=90, tp_price=0, opened_at=0, now_ts=60, cfg=cfg, score=0.0,
    )
    assert hit and reason == "trailing_stop"


def test_detect_exit_timeout():
    cfg = _cfg(roi_table=[], trailing_pct=0)
    hit, reason = detect_exit(
        side="long", entry_price=100, mark_price=100, peak_price=100,
        sl_price=90, tp_price=120, opened_at=0, now_ts=181 * 60, cfg=cfg, score=0.0,
    )
    assert hit and reason == "timeout"


# ------------------------------------------------------------------ 配置校验
def test_timeframe_whitelist():
    assert StrategySettings(timeframe="5m").timeframe == "5m"
    with pytest.raises(ValidationError):
        StrategySettings(timeframe="1m")


def test_roi_table_sorted_and_bounded():
    cfg = StrategySettings(roi_table=[[5, 0.3], [0, 0.6]])
    assert cfg.roi_table == [[0, 0.6], [5, 0.3]]
    with pytest.raises(ValidationError):
        StrategySettings(roi_table=[[0, 99]])


def test_partial_tp_ratio_bounds():
    assert StrategySettings(partial_tp_ratio=0.5).partial_tp_ratio == 0.5
    with pytest.raises(ValidationError):
        StrategySettings(partial_tp_ratio=1.5)
    with pytest.raises(ValidationError):
        StrategySettings(partial_tp_at_r=-1)


# ---------------------------------------------------------------- 分段耗时
def test_exec_metrics_percentiles():
    metrics = ExecMetrics(capacity=100)
    for value in [1.0, 2.0, 3.0, 4.0, 5.0]:
        metrics.record({"total_ms": value})
    summary = metrics.summary()
    assert summary["count"] == 5
    assert summary["total_ms"]["p50"] == pytest.approx(3.0, abs=0.1)
    assert summary["total_ms"]["max"] == 5.0
    metrics.reset()
    assert metrics.summary()["count"] == 0


# --------------------------------------------------------------------- 回测
def test_backtest_runs_without_crashing():
    candles = []
    price = 100.0
    for i in range(400):
        price *= 1.0005
        candles.append(
            bar(
                o=price,
                h=price * 1.001,
                l=price * 0.999,
                c=price,
                v=1000.0,
                t=i * 300_000,
            )
        )
    result = run_backtest(candles, StrategySettings(), symbol="TESTUSDT")
    summary = result.summary
    assert summary.get("error") is None
    assert summary["trades"] >= 0
    assert summary["fee_total"] >= 0
    assert summary["final_equity"] > 0
    for key in ("by_hour", "by_weekday", "by_hold", "by_exit"):
        assert key in summary
    for trade in result.trades:
        assert trade.qty >= 0
        assert trade.reason in {"stop_loss", "take_profit", "roi", "trailing_stop", "timeout"}


def test_backtest_needs_enough_bars():
    result = run_backtest([bar(1, 1, 1, 1, t=i * 60_000) for i in range(10)], StrategySettings())
    assert "error" in result.summary


# -------------------------------------------------------------- 部分平仓数量
def test_partial_close_qty_rounding_and_min():
    from nanoquant.engine.strategy import partial_close_qty

    # 正常取整
    assert partial_close_qty(1.0, 0.5, 0.001, 0.001) == pytest.approx(0.5)
    # 取整后低于最小下单量 → 放弃
    assert partial_close_qty(0.015, 0.5, 0.001, 0.01) == 0.0
    # 取整后为 0 → 放弃
    assert partial_close_qty(0.001, 0.5, 0.001, 0.001) == 0.0
    # 无步进时按原值
    assert partial_close_qty(1.0, 0.3, 0.0, 0.001) == pytest.approx(0.3)
    # 非法输入
    assert partial_close_qty(0, 0.5, 0.001, 0.001) == 0.0
    assert partial_close_qty(1.0, 0, 0.001, 0.001) == 0.0


# ---------------------------------------------------------------- 密钥归一化
def test_normalize_pem_and_public_key_detection():
    import base64

    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

    from nanoquant.exchange.client import looks_like_public_key, normalize_pem

    pem = Ed25519PrivateKey.generate().public_key().public_bytes(
        Encoding.PEM, PublicFormat.SubjectPublicKeyInfo
    ).decode()
    flat = normalize_pem(pem)
    assert "\n" not in flat and "-----" not in flat
    assert flat == base64.b64encode(base64.b64decode(flat)).decode()
    assert looks_like_public_key(pem) is True
    assert looks_like_public_key("a" * 64) is False


def test_sign_query_accepts_pem_private_key():
    import base64

    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, NoEncryption, PrivateFormat

    from nanoquant.exchange.client import sign_query

    priv = Ed25519PrivateKey.generate()
    pem = priv.private_bytes(Encoding.PEM, PrivateFormat.PKCS8, NoEncryption()).decode()
    query = "x=9&timestamp=1700000000000"
    priv.public_key().verify(base64.b64decode(sign_query(query, pem)), query.encode())


# ------------------------------------------------------- 单行 PEM(真实踩坑)
def test_single_line_pem_private_key_signs():
    """币安页面/编辑器常把 PEM 压成一行, 归一化必须按标记片段删除而不是按行过滤."""
    import base64

    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, NoEncryption, PrivateFormat

    from nanoquant.exchange.client import normalize_pem, sign_query

    priv = Ed25519PrivateKey.generate()
    pem = priv.private_bytes(Encoding.PEM, PrivateFormat.PKCS8, NoEncryption()).decode()
    one_line = " ".join(pem.split())

    body = normalize_pem(one_line)
    assert body and "-----" not in body
    assert base64.b64decode(body)

    query = "symbol=BTCUSDT&timestamp=1700000000000"
    priv.public_key().verify(base64.b64decode(sign_query(query, one_line)), query.encode())


# ---------------------------------------------------------------- 交易所签名
def test_sign_query_supports_ed25519_and_hmac():
    import base64
    import hashlib
    import hmac

    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, NoEncryption, PrivateFormat

    from nanoquant.exchange.client import sign_query

    query = "a=1&timestamp=1700000000000"

    # Ed25519: 签名是 base64, 能用公钥验证
    priv = Ed25519PrivateKey.generate()
    secret = base64.b64encode(
        priv.private_bytes(Encoding.DER, PrivateFormat.PKCS8, NoEncryption())
    ).decode()
    priv.public_key().verify(base64.b64decode(sign_query(query, secret)), query.encode())

    # HMAC: 行为保持不变
    hmac_secret = "plain-hmac-secret"
    expect = hmac.new(hmac_secret.encode(), query.encode(), hashlib.sha256).hexdigest()
    assert sign_query(query, hmac_secret) == expect


# ---------------------------------------------------------------- 告警静默
def test_quiet_hours_parsing():
    from nanoquant.services.alerts import _in_quiet_hours

    assert _in_quiet_hours("") is False
    assert _in_quiet_hours("bad-spec") is False
    # 全天区间本质上等价于 start == end -> 视为不静默
    assert _in_quiet_hours("08:00-08:00") is False


# ---------------------------------------------------------------- 写库容错
def test_write_retryable_classification():
    from nanoquant.services.db_writer import _is_retryable

    assert _is_retryable("sqlite3.OperationalError: database is locked")
    assert _is_retryable("database table is locked")
    assert not _is_retryable("boom")
    assert not _is_retryable("")


# ------------------------------------------------------------------ 参数网格
def test_grid_runs_and_ranks():
    from nanoquant.backtest.grid import run_grid

    candles = []
    price = 100.0
    for i in range(400):
        price *= 1.0006 if i % 3 else 0.9997
        candles.append(bar(o=price, h=price * 1.001, l=price * 0.999, c=price, v=1000.0, t=i * 300_000))

    result = run_grid(
        candles,
        StrategySettings(),
        symbol="TESTUSDT",
        grid={"entry_threshold": [0.2, 0.4], "sl_atr_mult": [1.0, 1.5]},
        leverage=20,
    )
    assert result["runs"] == 4
    assert result["best"] is not None
    returns = [row["summary"]["total_return_pct"] for row in result["items"]]
    assert returns == sorted(returns, reverse=True)
    assert set(result["items"][0]["params"]) == {"entry_threshold", "sl_atr_mult"}
