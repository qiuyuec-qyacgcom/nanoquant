"""语义契约测试: 把"改了不能坏"的规则固化成回归用例.

覆盖:
- 部分止盈的 R 计算 (含缺失止损的防御);
- 时间阶梯止盈的档位选择;
- 出场判定的优先级 (止损 / 止盈 / 阶梯止盈 / 移动止损 / 超时);
- 配置校验 (周期白名单、阶梯升序、部分止盈比例);
- 分段耗时的分位数计算;
- 回测引擎能跑通且不产生负数仓位;
- 执行 sidecar 协议/长连接复用与影子比对的判定.

这些用例**不联网、不碰真实数据库**（涉及库的一律走 conftest 的临时目录）, 适合放进 CI。
"""
from __future__ import annotations

import json
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


# -------------------------------------------------- 紧凑决策协议 v2 / 中文转译
def _ai_ctx():
    bars = [{"o": 0.1, "h": 0.11, "l": 0.09, "c": 0.105} for _ in range(14)]
    features = {
        "close": 0.105, "ema_fast": 0.104, "ema_slow": 0.103,
        "rsi": 81.8, "atr": 0.001, "atr_pct": 0.95, "vol_ratio": 1.93,
        "ema_gap_pct": 0.988, "ret_5": 1.93, "ret_15": 5.09,
        "quote_volume": 4.2e7, "funding": 0.00005, "_bars": bars,
    }
    return {
        "symbol": "ARBUSDT", "price": 0.105, "score": 0.697, "side_hint": "long",
        "quant_reasons": ["趋势 +0.988%", "量比 1.93"],
        "features": features, "klines": bars,
        "funding": 0.00005, "change_24h": 12.3, "quote_volume": 4.2e7,
        "account": {"equity": 5000, "available": 2400, "open_positions": 1, "today_pnl_pct": 0.3},
        "constraints": {"max_leverage": 35, "min_confidence": 0.45, "fee_rate_taker": 0.0005},
        "news": [{"title": "BTC 现货 ETF 净流入 1.05 亿美元", "sentiment": 0.42, "symbols": ["BTC"]} for _ in range(6)],
        "mantras": [{"title": "超买不追多", "rule": "RSI 高于 80 时禁止顺势追多" * 6, "health": 0.82} for _ in range(8)],
    }


def test_p0_snapshot_drops_duplicate_bars_and_long_text():
    import re

    from nanoquant.ai.prompts import clean_features, format_snapshot

    ctx = _ai_ctx()
    cleaned = clean_features(ctx["features"])
    assert "_bars" not in cleaned and "close" not in cleaned
    assert cleaned["rsi"] == 81.8

    v1 = format_snapshot(ctx)
    assert '"_bars"' not in v1
    assert v1.count('"recent_bars_ohlc"') == 1
    # 心法 rule 长文本 / 新闻长标题都被裁掉，条数也压缩
    assert "禁止顺势追多" not in v1
    assert v1.count('"sentiment"') == 3  # 新闻 6 条 → 3 条
    assert v1.count('"health"') == 4     # 心法 8 条 → 4 条
    assert len(v1) < 1200
    assert not re.search(r"[\u4e00-\u9fff]", json.dumps(cleaned))


def test_v2_snapshot_is_much_smaller_than_v1():
    import re

    from nanoquant.ai.compact import format_snapshot_v2
    from nanoquant.ai.prompts import format_snapshot

    ctx = _ai_ctx()
    v1, v2 = format_snapshot(ctx), format_snapshot_v2(ctx)
    assert len(v2) < len(v1) * 0.6
    assert '"_bars"' not in v2 and '"features"' not in v2
    assert '"k"' in v2 and '"s":"ARBUSDT"' in v2
    assert not re.search(r"[\u4e00-\u9fff]", v2)  # 输入全 ASCII


def test_v2_output_is_translated_to_chinese():
    from nanoquant.ai.compact import is_compact_payload, validate_codes
    from nanoquant.ai.decision import parse_decision

    assert validate_codes() == []
    v2 = json.dumps({
        "a": "H", "c": 0.62, "l": 3, "s": 1.6, "t": 2.0,
        "r": [12, 9], "k": [21, 11], "x": 1,
        "n": {"rsi": 81.8, "atr": 0.95}, "f": "wait for pullback",
    })
    assert is_compact_payload(json.loads(v2))
    d = parse_decision(v2, _ai_ctx())
    assert d.action == "hold" and d.confidence == pytest.approx(0.62)
    assert d.leverage == 3 and d.sl_pct == pytest.approx(1.6)
    assert "RSI 超买" in d.reason and "手续费" in d.reason
    assert "RSI 81.8" in d.reason and "ATR% 0.95" in d.reason  # 数值占位
    assert "wait for pullback" in d.reason
    assert "强平价" in "".join(d.risk_flags)
    assert "RSI 回落至 70" in d.invalidation


def test_v1_output_still_parses_and_auto_detection_works():
    from nanoquant.ai.compact import is_compact_payload
    from nanoquant.ai.decision import parse_decision

    v1 = json.dumps({
        "action": "open_long", "confidence": 0.7, "leverage": 5,
        "sl_pct": 1.2, "tp_pct": 2.4, "reason": "趋势确认",
        "risk_flags": ["追高"], "invalidation": "跌破 EMA21",
    })
    assert not is_compact_payload(json.loads(v1))
    d = parse_decision(v1, _ai_ctx())
    assert d.action == "open_long" and d.reason == "趋势确认"


def test_truncation_is_reported_explicitly():
    from nanoquant.ai.client import output_problem

    assert "max_tokens" in output_problem("{\"a\":", "length", 1200)
    assert "thinking" in output_problem("", "stop", 3000)
    assert output_problem('{"a":"H"}', "stop", 3000) == ""


def test_compact_defaults_are_enabled():
    from nanoquant.models import AiProvider
    from nanoquant.services.settings import AiSettings

    assert AiSettings().decision_schema_version == 2
    # SQLAlchemy 的 default 在 INSERT 时生效，实例化对象上仍是 None
    assert AiProvider.__table__.c.max_tokens.default.arg == 3000


# ------------------------------------------------------------ 思考强度映射
def test_reasoning_effort_maps_to_protocol_params():
    from nanoquant.ai.reasoning import apply_effort, normalize, supports_effort

    assert normalize("HIGH") == "high"
    assert normalize("ultra") == "auto"
    assert supports_effort("cn:deepseek-v4.1-flash")
    assert supports_effort("o3-mini")
    assert not supports_effort("gpt-4o-mini")

    # Anthropic: thinking 预算 + max_tokens 必须大于预算
    p = {"model": "claude", "max_tokens": 3000}
    keys = apply_effort(
        p, protocol="anthropic_messages", model="claude", effort="high", base_max_tokens=3000
    )
    assert p["thinking"] == {"type": "enabled", "budget_tokens": 16000}
    assert p["max_tokens"] == 19000
    assert keys == ["thinking", "max_tokens"]

    # 极值档触到上限时下调 budget，保证 max_tokens > budget
    p_max = {"max_tokens": 3000}
    apply_effort(p_max, protocol="anthropic_messages", model="claude", effort="max", base_max_tokens=3000)
    assert p_max["max_tokens"] == 64000
    assert p_max["thinking"]["budget_tokens"] < p_max["max_tokens"]

    p_none = {}
    apply_effort(p_none, protocol="anthropic_messages", model="claude", effort="none", base_max_tokens=3000)
    assert p_none["thinking"] == {"type": "disabled"}

    # auto 不注入
    p_auto = {}
    assert apply_effort(p_auto, protocol="openai_chat", model="deepseek-v4.1", effort="auto", base_max_tokens=3000) == []
    assert p_auto == {}

    # OpenAI 兼容：只对推理型模型注入
    p_openai = {}
    assert apply_effort(
        p_openai, protocol="openai_chat", model="deepseek-v4.1-flash", effort="low", base_max_tokens=3000
    ) == ["reasoning_effort"]
    assert p_openai["reasoning_effort"] == "low"

    p_plain = {}
    assert apply_effort(p_plain, protocol="openai_chat", model="gpt-4o-mini", effort="high", base_max_tokens=3000) == []
    assert p_plain == {}

    # Responses API 用 reasoning.effort，极值档收敛到 high
    p_resp = {}
    apply_effort(p_resp, protocol="openai_responses", model="o3-mini", effort="max", base_max_tokens=3000)
    assert p_resp["reasoning"] == {"effort": "high"}


def test_reasoning_model_library_covers_2026_families():
    from nanoquant.ai.reasoning import (
        effort_options,
        library,
        match_family,
        supports_effort,
        supports_extreme_effort,
    )

    reasoning_models = [
        "gpt-6", "chatgpt-6", "gpt-5-mini", "o1-preview", "o3-mini", "o4-mini",
        "gpt-oss-120b", "claude-4-opus", "claude-3-7-sonnet", "gemini-3-pro",
        "gemini-2.5-flash", "deepseek-v4.1-flash", "deepseek-reasoner",
        "qwen3-vl-235b", "qwen-3-max", "qwq-32b", "kimi-k3", "kimi-k2.7",
        "glm-5", "minimax-m2", "mimo-v2", "grok-4", "grok-3-mini",
    ]
    for model in reasoning_models:
        assert supports_effort(model), model
        assert match_family(model) is not None

    for model in ["gpt-4o-mini", "gpt-3.5-turbo", "qwen2.5-72b", "llama-3.1-70b",
                  "mistral-large", "deepseek-chat", "claude-3-5-haiku"]:
        assert not supports_effort(model), model

    assert supports_extreme_effort("gpt-5")
    assert supports_extreme_effort("o4-mini")
    assert supports_extreme_effort("grok-4")
    assert not supports_extreme_effort("deepseek-v4.1-flash")
    assert not supports_extreme_effort("qwen3-max")

    opts = {o["value"]: o for o in effort_options("deepseek-v4.1-flash")}
    assert opts["high"]["enabled"] and not opts["xhigh"]["enabled"]
    assert opts["xhigh"]["requires_extreme"] is True

    lib = library()
    assert {f["key"] for f in lib["families"]} >= {"openai-gpt5", "qwen3", "minimax", "gpt-oss"}
    assert "qwen2.5" in lib["non_reasoning_hints"]


def test_provider_schema_defaults_for_reasoning():
    from nanoquant.api.schemas import ProviderIn, ProviderPatch

    p = ProviderIn(name="x", base_url="http://localhost:9/v1")
    assert p.reasoning_effort == "auto"
    assert p.max_tokens == 3000
    assert ProviderPatch().reasoning_effort is None


# ------------------------------------------------------------ 下单精度 -1111
def test_floor_to_step_and_legacy_formatter():
    """根因：float(12417.2) 在 :.12f 下就是 12417.200000000001 → -1111。

    数值层面 floor_to_step 正确，但发给交易所的字符串必须用 format_qty 定格小数位。
    """
    from nanoquant.exchange.precision import SymbolFilters, ceil_to_step, floor_to_step, round_to_tick

    qty = floor_to_step(2250.0 / 0.1812, 0.1)
    assert qty == pytest.approx(12417.2)
    legacy = f"{qty:.12f}".rstrip("0").rstrip(".")
    assert legacy.count(".") == 1 and len(legacy.split(".")[1]) > 1  # 旧写法超出 1 位精度
    f = SymbolFilters(symbol="ARBUSDT", qty_step=0.1, price_tick=0.0001)
    assert f.format_qty(qty) == "12417.2"
    assert ceil_to_step(3.14159, 0.01) == 3.15
    assert round_to_tick(0.18126, 0.0001) == 0.1813


def test_symbol_filters_formats_exchange_precision():
    from nanoquant.exchange.precision import SymbolFilters

    arb = SymbolFilters(symbol="ARBUSDT", price_tick=0.0001, qty_step=0.1, min_qty=0.1)
    assert arb.format_qty(12417.200000000001) == "12417.2"
    assert arb.format_price(0.181249999) == "0.1812"
    assert arb.qty_decimals() == 1 and arb.price_decimals() == 4

    btc = SymbolFilters(symbol="BTCUSDT", price_tick=0.1, qty_step=0.0001, min_qty=0.0001)
    assert btc.format_qty(0.082399999999) == "0.0823"
    assert btc.format_price(74374.66) == "74374.6"


def test_arb_order_quantity_would_not_trigger_1111():
    """按真实账户/ARB 规格复算：修复前发送 12417.200000000001，修复后 12417.2."""
    from nanoquant.exchange.broker import estimate_qty
    from nanoquant.exchange.precision import SymbolFilters
    from nanoquant.services.settings import RiskSettings

    cfg = RiskSettings(risk_per_trade_pct=10.0, margin_pct=50.0, default_leverage=1)
    f = SymbolFilters(symbol="ARBUSDT", price_tick=0.0001, qty_step=0.1, min_qty=0.1, min_notional=5.0)
    f.last_price = 0.1812
    qty, note = estimate_qty(5000.0, 0.1812, 1, cfg, 0.009, f, available=2500.0)
    assert note == "ok"
    sent = f.format_qty(qty)
    assert sent.count(".") == 1 and len(sent.split(".")[1]) <= 1
    # 即使外部传入带二进制尾的旧值，format_qty 也必须救回来
    assert f.format_qty(12417.200000000001) == "12417.2"


def test_precision_hint_is_actionable():
    from nanoquant.exchange.broker import precision_hint
    from nanoquant.exchange.precision import SymbolFilters

    text = precision_hint(SymbolFilters(symbol="ARBUSDT", qty_step=0.1, price_tick=0.0001), 12417.2)
    assert "-1111" in text and "qty_step=0.1" in text and "重新添加" in text


# ------------------------------------------------ 并发开仓的保证金账本
def test_margin_ledger_serializes_concurrent_opens():
    """复现 20:40 那一轮：DOGE/ARB 同一秒各按 available 算满额，第二笔 -2019."""
    import asyncio

    from nanoquant.engine.margin import MarginLedger

    async def run():
        ledger = MarginLedger()
        ledger.begin_cycle(3000.0)
        assert ledger.remaining == 3000.0
        gen = await ledger.reserve(2720.0)
        assert gen == 1
        assert await ledger.reserve(2720.0) == 0  # 第二笔没有额度了
        assert ledger.remaining == pytest.approx(280.0)

        await ledger.release(2720.0, gen)  # 下单失败要归还
        assert await ledger.reserve(2720.0) == 1

        # 跨轮迟到的归还不能动新一轮的预算（用剩余 280 占满本轮）
        old_gen = await ledger.reserve(280.0)
        assert old_gen == 1
        ledger.begin_cycle(3000.0)
        assert await ledger.reserve(500.0) == 2
        await ledger.release(280.0, old_gen)
        assert ledger.used == pytest.approx(500.0)

        ledger.begin_cycle(3000.0)  # 下一轮用新快照重置
        assert ledger.remaining == 3000.0
        # 真并发：只允许一个成功
        results = await asyncio.gather(ledger.reserve(2000.0), ledger.reserve(2000.0))
        assert sorted(results) == [0, 3]

    asyncio.run(run())


def test_plan_position_respects_ledger_remaining():
    from nanoquant.engine.risk import plan_position
    from nanoquant.exchange.precision import SymbolFilters
    from nanoquant.services.settings import RiskSettings

    account = SimpleNamespace(equity=5000.0, available=3000.0)
    cfg = RiskSettings(risk_per_trade_pct=10.0, margin_pct=50.0, default_leverage=5)
    f = SymbolFilters(symbol="ARBUSDT", price_tick=0.0001, qty_step=0.1, min_qty=0.1, min_notional=5.0)

    _, full = plan_position(
        price=0.1812, filters=f, account=account, cfg_risk=cfg, sl_pct=0.9, leverage=5,
    )
    _, rest = plan_position(
        price=0.1812, filters=f, account=account, cfg_risk=cfg, sl_pct=0.9, leverage=5,
        available=280.0,
    )
    assert full["available"] == 3000.0
    assert rest["available"] == 280.0
    assert rest["margin"] <= 280.0 * 0.9 + 1e-6
    assert rest["margin"] < full["margin"]


# ------------------------------------------------------ 可用保证金与 -2019
def test_estimate_qty_caps_by_available_margin():
    """复现 -2019: 按总权益算得起的仓位，可用余额其实付不起初始保证金."""
    from nanoquant.exchange.broker import MARGIN_SAFETY, estimate_qty
    from nanoquant.exchange.precision import SymbolFilters
    from nanoquant.services.settings import RiskSettings

    cfg = RiskSettings(risk_per_trade_pct=10.0, margin_pct=50.0, default_leverage=1)
    filters = SymbolFilters(symbol="ICNTUSDT", qty_step=1.0, min_qty=1.0, min_notional=5.0)
    # equity 5000 / leverage 1 → margin_cap = 2500 名义额，需 2500 保证金
    no_cap, _ = estimate_qty(5000.0, 0.3, 1, cfg, 0.009, filters)
    assert no_cap == pytest.approx(8333.0, abs=1.0)

    # 可用余额只有 2000 → 名义额被压到 2000*0.9 = 1800
    capped, note = estimate_qty(5000.0, 0.3, 1, cfg, 0.009, filters, available=2000.0)
    assert capped == pytest.approx(2000.0 * MARGIN_SAFETY / 0.3, abs=1.0)
    assert note == "ok"

    # 连最小名义额都付不起 → 明确拒绝，不再把订单丢给交易所吃 -2019
    qty, note = estimate_qty(5000.0, 0.3, 1, cfg, 0.009, filters, available=1.0)
    assert qty == 0.0
    assert "可用保证金不足" in note


def test_estimate_qty_high_leverage_still_respects_available():
    from nanoquant.exchange.broker import estimate_qty
    from nanoquant.exchange.precision import SymbolFilters
    from nanoquant.services.settings import RiskSettings

    cfg = RiskSettings(risk_per_trade_pct=10.0, margin_pct=50.0, default_leverage=20)
    filters = SymbolFilters(symbol="ICNTUSDT", qty_step=0.001, min_qty=0.001, min_notional=5.0)
    qty, note = estimate_qty(5000.0, 100.0, 20, cfg, 0.01, filters, available=100.0)
    assert note == "ok"
    # 所需初始保证金 = qty*price/20 <= 100*0.9
    assert qty * 100.0 / 20 <= 100.0 * 0.9 + 1e-6


def test_margin_insufficient_hint_is_actionable():
    from nanoquant.exchange.broker import margin_insufficient_hint

    text = margin_insufficient_hint(2493.0, 1)
    assert "-2019" in text and "2493.00" in text
    assert "平掉" in text and "杠杆" in text


# ------------------------------------------------------------ 持仓浮盈亏
def _position(**kwargs):
    base = dict(
        symbol="TESTUSDT", side="short", entry_price=100.0, mark_price=100.0,
        qty=10.0, margin=100.0, leverage=10.0, partial_taken_at=0,
    )
    base.update(kwargs)
    return SimpleNamespace(**base)


def test_derived_prefers_fresh_exchange_unrealized():
    """lastPrice 冻结但交易所 markPrice 已变: 必须显示交易所的浮盈亏, 不能显示 0."""
    import time

    from nanoquant.services.position_view import derived

    now = int(time.time())
    stale_price = _position(mark_price=100.0)  # 24h lastPrice 没动
    assert derived(stale_price)["unrealized"] == 0.0

    with_venue = _position(mark_price=100.0, venue_unrealized=-88.79837226, venue_ts=now)
    out = derived(with_venue)
    assert out["unrealized"] == pytest.approx(-88.79837226)
    assert out["unrealized_source"] == "exchange"
    assert out["roi_pct"] == pytest.approx(-88.798, abs=0.01)

    # 超过 90s 的快照不能继续用，回退到按标记价估算
    old = _position(mark_price=101.0, venue_unrealized=999.0, venue_ts=now - 120)
    fallback = derived(old)
    assert fallback["unrealized"] == pytest.approx(-10.0)  # (101-100)*10*(-1)
    assert fallback["unrealized_source"] == "estimated"


def test_merge_venue_syncs_position_rows_with_account_total():
    """总览：持仓行必须与顶部 account.unrealized 同源同刻，不能各自用不同时刻的快照."""
    import time

    from nanoquant.services.position_view import derived, merge_venue

    now = int(time.time())
    rows = []
    # 两笔仓位：DB 里是 15s 前的旧快照，venue 是本次请求的实时值
    for mark, stale, live in ((101.0, -8.0, -10.0), (99.0, 5.0, 7.0)):
        pos = _position(mark_price=100.0, venue_unrealized=stale, venue_ts=now - 15)
        rows.append({"mark_price": 100.0, **derived(pos)})
        rows[-1] = merge_venue(
            rows[-1], qty=10.0, margin=100.0, mark_price=mark,
            unrealized=live, ts=now,
        )
    assert [r["unrealized"] for r in rows] == [-10.0, 7.0]
    assert abs(sum(r["unrealized"] for r in rows) - (-3.0)) < 1e-9  # 账户 totalUnrealizedProfit
    assert all(r["unrealized_source"] == "exchange" and r["venue_ts"] == now for r in rows)
    assert rows[0]["mark_price"] == 101.0 and rows[0]["roi_pct"] == -10.0

    # venue 价格无效时保持原样（等价于本次没读到，前端显示「估算」）
    keep = {"mark_price": 100.0, **derived(_position(venue_unrealized=1.0, venue_ts=now))}
    assert merge_venue(keep, qty=10, margin=100, mark_price=0, unrealized=2.0, ts=now) is keep


def test_record_external_position_writes_mode_and_venue_pnl():
    from nanoquant.db import init_db, session_scope
    from nanoquant.exchange.broker import record_external_position
    from nanoquant.models import PositionRecord
    from nanoquant.services.position_view import derived

    init_db()
    with session_scope() as s:
        s.query(PositionRecord).filter(PositionRecord.symbol == "EXTUSDT").delete()
    record_external_position("EXTUSDT", "long", 2.0, 100.0, 101.0, 10.0, "demo", unrealized=2.0)
    with session_scope() as s:
        row = s.query(PositionRecord).filter(PositionRecord.symbol == "EXTUSDT").one()
        assert row.mode == "demo" and row.status == "open"
        assert row.venue_unrealized == 2.0 and row.venue_ts > 0
        assert derived(row)["unrealized"] == pytest.approx(2.0)
        s.delete(row)


# ------------------------------------------------------ 轮询成本与信号去重
def test_cycle_interval_has_15s_floor_and_migrates_legacy_value():
    from pydantic import ValidationError

    from nanoquant.db import init_db, session_scope
    from nanoquant.models import Setting
    from nanoquant.services.settings import MIN_CYCLE_INTERVAL_SEC, EngineSettings, get_group, invalidate

    assert MIN_CYCLE_INTERVAL_SEC == 15
    assert EngineSettings().cycle_interval_sec == 15
    with pytest.raises(ValidationError):
        EngineSettings(cycle_interval_sec=5)

    init_db()
    with session_scope() as s:
        row = s.get(Setting, "engine") or Setting(key="engine", value="{}")
        raw = {"cycle_interval_sec": 5, "max_positions": 3}
        row.value = json.dumps(raw)
        s.add(row)
    invalidate("engine")
    assert get_group("engine", use_cache=False)["cycle_interval_sec"] == 15
    assert get_group("engine", use_cache=False)["max_positions"] == 3  # 不能整组回退默认值
    with session_scope() as s:
        assert json.loads(s.get(Setting, "engine").value)["cycle_interval_sec"] == 15


def test_funding_and_open_interest_are_cached():
    import asyncio

    from nanoquant.exchange.client import BinanceClient

    calls: list[str] = []

    async def fake_request(method, path, params=None, **kwargs):  # noqa: ARG001
        calls.append(path)
        if path.endswith("premiumIndex"):
            return [
                {"symbol": "BTCUSDT", "lastFundingRate": "0.0001"},
                {"symbol": "ETHUSDT", "lastFundingRate": "0.0002"},
            ]
        return {"openInterest": "123.5"}

    c = BinanceClient()
    c._request = fake_request  # type: ignore[method-assign]

    async def run():
        for _ in range(3):
            assert (await c.funding_rates(["BTCUSDT"]))["BTCUSDT"] == pytest.approx(0.0001)
            assert (await c.open_interest(["BTCUSDT"]))["BTCUSDT"] == pytest.approx(123.5)
        assert calls.count("/fapi/v1/premiumIndex") == 1
        assert calls.count("/fapi/v1/openInterest") == 1
        # TTL 过期后重新拉一次
        c._funding_ts -= 120
        c._oi.clear()
        await c.funding_rates(["BTCUSDT"])
        await c.open_interest(["BTCUSDT"])
        assert calls.count("/fapi/v1/premiumIndex") == 2
        assert calls.count("/fapi/v1/openInterest") == 2

    asyncio.run(run())


def test_flat_signals_are_deduped_with_repeat_count():
    from nanoquant.db import init_db, session_scope
    from nanoquant.engine.worker import TradingEngine
    from nanoquant.models import Signal

    init_db()
    engine = TradingEngine()
    ticker = SimpleNamespace(last=100.0)

    def sig(direction="flat", blocked="波动率过低"):
        return SimpleNamespace(
            symbol="TESTUSDT", direction=direction, score=0.12,
            features={"atr_pct": 0.5}, blocked=blocked,
        )

    first = engine._record_signal(sig(), ticker, "scan")["signal_id"]
    second = engine._record_signal(sig(), ticker, "scan")["signal_id"]
    assert first == second
    with session_scope() as s:
        assert s.get(Signal, first).repeats == 2

    # 拒绝原因变化 → 新行；非 flat 信号永远单独成行，避免影响后续裁决更新
    other = engine._record_signal(sig(blocked="量比不足"), ticker, "scan")["signal_id"]
    long1 = engine._record_signal(sig(direction="long", blocked=""), ticker, "scan")["signal_id"]
    long2 = engine._record_signal(sig(direction="long", blocked=""), ticker, "scan")["signal_id"]
    assert other != first
    assert long1 != long2 and long1 not in {first, other}

    with session_scope() as s:
        for row in s.query(Signal).filter(Signal.symbol == "TESTUSDT").all():
            s.delete(row)


# ------------------------------------------------------------ 心法效果对照
def _trade(pnl: float, *, ai_used=True, ids=None, features=None):
    feat = {} if features is None else dict(features)
    if ai_used is not None:
        feat["ai_used"] = ai_used
    if ids is not None:
        feat["injected_mantra_ids"] = ids
    return {"pnl": pnl, "features": feat}


def _clean_mantras():
    from nanoquant.db import init_db, session_scope
    from nanoquant.models import Mantra

    init_db()
    with session_scope() as s:
        s.query(Mantra).delete()


def test_mantra_replaces_similar_version_after_cooldown():
    """语义重复时新版本必须替代旧版本，而不是无限 merged 出 25 条并列心法."""
    import time

    from nanoquant.db import session_scope
    from nanoquant.evolution import mantra as store
    from nanoquant.models import Mantra

    _clean_mantras()
    rule = "当量比低于 0.9 且 ATR 占比低于 0.25% 时不开仓，等待量能放大后再介入。"
    first = store.upsert_lesson(title="低量能追单易被止损", rule=rule, category="entry", confidence=0.55)
    assert first["action"] == "created"
    old_id = first["mantra"]["id"]

    # 冷却期内（刚建、hits=0）先合并，避免同一晚反复翻烧饼
    second = store.upsert_lesson(
        title="低量能追单容易被止损", rule=rule, category="entry", confidence=0.6,
    )
    assert second["action"] == "merged"

    # 过了冷却且被验证过：相似度达标的新版本直接替代
    with session_scope() as s:
        row = s.get(Mantra, old_id)
        row.created_at = int(time.time()) - 7 * 3600
        row.hits = 2
    third = store.upsert_lesson(
        title="低量能追单易被止损",
        rule=rule + "连续两根放量确认后再入场。",
        category="entry",
        confidence=0.62,
    )
    assert third["action"] == "replaced", third
    assert third["replaced_id"] == old_id
    assert third["mantra"]["version"] == 2
    with session_scope() as s:
        old = s.get(Mantra, old_id)
        assert old.enabled is False and old.superseded_by == third["mantra"]["id"]
        assert old.evidence.get("superseded_reason")
    # 被替代的不再进入注入排名
    assert all(m["id"] != old_id for m in store.rank_lessons(20))

    # 复盘结束后归档旧版本（保留审计链，绝不物理删除）
    assert store.purge_superseded() == 1
    assert store.purge_superseded() == 0  # 幂等
    with session_scope() as s:
        old = s.get(Mantra, old_id)
        assert old is not None
        assert old.enabled is False
        assert "archived_reason" in (old.evidence or {})


def test_mantra_ai_replaces_respects_cooldown_gate():
    """AI 点名 replaces 也不再绕过冷却/置信度门槛；过门槛后才允许跨分类替代."""
    import time

    from nanoquant.db import session_scope
    from nanoquant.evolution import mantra as store
    from nanoquant.models import Mantra

    _clean_mantras()
    first = store.upsert_lesson(
        title="多空信号冲突时观望",
        rule="当方向分与量能方向相反时不开仓，等待结构确认。",
        category="entry",
        confidence=0.55,
    )
    assert first["action"] == "created"
    old_id = first["mantra"]["id"]
    fresh = store.upsert_lesson(
        title="指标互相矛盾时先观望",
        rule="当趋势分与动量分符号相反时等待一根 K 线收盘确认。",
        category="risk",
        confidence=0.6,
        replaces="多空信号冲突时观望",
    )
    # 刚入库且 hits=0：模型一句话不能直接换掉既有心法
    assert fresh["action"] != "replaced", fresh

    _clean_mantras()
    again = store.upsert_lesson(
        title="多空信号冲突时观望",
        rule="当方向分与量能方向相反时不开仓，等待结构确认。",
        category="entry",
        confidence=0.55,
    )
    with session_scope() as s:
        row = s.get(Mantra, again["mantra"]["id"])
        row.created_at = int(time.time()) - 7 * 3600
        row.hits = 2
    second = store.upsert_lesson(
        title="指标互相矛盾时先观望",
        rule="当趋势分与动量分符号相反时等待一根 K 线收盘确认。",
        category="risk",
        confidence=0.6,
        replaces="多空信号冲突时观望",
    )
    assert second["action"] == "replaced", second
    assert second["replaced_id"] == again["mantra"]["id"]


def test_mantra_dedupe_collapses_existing_duplicates():
    """修复前积累的重复心法：保留置信度最高的一条，其余标记被替代."""
    import time

    from nanoquant.db import session_scope
    from nanoquant.evolution import mantra as store
    from nanoquant.models import Mantra

    _clean_mantras()
    now = int(time.time())
    expires = now + 7 * 86400
    rows = [
        ("低量能追单易被止损", "当量比低于 0.9 且 ATR 占比低于 0.25% 时不开仓，等待量能放大后再介入。", 0.5),
        ("低量能追单容易被止损", "当量比低于 0.9 且 ATR 占比低于 0.25% 时不开仓，等待量能放大后再介入。", 0.78),
        ("超时离场仍盈利", "当持仓超时且浮盈仍大于 1% 时，下次把止盈目标下调到 0.8%。", 0.6),
    ]
    with session_scope() as s:
        for title, rule, conf in rows:
            s.add(
                Mantra(
                    title=title, rule=rule, category="entry", source="test",
                    confidence=conf, health=conf, enabled=True, version=1,
                    superseded_by=0, evidence={}, created_at=now, updated_at=now,
                    expires_at=expires,
                )
            )

    out = store.dedupe()
    assert out["ok"] and len(out["replaced"]) == 1
    assert out["replaced"][0]["kept_title"] == "低量能追单容易被止损"  # 保留置信度最高
    assert out["kept"] == 2
    active = store.list_mantras(include_disabled=False, limit=20)
    assert {m["title"] for m in active} == {"低量能追单容易被止损", "超时离场仍盈利"}
    with session_scope() as s:
        assert s.query(Mantra).filter(Mantra.superseded_by != 0).count() == 1


def test_purge_superseded_archives_old_versions():
    """每轮复盘结束后：被替代的旧心法归档停用留痕，绝不物理删除."""
    import time

    from nanoquant.db import session_scope
    from nanoquant.evolution import mantra as store
    from nanoquant.models import Mantra

    _clean_mantras()
    now = int(time.time())
    with session_scope() as s:
        keep = Mantra(
            title="正常生效心法", rule="当量比低于 0.9 时不开仓，等待确认。", category="entry",
            source="test", confidence=0.6, health=0.6, enabled=True, version=1,
            superseded_by=0, evidence={}, created_at=now, updated_at=now, expires_at=now + 86400,
        )
        s.add(keep)
        s.flush()
        keep_id = keep.id
        s.add(
            Mantra(
                title="旧版本心法", rule="当量比低于 0.9 时不开仓，等待量能放大。", category="entry",
                source="test", confidence=0.55, health=0.55, enabled=False, version=1,
                superseded_by=keep_id, evidence={"superseded_reason": "test"},
                created_at=now, updated_at=now, expires_at=now + 86400,
            )
        )
        s.add(
            Mantra(
                title="手动停用心法", rule="当 ATR 占比低于 0.2% 时降低仓位。", category="risk",
                source="test", confidence=0.5, health=0.5, enabled=False, version=1,
                superseded_by=0, evidence={}, created_at=now, updated_at=now, expires_at=now + 86400,
            )
        )

    assert store.purge_superseded() == 1
    assert store.purge_superseded() == 0  # 幂等
    with session_scope() as s:
        titles = {m.title for m in s.query(Mantra).all()}
        old = next(m for m in s.query(Mantra).all() if m.title == "旧版本心法")
        assert old.enabled is False
        assert "archived_reason" in (old.evidence or {})
    assert titles == {"正常生效心法", "手动停用心法", "旧版本心法"}


def test_ai_dedupe_merges_semantically_similar(monkeypatch):
    """存量去重优先交给 AI：它按语义分组，规则只做兜底."""
    import asyncio
    from types import SimpleNamespace

    from nanoquant.db import session_scope
    from nanoquant.evolution import review as evolution_review
    from nanoquant.models import Mantra

    _clean_mantras()
    now = int(__import__("time").time())
    with session_scope() as s:
        ids = []
        for title, rule, conf in (
            ("低分信号不追单", "当方向分低于 0.45 且量比低于 0.9 时不开仓，等待确认。", 0.72),
            ("弱势信号先观望", "当方向分不足 0.45 且量比不足 0.9 时保持观望，等结构确认。", 0.58),
            ("超时离场止盈过远", "当持仓超时而浮盈仍大于 1% 时，下次把止盈下调到 0.8%。", 0.6),
        ):
            row = Mantra(
                title=title, rule=rule, category="entry", source="test",
                confidence=conf, health=conf, enabled=True, version=1,
                superseded_by=0, evidence={}, created_at=now, updated_at=now,
                expires_at=now + 7 * 86400,
            )
            s.add(row)
            s.flush()
            ids.append(row.id)
    keep_id, old_id, other_id = ids

    async def fake_chat(*args, **kwargs):  # noqa: ARG001
        return SimpleNamespace(
            ok=True,
            text=json.dumps({"merge": [{"keep": keep_id, "replaced": [old_id], "reason": "同一主题不同措辞"}]}),
        )

    monkeypatch.setattr(evolution_review.ai_client, "chat_with_fallback", fake_chat)
    monkeypatch.setattr(
        evolution_review, "get_ai",
        lambda: SimpleNamespace(enabled=True, provider_id=0, fallback_provider_ids=[]),
    )
    out = asyncio.run(evolution_review.dedupe_mantras(use_ai=True))
    assert out["mode"] == "ai" and len(out["replaced"]) == 1
    assert out["replaced"][0]["id"] == old_id
    with session_scope() as s:
        assert s.get(Mantra, old_id).enabled is False
        assert s.get(Mantra, old_id).superseded_by == keep_id
        assert s.get(Mantra, other_id).enabled is True


def test_effectiveness_splits_by_injection_snapshot():
    from nanoquant.evolution.effectiveness import compare_groups

    rows = [
        _trade(2.0, ids=[1, 2]),
        _trade(-1.0, ids=[1]),
        _trade(1.0, ids=[]),
        _trade(-2.0),  # AI 参与但没注入
        _trade(5.0, ai_used=False),
        _trade(1.0, ai_used=None),  # 旧数据: legacy
        {"pnl": 0.5, "features": None},
    ]
    out = compare_groups(rows)
    assert out["injected"]["trades"] == 2
    assert out["control"]["trades"] == 2
    assert out["ai_off"]["trades"] == 1
    assert out["legacy"]["trades"] == 2
    assert out["per_mantra"][0]["id"] == 1 and out["per_mantra"][0]["trades"] == 2
    assert out["delta_win_rate_pct"] == 0.0  # 两组各 50%
    assert out["sample_ok"] is False
    assert "随机 A/B" in out["note"]


def test_effectiveness_needs_min_sample_and_reports_delta():
    from nanoquant.evolution.effectiveness import MIN_SAMPLE, compare_groups

    rows = [_trade(1.0, ids=[7]) for _ in range(MIN_SAMPLE)]
    rows += [_trade(-1.0, ids=[]) for _ in range(MIN_SAMPLE)]
    out = compare_groups(rows)
    assert out["sample_ok"] is True
    assert out["delta_win_rate_pct"] == 100.0
    assert out["delta_avg_pnl"] == 2.0
    assert "更高" in out["verdict"]


# ------------------------------------------------------ 组合风险与波动率目标
def test_side_cap_reason_counts_same_direction():
    from nanoquant.engine.risk import side_cap_reason

    positions = [SimpleNamespace(side="long"), SimpleNamespace(side="short")]
    assert side_cap_reason(positions, "long", 0) == ""  # 0=不限制, 保持旧行为
    assert side_cap_reason(positions, "long", 2) == ""
    assert "同方向" in side_cap_reason(positions, "long", 1)
    assert "同方向" in side_cap_reason(positions, "short", 1)
    assert side_cap_reason(positions, "", 1) == ""  # 没有方向信息时不做判断


def test_vol_risk_scale_clamps_and_disables():
    from nanoquant.engine.risk import vol_risk_scale

    assert vol_risk_scale(0.0, 1.0) == 1.0  # 关闭
    assert vol_risk_scale(1.5, 0.0) == 1.0  # ATR 无效
    assert vol_risk_scale(1.5, 1.5) == 1.0
    assert vol_risk_scale(1.5, 3.0) == pytest.approx(0.5)  # 高波动缩仓
    assert vol_risk_scale(1.0, 0.1) == pytest.approx(2.0)  # 放大上限
    assert vol_risk_scale(1.0, 10.0) == pytest.approx(0.25)  # 缩小下限


def test_estimate_qty_applies_risk_scale():
    from nanoquant.exchange.broker import estimate_qty
    from nanoquant.exchange.precision import SymbolFilters
    from nanoquant.services.settings import RiskSettings

    cfg = RiskSettings(risk_per_trade_pct=1.0, margin_pct=50.0, default_leverage=20)
    filters = SymbolFilters(symbol="TESTUSDT", qty_step=0.001, min_qty=0.001, min_notional=5.0)

    full, _ = estimate_qty(1000.0, 100.0, 20, cfg, 0.01, filters)
    half, _ = estimate_qty(1000.0, 100.0, 20, cfg, 0.01, filters, risk_scale=0.5)
    assert full == pytest.approx(10.0)  # 权益1%÷止损1% = 1000 名义额 → 10 个
    assert half == pytest.approx(5.0)


# ------------------------------------------------------------ 回测保真度
def _flat(n: int, price: float = 100.0, start_ts: int = 0, step_ms: int = 300_000):
    return [bar(o=price, h=price, l=price, c=price, v=1e6, t=start_ts + i * step_ms) for i in range(n)]


def _force_signal(monkeypatch, direction: str = "long", sl_pct: float = 1.0) -> None:
    import nanoquant.backtest.engine as bt

    monkeypatch.setattr(bt, "build_features", lambda *a, **k: None)
    monkeypatch.setattr(
        bt,
        "score_signal",
        lambda *a, **k: SimpleNamespace(direction=direction, score=0.9, sl_pct=sl_pct, tp_pct=0.0),
    )


def test_funding_crossings_counts_8h_boundaries():
    from nanoquant.backtest.engine import FUNDING_INTERVAL_MS as F
    from nanoquant.backtest.engine import funding_crossings

    assert F == 8 * 3600 * 1000
    assert funding_crossings(0, F) == 1
    assert funding_crossings(F, F) == 0
    assert funding_crossings(F - 1, F + 1) == 1
    assert funding_crossings(0, 3 * F) == 3


def test_funding_charges_long_and_pays_short(monkeypatch):
    """资金费必须真的扣/收: 多头付、空头收, 且只按跨过的结算点计次."""
    from nanoquant.backtest.engine import FUNDING_INTERVAL_MS as F
    from nanoquant.backtest.engine import run_backtest

    step = 300_000
    start = F - 61 * step  # candles[61].open_time == 结算点
    candles = _flat(64, start_ts=start, step_ms=step)

    def run(side: str, rate: float) -> float:
        _force_signal(monkeypatch, direction=side)
        res = run_backtest(
            candles,
            StrategySettings(roi_table=[], trailing_pct=0.0, max_hold_minutes=5),
            fee_bps=0.0,
            slippage_bps=0.0,
            spread_bps=0.0,
            funding_bps_8h=rate,
            risk_per_trade_pct=1.0,
            margin_pct=50.0,
            leverage=20.0,
            max_bars_hold=1,
        )
        return float(res.summary["funding_total"])

    # 单笔 notional = 权益1% / 止损1% = 100 → 10bps = 0.1
    assert run("long", 0.0) == 0.0
    assert run("long", 10.0) == pytest.approx(-0.1, abs=1e-9)
    assert run("short", 10.0) == pytest.approx(0.1, abs=1e-9)


def test_partial_tp_books_half_and_moves_stop_to_breakeven(monkeypatch):
    from nanoquant.backtest.engine import run_backtest

    _force_signal(monkeypatch, direction="long")
    candles = _flat(64)
    # 开仓价 100, 止损 99; 第 61 根冲到 102 (1R=101) → 平一半; 第 62 根回到 100 → 保本止损
    candles[61] = bar(o=100, h=102, l=100.2, c=101, v=1e6, t=61 * 300_000)
    candles[62] = bar(o=101, h=101, l=99.5, c=100, v=1e6, t=62 * 300_000)

    res = run_backtest(
        candles,
        StrategySettings(roi_table=[], trailing_pct=0.0),
        fee_bps=0.0,
        slippage_bps=0.0,
        spread_bps=0.0,
        funding_bps_8h=0.0,
        risk_per_trade_pct=1.0,
    )
    assert res.summary["partial_tp_count"] == 1
    trade = res.trades[0]
    assert trade.partial_tp is True
    assert trade.reason == "stop_loss"  # 剩余仓位在保本位被打掉
    assert trade.qty == pytest.approx(1.0)  # 明细按初始仓位记
    assert trade.pnl == pytest.approx(0.5, abs=1e-6)  # (101-100)*0.5

    # 关掉部分止盈后, 同一段行情只会在 101 之前被原止损 99 打掉, 不该有 partial
    res2 = run_backtest(
        candles,
        StrategySettings(roi_table=[], trailing_pct=0.0, partial_tp_at_r=0.0),
        fee_bps=0.0,
        slippage_bps=0.0,
        funding_bps_8h=0.0,
        risk_per_trade_pct=1.0,
    )
    assert res2.summary["partial_tp_count"] == 0


def test_backtest_spread_is_a_round_trip_cost():
    """点差按往返计: 同样的行情, 加点差后收益必须更低."""
    from nanoquant.backtest.engine import run_backtest

    candles = []
    price = 100.0
    for i in range(300):
        price *= 1.0008 if i % 4 else 0.9995
        candles.append(bar(o=price, h=price * 1.001, l=price * 0.999, c=price, v=1000.0, t=i * 300_000))

    common = dict(symbol="TESTUSDT", fee_bps=0.0, slippage_bps=0.0, funding_bps_8h=0.0)
    clean = run_backtest(candles, StrategySettings(), **common)
    costly = run_backtest(candles, StrategySettings(), spread_bps=10.0, **common)
    assert clean.summary["trades"] and costly.summary["trades"]
    assert costly.summary["total_return_pct"] < clean.summary["total_return_pct"]


# ------------------------------------------------------------ 样本外验证
def test_walkforward_reports_overfit_gap():
    from nanoquant.backtest.walkforward import run_walkforward

    candles = []
    price = 100.0
    for i in range(420):
        price *= 1.0006 if i % 3 else 0.9997
        candles.append(bar(o=price, h=price * 1.001, l=price * 0.999, c=price, v=1000.0, t=i * 300_000))

    result = run_walkforward(
        candles,
        StrategySettings(),
        symbol="TESTUSDT",
        grid={"entry_threshold": [0.2, 0.4]},
        folds=2,
        train_bars=200,
    )
    assert result["folds"] == 2
    assert result["requested_folds"] == 2
    assert len(result["items"]) == 2
    for item in result["items"]:
        assert set(item["params"]) == {"entry_threshold"}
        assert "total_return_pct" in item["out_sample"]
    aggregate = result["aggregate"]
    assert {"oos_return_pct", "overfit_gap_pct", "consistency", "positive_folds"} <= set(aggregate)


def test_walkforward_rejects_short_history():
    from nanoquant.backtest.walkforward import run_walkforward

    result = run_walkforward(_flat(100), StrategySettings(), grid={"entry_threshold": [0.3]}, folds=2)
    assert "error" in result


# ------------------------------------------------------------ 执行 sidecar 协议
def test_exec_sidecar_signs_over_reused_connection():
    """协议: health / plan 都要能走同一条连接 (每单重建连接会把握手塞进签名路径)."""
    import asyncio
    import json

    from nanoquant.exchange.client import sign_query
    from nanoquant.exchange.exec_client import ExecSidecar

    secret = "unit-test-hmac-secret"
    query = "symbol=BTCUSDT&side=BUY&type=MARKET&timestamp=1700000000000"
    accepted = {"n": 0}

    async def handler(reader, writer):
        accepted["n"] += 1
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
                req = json.loads(line.decode())
                if req["op"] == "health":
                    resp = {"id": req["id"], "ok": True, "algo": "hmac-sha256"}
                else:
                    resp = {
                        "id": req["id"],
                        "ok": True,
                        "algo": "hmac-sha256",
                        "signature": sign_query(req["query"], secret),
                        "query": req["query"],
                    }
                writer.write((json.dumps(resp) + "\n").encode())
                await writer.drain()
        finally:
            writer.close()

    async def run():
        server = await asyncio.start_server(handler, "127.0.0.1", 0)
        port = server.sockets[0].getsockname()[1]
        sidecar = ExecSidecar(f"tcp:127.0.0.1:{port}", timeout=2)
        try:
            health = await sidecar.health()
            assert health["algo"] == "hmac-sha256"
            first = await sidecar.sign(query)
            second = await sidecar.sign(query)
            assert first["signature"] == sign_query(query, secret) == second["signature"]
            assert first["query"] == query
            assert sidecar.counters["connects"] == 1
        finally:
            await sidecar.close()
            server.close()
            await server.wait_closed()

    asyncio.run(run())


def test_exec_sidecar_unreachable_raises():
    import asyncio

    from nanoquant.exchange.exec_client import ExecSidecar, ExecSidecarError

    async def run():
        # 9 端口在本机恒不可用: 只验证"连接失败要有明确错误", 不依赖外部网络
        sidecar = ExecSidecar("tcp:127.0.0.1:9", timeout=0.3)
        try:
            await sidecar.sign("a=1")
        except ExecSidecarError as exc:
            assert "sidecar" in str(exc)
        else:  # pragma: no cover - 极少数环境 9 端口可写
            raise AssertionError("不可达地址竟然成功")
        assert sidecar.counters["errors"] >= 1
        assert sidecar.connected is False

    asyncio.run(run())


# ------------------------------------------------------------ 影子比对判定
class _FakeSidecar:
    """最小 sidecar 替身: 只回一个签名, 其余属性对齐 ExecSidecar."""

    def __init__(self, signature: str, *, query_echo: bool = True) -> None:
        self.signature = signature
        self.query_echo = query_echo
        self.timeout = 1.0
        self.target = "tcp:fake"
        self.connected = True
        self.algo = "hmac-sha256"
        self.last_error = ""
        self.counters = {"requests": 0, "errors": 0, "connects": 0, "timeouts": 0}

    def reconfigure(self, target, timeout=None):  # noqa: ARG002
        return None

    async def sign(self, query: str) -> dict:
        self.counters["requests"] += 1
        return {
            "ok": True,
            "algo": self.algo,
            "query": query if self.query_echo else query + "x",
            "signature": self.signature,
        }


def _shadow(client, mode: str):
    from nanoquant.services.exec_shadow import ExecShadow, ShadowConfig

    return ExecShadow(
        client=client,
        config_provider=lambda: ShadowConfig(mode=mode, target="", sample_pct=100, timeout_ms=300),
        notifier=lambda *_: None,
    )


def test_exec_shadow_enforce_blocks_mismatch():
    import asyncio

    from nanoquant.services.exec_shadow import ExecShadowError

    fake = _FakeSidecar("not-the-local-signature")
    shadow = _shadow(fake, "enforce")

    async def run():
        try:
            await shadow.observe("a=1", "local-signature")
        except ExecShadowError:
            return "blocked"
        return "passed"

    assert asyncio.run(run()) == "blocked"
    assert shadow.counts["mismatched"] == 1
    assert shadow.last_mismatch and shadow.last_mismatch["echo_ok"] is True
    assert "签名不一致" in shadow.status()["verdict"]


def test_exec_shadow_enforce_accepts_match_and_ignores_echo_bug():
    import asyncio

    fake = _FakeSidecar("same")
    shadow = _shadow(fake, "enforce")

    assert asyncio.run(shadow.observe("a=1", "same")) is None
    assert shadow.counts["matched"] == 1

    # 签名一致但 query 回显被改写: 说明协议层串了包, 同样视为不一致
    from nanoquant.services.exec_shadow import ExecShadowError

    fake2 = _FakeSidecar("same", query_echo=False)
    shadow2 = _shadow(fake2, "enforce")
    with pytest.raises(ExecShadowError):
        asyncio.run(shadow2.observe("a=1", "same"))
    assert shadow2.counts["mismatched"] == 1


def test_exec_shadow_shadow_mode_is_async_and_never_raises():
    import asyncio

    fake = _FakeSidecar("mismatch")
    shadow = _shadow(fake, "shadow")

    async def run():
        await shadow.observe("a=1", "local")
        assert shadow.counts["inflight"] == 1
        await asyncio.sleep(0.05)

    asyncio.run(run())
    assert shadow.counts["matched"] == 0
    assert shadow.counts["mismatched"] == 1
    assert shadow.counts["inflight"] == 0
    assert shadow.counts["skipped"] == 0


def test_exec_shadow_verdict_degrades_when_sidecar_down():
    fake = _FakeSidecar("x")
    fake.connected = False
    shadow = _shadow(fake, "shadow")
    # 历史一致过, 但现在连不上: 判据必须是"当前不可用", 不能继续显示"一致"
    shadow.counts["matched"] = 3
    shadow.counts["errors"] = 5
    shadow.last_error = "sidecar 响应超时(800ms)"

    verdict = shadow.status()["verdict"]
    assert "不可用" in verdict
    assert "降级" in verdict


def test_exec_shadow_latency_uses_client_roundtrip():
    """延迟口径必须是 sidecar 往返, 不能把排队时间算进去 (否则宕机时 p99 假性飙高)."""
    import asyncio

    fake = _FakeSidecar("same")
    fake.last_ms = 0.25
    shadow = _shadow(fake, "enforce")

    asyncio.run(shadow.observe("a=1", "same"))
    assert shadow.status()["latency_ms"]["p50"] == pytest.approx(0.25, abs=0.001)


def test_exec_shadow_off_skips_everything():
    import asyncio

    fake = _FakeSidecar("whatever")
    shadow = _shadow(fake, "off")

    asyncio.run(shadow.observe("a=1", "local"))
    assert fake.counters["requests"] == 0
    assert shadow.counts["matched"] == 0 and shadow.counts["mismatched"] == 0
    assert "未启用" in shadow.status()["verdict"]


# ---------------------------------------------- 外部信号跟单 (RUNBOOK 管道)
def test_parse_signal_normalizes_templates_and_events():
    from nanoquant.services import signal_follow as sf

    parsed = sf.parse_signal({
        "id": "msg-42", "symbol": "btc/usdt", "side": "开多", "event": "开仓",
        "leverage": "20x", "entry": "1,234.5", "stop": 1200, "tp": 1300,
        "ts": 1730000000000,
    })
    assert parsed["symbol"] == "BTCUSDT" and parsed["side"] == "long"
    assert parsed["leverage"] == 20.0 and parsed["entry"] == 1234.5
    assert parsed["ts"] == 1730000000 and parsed["event"] == "open"

    assert sf.classify_event({"event": "平仓"}) == "exit"
    assert sf.classify_event({"event": "止盈更新"}) == "exit"
    assert sf.classify_event({"action": "buy"}) == "open"
    assert sf.classify_event({"symbol": "BTCUSDT"}) == ""  # 模板变体: 无事件字段
    assert sf.classify_event({"event": "something_odd"}) == "unknown"
    assert sf.parse_signal({"symbol": "BTCUSDT", "action": "平多"}) is None

    from datetime import datetime

    iso = sf.parse_signal({"symbol": "ETHUSDT", "side": "short", "ts": "2026-11-10T10:00:00+00:00"})
    assert iso["ts"] == int(datetime.fromisoformat("2026-11-10T10:00:00+00:00").timestamp())
    assert sf.signal_identity(parsed) == "msg-42:BTCUSDT:long"


def test_follow_leverage_adds_bonus_and_obeys_system_cap():
    from nanoquant.engine.risk import choose_leverage
    from nanoquant.exchange.precision import SymbolFilters
    from nanoquant.services import signal_follow as sf
    from nanoquant.services.settings import EngineSettings, RiskSettings, SignalFollowSettings

    cfg = SignalFollowSettings()
    assert cfg.leverage_bonus == 10 and cfg.auto_add_symbol and cfg.dry_run
    assert cfg.poll_sec == 1  # 本地文件增量读, 不受 Telegram 轮询 10s 下限约束
    assert sf.follow_leverage(20.0, 5.0, cfg.leverage_bonus) == 30
    assert sf.follow_leverage(0.0, 5.0, cfg.leverage_bonus) == 5  # 缺杠杆不凭空 +10

    engine = EngineSettings()
    risk = RiskSettings(default_leverage=5, max_leverage=25)
    f25 = SymbolFilters(symbol="BTCUSDT", max_leverage=50, price_tick=0.01, qty_step=0.001)
    base = sf.follow_leverage(20.0, risk.default_leverage, cfg.leverage_bonus)
    assert choose_leverage("BTCUSDT", engine, risk, f25, ai_leverage=float(base)) == 25
    f12 = SymbolFilters(symbol="BTCUSDT", max_leverage=12, price_tick=0.01, qty_step=0.001)
    assert choose_leverage("BTCUSDT", engine, risk, f12, ai_leverage=float(base)) == 12


def test_read_new_lines_incremental_partial_and_truncated(tmp_path):
    import json
    import os
    import time

    from nanoquant.services import signal_follow as sf

    path = tmp_path / "signals.jsonl"
    path.write_text(json.dumps({"id": 1}) + "\n" + json.dumps({"id": 2}) + "\n", encoding="utf-8")
    rows, offset = sf.read_new_lines(str(path), 0)
    assert [r["id"] for r in rows] == [1, 2] and offset == path.stat().st_size

    with open(path, "a", encoding="utf-8") as fh:
        fh.write(json.dumps({"id": 3}))  # 半行不消费
    assert sf.read_new_lines(str(path), offset) == ([], offset)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write("\n" + json.dumps({"id": 4}) + "\n")
    rows, offset = sf.read_new_lines(str(path), offset)
    assert [r["id"] for r in rows] == [3, 4]

    path.write_text(json.dumps({"id": 9}) + "\n", encoding="utf-8")  # --reset 截断
    rows, offset = sf.read_new_lines(str(path), offset)
    assert [r["id"] for r in rows] == [9] and offset == path.stat().st_size

    root = tmp_path / "ao-monitor" / "data"
    (root / "chan_a").mkdir(parents=True)
    (root / "chan_b").mkdir(parents=True)
    a, b = root / "chan_a" / "signals.jsonl", root / "chan_b" / "signals.jsonl"
    a.write_text("{}\n", encoding="utf-8")
    b.write_text("{}\n", encoding="utf-8")
    os.utime(str(b), (time.time() + 10, time.time() + 10))
    import nanoquant.services.signal_follow as module

    assert module._ENV_ROOT
    from pytest import MonkeyPatch

    mp = MonkeyPatch()
    try:
        mp.delenv(sf._ENV_PATH, raising=False)
        mp.setenv(sf._ENV_ROOT, str(root))
        assert sf.resolve_path("", str(tmp_path / "nanoquant")) == str(b)
        assert sf.resolve_path(str(root / "chan_a"), str(tmp_path)) == str(a)
    finally:
        mp.undo()


def test_margin_inflight_reservation_survives_engine_cycle():
    import asyncio

    from nanoquant.engine.margin import MarginLedger

    async def run():
        ledger = MarginLedger()
        ledger.begin_cycle(1000.0)
        rid = await ledger.reserve_tracked(400.0)
        assert rid > 0 and ledger.inflight == 400.0
        ledger.begin_cycle(1000.0)  # 引擎下一轮快照尚未扣到跟仓在途单
        assert ledger.remaining == 600.0
        assert await ledger.reserve_tracked(700.0) == 0  # 轮询开仓不能重复占额度
        await ledger.settle(rid, opened=False)
        assert ledger.remaining == 1000.0

        rid2 = await ledger.reserve_tracked(300.0)
        ledger.refresh(700.0)  # 新快照已扣掉另一笔成交
        assert ledger.inflight == 300.0
        await ledger.settle(rid2, opened=True)
        assert ledger.used == 300.0 and ledger.remaining == 400.0

    asyncio.run(run())


# ------------------------------------------- 标的池：股票/商品类永续（TRADIFI）
def test_pool_accepts_tradifi_equity_contract():
    """币安股票合约是 TRADIFI_PERPETUAL（underlyingType=EQUITY），旧逻辑只认 PERPETUAL 会全拒."""
    import pytest

    from nanoquant.exchange.precision import parse_exchange_info
    from nanoquant.services import symbols as pool

    equity = {
        "symbol": "AAPLUSDT",
        "baseAsset": "AAPL",
        "quoteAsset": "USDT",
        "marginAsset": "USDT",
        "contractType": "TRADIFI_PERPETUAL",
        "status": "TRADING",
        "underlyingType": "EQUITY",
        "filters": [
            {"filterType": "LOT_SIZE", "stepSize": "0.01", "minQty": "0.01"},
            {"filterType": "PRICE_FILTER", "tickSize": "0.01000"},
            {"filterType": "MIN_NOTIONAL", "notional": "5"},
        ],
    }
    pool._validate_contract(equity, "AAPLUSDT")  # 不抛即通过
    row = pool.build_pool_row(equity)
    assert row["symbol"] == "AAPLUSDT" and row["name"] == "AAPL"
    assert row["precision"] == 2 and row["min_notional"] == 5.0

    # 休市（BREAK）允许先入池
    pool._validate_contract({**equity, "status": "BREAK"}, "AAPLUSDT")

    # 交割合约与异常状态仍然拒绝
    with pytest.raises(pool.PoolError):
        pool._validate_contract({**equity, "contractType": "CURRENT_QUARTER"}, "AAPLUSDT")
    with pytest.raises(pool.PoolError):
        pool._validate_contract({**equity, "status": "SETTLING"}, "AAPLUSDT")

    # 当前环境没有该合约：提示模拟盘只上线部分股票合约
    with pytest.raises(pool.PoolError) as exc:
        pool._validate_contract({}, "AAPLUSDT")
    assert "模拟盘" in str(exc.value)

    # 行情精度表同样纳入 TRADIFI 与休市状态
    parsed = parse_exchange_info(
        {"symbols": [equity, {**equity, "symbol": "TSLAUSDT", "status": "BREAK"}]}
    )
    assert {"AAPLUSDT", "TSLAUSDT"} <= set(parsed)


def test_candidates_marks_equity_contracts(monkeypatch):
    import asyncio

    from nanoquant.services import symbols as pool

    rows = {
        "AAPLUSDT": {
            "symbol": "AAPLUSDT", "baseAsset": "AAPL", "quoteAsset": "USDT",
            "contractType": "TRADIFI_PERPETUAL", "status": "TRADING", "underlyingType": "EQUITY",
            "filters": [{"filterType": "LOT_SIZE", "stepSize": "0.01", "minQty": "0.01"}],
        }
    }

    async def fake_symbols(force: bool = False):  # noqa: ARG001
        return rows

    monkeypatch.setattr(pool.binance_client, "exchange_symbols", fake_symbols)
    out = asyncio.run(pool.candidates("AAPL"))
    assert len(out) == 1
    assert out[0]["symbol"] == "AAPLUSDT"
    assert out[0]["is_equity"] is True
    assert out[0]["contract_type"] == "TRADIFI_PERPETUAL"


# ------------------------------------------- 自进化：宪法 Linter / 证据健康分
def test_guard_rejects_single_event_and_risk_tampering():
    from nanoquant.evolution.guard import audit

    good = "当量比低于 0.9 且 ATR 占比低于 0.25% 时不开仓，等待量能放大后再介入。"
    assert audit("低量能不追单", good, "entry", 0.6)[0] is True
    # 单笔偶发事件不得写入长期心法
    ok, why = audit("低量能不追单", good, "entry", 0.6, sample_size=1, min_sample=2)
    assert not ok and "样本量不足" in why
    assert audit("低量能不追单", good, "entry", 0.6, sample_size=2, min_sample=2)[0] is True
    # 变更动词 × 风险名词共现兜底
    assert audit("提高杠杆", "当胜率回升时应提高杠杆以放大收益。", "position", 0.6)[0] is False
    assert audit("放宽止损", "当波动放大时应放宽止损以避免被扫。", "risk", 0.6)[0] is False
    # 禁止式表述属安全知识，不得误杀
    assert audit("严禁放宽止损", "当波动放大时严禁放宽止损，避免扛单。", "risk", 0.6)[0] is True
    # 硬编码绝对资金金额
    assert audit("固定仓位", "当信号分大于 0.5 时固定使用 500 USDT 名义额开仓。", "position", 0.6)[0] is False
    # 不可评估因子
    assert audit("动能失效", "当 velocity 转负且 ATR 抬升时不开仓。", "entry", 0.6)[0] is False


def test_health_scores_by_cohort_and_needs_min_sample():
    from nanoquant.evolution import health

    rows = [
        {"symbol": "BTCUSDT", "side": "long", "pnl": 2.0, "exit_reason": "roi", "observability": "FULL"},
        {"symbol": "BTCUSDT", "side": "long", "pnl": 2.0, "exit_reason": "roi", "observability": "FULL"},
        {"symbol": "ETHUSDT", "side": "short", "pnl": -1.0, "exit_reason": "trailing_stop", "observability": "FULL"},
    ] * 4
    scored = health.score_lesson_against_trades(
        "当 BTCUSDT 做多且量比不足时不开仓", closed_trades=rows, min_sample=3
    )
    assert scored is not None and scored["sample_size"] == 8
    assert "BTCUSDT" in scored["basis"] and "多头" in scored["basis"]
    assert scored["health"] > 0.5
    # 样本不足 = 未知，绝不改分、绝不归档
    assert health.score_lesson_against_trades("当 BTCUSDT 做多时不开仓", closed_trades=rows[:2], min_sample=10) is None
    assert health.decide_archive({"enabled": True}, None)[0] is False
    low = {"health": 0.2, "sample_size": 20, "basis": "全样本"}
    should, _ = health.decide_archive({"enabled": True}, low, threshold=0.45)
    assert should is True
    low2 = {**low, "sample_size": 3}
    assert health.decide_archive({"enabled": True}, low2, threshold=0.45)[0] is False
    assert health.snapshot_observability({"rsi": 50}) == "PRICE_ONLY"
    assert health.snapshot_observability({}) == "NONE"


def test_batch_review_no_change_never_writes_mantras(monkeypatch):
    """模型说 NO_CHANGE / 给不出提案 / AI 挂掉，都不允许规则兜底造心法."""
    import asyncio
    from types import SimpleNamespace

    from nanoquant.db import init_db, session_scope
    from nanoquant.evolution import review as ev
    from nanoquant.models import Mantra, TradeRecord

    init_db()
    _clean_mantras()
    now = 1700000000
    with session_scope() as s:
        for i in range(4):
            s.add(TradeRecord(
                symbol="BTCUSDT", side="long", qty=1, entry_price=100, exit_price=99,
                pnl=-1.0, pnl_pct=-1, roi_pct=-1, fees=0.1, opened_at=now + i,
                closed_at=now + i + 60, hold_seconds=60, exit_reason="trailing_stop",
                reviewed=False, features={"rsi": 50, "atr_pct": 0.5, "vol_ratio": 0.8},
            ))

    async def fake_chat(*args, **kwargs):  # noqa: ARG001
        return SimpleNamespace(ok=True, text='{"change_status":"NO_CHANGE","lessons":[],"diagnosis":[]}')

    monkeypatch.setattr(ev.ai_client, "chat_with_fallback", fake_chat)
    out = asyncio.run(ev.review_batch(limit=4))
    assert out["ok"] and out["change_status"] == "NO_CHANGE"
    assert out["applied"] == []
    with session_scope() as s:
        assert s.query(Mantra).count() == 0

    # LLM 全链失败：只产出台账事实（带前缀），仍 NO_CHANGE、不入库
    async def failing_chat(*args, **kwargs):  # noqa: ARG001
        return SimpleNamespace(ok=False, error="upstream down", text="")

    monkeypatch.setattr(ev.ai_client, "chat_with_fallback", failing_chat)
    out2 = asyncio.run(ev.review_batch(limit=4, force=True))
    assert out2["change_status"] == "NO_CHANGE" and out2["applied"] == []
    assert out2["insights"] and out2["insights"][0].startswith("[本地台账推导]")
    with session_scope() as s:
        assert s.query(Mantra).count() == 0
        s.query(TradeRecord).delete()


def test_effectiveness_feedback_blocks_repeated_proposals():
    """已证伪心法会降权，并让生成侧拒绝语义重复的新提案."""
    import time

    from nanoquant.db import session_scope
    from nanoquant.evolution import mantra as store
    from nanoquant.models import Mantra

    _clean_mantras()
    now = int(time.time())
    with session_scope() as s:
        s.add(Mantra(
            title="低量能追单易被止损",
            rule="当量比低于 0.9 且 ATR 占比低于 0.25% 时不开仓，等待量能放大。",
            category="entry", source="test", confidence=0.7, health=0.9,
            enabled=True, version=1, superseded_by=0,
            evidence={"effectiveness_verdict": "underperform"},
            created_at=now, updated_at=now, expires_at=now + 86400,
        ))
        s.add(Mantra(
            title="量能确认后再入场",
            rule="当 RSI 高于 78 且价格远离均线时不开仓，等待回踩确认。",
            category="entry", source="test", confidence=0.7, health=0.7,
            enabled=True, version=1, superseded_by=0, evidence={},
            created_at=now, updated_at=now, expires_at=now + 86400,
        ))
    hit = store.underperforming_similar(
        "低量能追单容易被止损", "当量比低于 0.9 且 ATR 占比低于 0.25% 时不开仓，等待量能放大后介入。"
    )
    assert hit and hit["title"] == "低量能追单易被止损"
    assert store.underperforming_similar("超买不追多", "当 RSI 高于 80 时不开仓，等待回踩。") is None
    ranked = store.rank_lessons(10)
    under = next(m for m in ranked if m["id"] == hit["id"])
    assert under["health"] <= 0.9 * 0.8 + 1e-6  # 注入排序已降权


def test_upsert_lesson_honors_sample_size_and_cas():
    from nanoquant.evolution import mantra as store
    from nanoquant.services import state

    _clean_mantras()
    rule = "当 ATR 占比低于 0.25% 且量比不足时不开仓，等待确认。"
    rejected = store.upsert_lesson(title="小样本噪声", rule=rule, category="entry", confidence=0.6, sample_size=1)
    assert rejected["action"] == "rejected" and "样本量不足" in rejected["reason"]
    created = store.upsert_lesson(title="足样本结论", rule=rule, category="entry", confidence=0.6, sample_size=12)
    assert created["action"] == "created"
    assert state.get("mantra_version", "0") != "0"  # CAS 版本已推进
    conflict = store.upsert_lesson(
        title="陈旧快照写入", rule=rule, category="entry", confidence=0.6,
        sample_size=12, expected_version="stale-version",
    )
    assert conflict["action"] == "conflict"


def test_injection_report_discloses_not_injected():
    from nanoquant.evolution import mantra as store
    from nanoquant.services.settings import get_evolution, update_group

    import time

    from nanoquant.db import session_scope
    from nanoquant.models import Mantra

    _clean_mantras()
    old = get_evolution().max_injected
    update_group("evolution", {"max_injected": 1})
    try:
        now = int(time.time())
        with session_scope() as s:
            for i, (title, rule) in enumerate([
                ("量能不足不追单", "当量比低于 0.8 且 ATR 占比偏低时不开仓，等待放量确认。"),
                ("超买不追多", "当 RSI 高于 80 且价格远离均线时不开仓，等待回踩。"),
                ("持仓超时离场", "当持仓超过 180 分钟且浮盈不足时主动离场，不扛单。"),
            ]):
                s.add(Mantra(
                    title=title, rule=rule, category="entry", source="test",
                    confidence=0.6, health=0.6, enabled=True, version=1,
                    superseded_by=0, evidence={}, created_at=now + i, updated_at=now,
                    expires_at=now + 86400,
                ))
        report = store.injection_report()
        assert report["active"] == 3 and report["injected"] == 1 and report["limit"] == 1
        assert len(report["not_injected"]) == 2
    finally:
        update_group("evolution", {"max_injected": old})


# ---------------------------------------------- 全局杠杆下限 / 上限
def test_choose_leverage_clamps_ai_to_global_floor_and_cap():
    """AI/心法给出的杠杆只是区间内建议值，越界一律钳回全局下限/上限."""
    from nanoquant.engine.risk import choose_leverage
    from nanoquant.exchange.precision import SymbolFilters
    from nanoquant.services.settings import EngineSettings, RiskSettings

    engine = EngineSettings()
    f = SymbolFilters(symbol="BTCUSDT", max_leverage=125, price_tick=0.01, qty_step=0.001)
    cfg = RiskSettings(default_leverage=20, max_leverage=100)

    assert choose_leverage("BTCUSDT", engine, cfg, f) == 20           # 无建议 → 下限
    assert choose_leverage("BTCUSDT", engine, cfg, f, 1) == 20        # 过低 → 抬到下限
    assert choose_leverage("BTCUSDT", engine, cfg, f, 5) == 20
    assert choose_leverage("BTCUSDT", engine, cfg, f, 35) == 35       # 区间内采纳
    assert choose_leverage("BTCUSDT", engine, cfg, f, 125) == 100     # 过高 → 压到上限

    tight = RiskSettings(default_leverage=20, max_leverage=30)
    assert choose_leverage("BTCUSDT", engine, tight, f, 99) == 30

    # 交易所上限更低时以交易所为准，即使全局下限更高也不能突破
    f12 = SymbolFilters(symbol="BTCUSDT", max_leverage=12, price_tick=0.01, qty_step=0.001)
    assert choose_leverage("BTCUSDT", engine, cfg, f12, 80) == 12
    assert choose_leverage("BTCUSDT", engine, cfg, f12) == 12

    low = RiskSettings(default_leverage=1, max_leverage=100)
    assert choose_leverage("BTCUSDT", engine, low, f, 1) == 1        # 下限最低 1x


def test_strategy_pick_defaults_to_none_without_profiles():
    """没有任何启用策略时 pick_from 返回 None ⇒ AI 走默认分析."""
    from nanoquant.services import strategy_profiles as sp

    assert sp.pick_from("BTCUSDT", []) is None
    assert sp.pick_from("BTCUSDT", [{"id": 1, "name": "停用", "enabled": False, "filters": {}}]) is None


def test_strategy_pick_priority_and_symbol_scope():
    from nanoquant.services import strategy_profiles as sp

    profiles = [
        {"id": 1, "name": "只给 DOGE", "enabled": True, "priority": 10,
         "filters": sp.normalize_filters({"symbols": ["DOGEUSDT"], "side": "long"})},
        {"id": 2, "name": "全池通用", "enabled": True, "priority": 20,
         "filters": sp.normalize_filters({"side": "any"})},
    ]
    assert sp.pick_from("DOGEUSDT", profiles)["name"] == "只给 DOGE"
    assert sp.pick_from("BTCUSDT", profiles)["name"] == "全池通用"
    # 优先级更高的通用策略先命中
    profiles[1]["priority"] = 5
    assert sp.pick_from("DOGEUSDT", profiles)["name"] == "全池通用"


def test_strategy_filters_normalize_scope_and_summary():
    from nanoquant.services import strategy_profiles as sp

    f = sp.normalize_filters({"symbols": "btcusdt, ethusdt", "side": "LONG", "min_quote_volume": 5e8})
    assert f == {"symbols": ["BTCUSDT", "ETHUSDT"], "side": "long"}
    summary = sp.summarize(f)
    assert "BTCUSDT/ETHUSDT" in summary and "只做多" in summary
    assert "全池" in sp.summarize({})
    assert len(sp.normalize_rule("x" * 5000)) == sp.MAX_RULE_CHARS


def test_weight_limiter_fails_fast_instead_of_queueing(monkeypatch):
    """令牌不足时最多排队 MAX_WAIT_SEC，超时快速失败，避免限频期间请求堆积."""
    import asyncio

    from nanoquant.exchange import ratelimit

    async def run():
        limiter = ratelimit.WeightLimiter(capacity=10, window=60.0)
        await limiter.acquire(10)  # 用光令牌
        monkeypatch.setattr(ratelimit, "MAX_WAIT_SEC", 0.0)
        try:
            await limiter.acquire(1)
        except ratelimit.RateLimited as exc:
            assert "权重预算不足" in str(exc)
        else:  # pragma: no cover
            raise AssertionError("默认应快速失败，而不是挂起排队")

    asyncio.run(run())


def test_ai_prompt_exposes_global_leverage_bounds_and_omits_mantra_rules(monkeypatch):
    """决策上下文显式给出全局区间，不再把心法规则原文塞给模型，同时注入适用策略."""
    from types import SimpleNamespace

    from nanoquant.ai.compact import format_snapshot_v2
    from nanoquant.ai.prompts import DECISION_SYSTEM, format_snapshot
    from nanoquant.engine import worker as w
    from nanoquant.services import strategy_profiles as sp
    from nanoquant.services.settings import EngineSettings, RiskSettings

    assert "全局" in DECISION_SYSTEM and "心法" in DECISION_SYSTEM
    assert "min_leverage" in DECISION_SYSTEM and "max_leverage" in DECISION_SYSTEM
    assert "strategy" in DECISION_SYSTEM

    monkeypatch.setattr(
        w, "get_ai",
        lambda: SimpleNamespace(inject_mantras=True, inject_news=False, min_confidence=0.45),
    )
    monkeypatch.setattr(w.mantra_store, "rank_lessons", lambda: [
        {"id": 1, "title": "高波动技巧", "rule": "当信号强时可用 30x 杠杆", "health": 0.7},
    ])
    monkeypatch.setattr(w.risk_mod, "open_positions", lambda: [])
    monkeypatch.setattr(w.state, "get_float", lambda *a, **k: 1000.0)
    monkeypatch.setattr(sp, "pick_for", lambda _symbol: {
        "id": 9, "name": "回踩做多", "enabled": True, "priority": 10,
        "rule": "当 15m 回踩 EMA21 不破且量比大于 1.2 时做多；跌破回踩低点离场。",
        "filters": {"symbols": ["BTCUSDT"], "side": "long"},
    })

    sig = SimpleNamespace(symbol="BTCUSDT", score=0.5, direction="long", features={}, reasons=[])
    ticker = SimpleNamespace(last=100.0, change_pct=1.0, quote_volume=1e9)
    account = SimpleNamespace(equity=1000.0, available=1000.0)
    filters = SimpleNamespace(max_leverage=125, effective_min_notional=lambda p: 5.0)
    ctx = w.TradingEngine()._build_ai_context(
        sig, ticker, account, EngineSettings(),
        RiskSettings(default_leverage=20, max_leverage=100), filters,
    )
    assert all("rule" not in m for m in ctx["mantras"])
    assert "30x" not in format_snapshot(ctx)
    assert ctx["constraints"]["min_leverage"] == 20
    assert ctx["constraints"]["max_leverage"] == 100
    assert "levmin" in format_snapshot_v2(ctx)

    # 策略作为分析指令进入上下文与两种 prompt 协议
    assert ctx["strategy"]["name"] == "回踩做多"
    assert "回踩 EMA21" in format_snapshot(ctx)
    assert '"st"' in format_snapshot_v2(ctx) and "回踩 EMA21" in format_snapshot_v2(ctx)

    # 没有适用策略时是 None（默认分析）
    monkeypatch.setattr(sp, "pick_for", lambda _symbol: None)
    ctx2 = w.TradingEngine()._build_ai_context(
        sig, ticker, account, EngineSettings(),
        RiskSettings(default_leverage=20, max_leverage=100), filters,
    )
    assert ctx2["strategy"] is None
