"""开仓准入体检.

把「能不能开仓」拆成可逐条勾选的 6 个前置条件, 每一项都给出真实状态与修复动作,
避免新手面对一堆参数却不知道卡在哪一步.

只读为主: 不触发下单, 不改配置; 唯一的写入是日切锚点(与引擎行为一致).
"""
from __future__ import annotations

from typing import Any

from ..ai.client import providers_available
from ..db import session_scope
from ..exchange.broker import estimate_qty, get_broker, min_notional_buffer
from ..exchange.client import BinanceError, NoCredentials
from ..exchange.client import client as binance_client
from ..logging_setup import get_logger
from ..models import PositionRecord, TradingSymbol
from ..security import decrypt_secret
from . import state
from . import symbols as symbol_service
from .settings import get_ai, get_data, get_engine, get_risk, get_strategy, normalize_mode

log = get_logger("admission")


def _step(key: str, title: str, ok: bool, detail: str, *, level: str = "fail", action: str = "", action_text: str = "") -> dict[str, Any]:
    return {
        "key": key,
        "title": title,
        "ok": ok,
        "level": "ok" if ok else level,  # ok / warn / fail
        "detail": detail,
        "action": action,
        "action_text": action_text,
    }


async def evaluate() -> dict[str, Any]:
    engine_cfg = get_engine()
    risk_cfg = get_risk()
    data_cfg = get_data()
    ai_cfg = get_ai()
    strat_cfg = get_strategy()

    broker = get_broker(engine_cfg.mode)
    account = None
    account_error = ""
    try:
        account = await broker.account()
    except Exception as exc:  # noqa: BLE001
        account_error = f"{type(exc).__name__}: {exc}"[:160]

    equity = account.equity if account else 0.0
    available = account.available if account else 0.0

    # ---------------------------------------------------------- 1. 交易通道
    mode = normalize_mode(engine_cfg.mode)
    label = "模拟盘（币安测试网）" if mode == "demo" else "实盘（币安主网）"
    if mode == "demo":
        key_ok = bool(decrypt_secret(data_cfg.demo_api_key_enc)) and bool(decrypt_secret(data_cfg.demo_api_secret_enc))
    else:
        key_ok = bool(decrypt_secret(data_cfg.live_api_key_enc)) and bool(decrypt_secret(data_cfg.live_api_secret_enc))
    steps = [
        _step(
            "channel",
            "交易通道",
            key_ok and not account_error,
            account_error
            or (f"{label} 凭证已配置，接口可用" if key_ok else f"{label} 缺少 API Key/Secret"),
            action="#/strategy?group=data",
            action_text="配置当前环境密钥",
        )
    ]

    # ---------------------------------------------------------- 3. AI 复核
    ai_ready = providers_available() > 0
    if not ai_cfg.enabled:
        steps.append(
            _step(
                "ai",
                "AI 复核",
                True,
                "AI 已关闭: 纯量化决策, 不影响开仓",
                level="warn",
                action="#/ai",
                action_text="接入模型",
            )
        )
    else:
        steps.append(
            _step(
                "ai",
                "AI 复核",
                ai_ready,
                "已接入可用模型, 开仓前会做方向/杠杆复核"
                if ai_ready
                else "AI 已开启但未配置模型: 当前自动回退纯量化 (不会阻塞开仓)",
                level="warn",
                action="#/ai",
                action_text="接入模型",
            )
        )

    # ---------------------------------------------------------- 4. 标的池
    with session_scope() as s:
        pool_rows = s.query(TradingSymbol).all()
        open_count = (
            s.query(PositionRecord)
            .filter(PositionRecord.status == "open", PositionRecord.mode == mode)
            .count()
        )
    enabled_pool = [r.symbol for r in pool_rows if r.enabled]
    steps.append(
        _step(
            "pool",
            "标的池",
            bool(enabled_pool),
            f"已启用 {len(enabled_pool)} 个标的: {', '.join(enabled_pool[:6])}"
            if enabled_pool
            else "标的池为空, 引擎不会扫描任何品种",
            action="#/symbols",
            action_text="管理标的池",
        )
    )

    # ---------------------------------------------------------- 5. 资金与风控
    allocatable = equity * risk_cfg.margin_pct / 100.0 * risk_cfg.default_leverage
    fund_ok = equity >= risk_cfg.min_equity_usdt and available > 0
    steps.append(
        _step(
            "funds",
            "资金与风控",
            fund_ok and bool(account),
            (
                f"权益 {equity:.4f} USDT (下限 {risk_cfg.min_equity_usdt}), "
                f"单笔可分配名义额约 {allocatable:.2f} USDT"
            )
            if account
            else f"账户读取失败: {account_error}",
            action="#/strategy?group=risk",
            action_text="调整风控参数",
        )
    )

    # ---------------------------------------------------------- 6. 引擎与时段
    window_ok, window_reason = True, "全天可交易"
    try:
        from ..engine.risk import trading_window_open

        window_ok, window_reason = trading_window_open(engine_cfg)
        window_reason = window_reason or "当前处于可交易时段"
    except Exception:  # noqa: BLE001
        # 时段闸门算不出来时按"可交易"放行(与原行为一致), 但必须留痕:
        # 这条静默意味着闸门可能一直是失效状态, 却看不出来
        log.warning("交易时段计算失败, 本次按全天可交易放行", exc_info=True)
    cooldowns = state.active_cooldowns(int(__import__("time").time()))
    consecutive = state.get_int("consecutive_losses", 0)
    halted = state.normalize_halted(state.get("halted"))
    engine_ok = (
        engine_cfg.enabled
        and window_ok
        and consecutive < risk_cfg.consecutive_loss_limit
        and not halted
    )
    engine_detail = []
    if not engine_cfg.enabled:
        engine_detail.append("引擎未启动 (总览页点「启动引擎」)")
    if not window_ok:
        engine_detail.append(window_reason)
    if consecutive >= risk_cfg.consecutive_loss_limit:
        engine_detail.append(f"连亏 {consecutive} 笔触发冷静期")
    if halted:
        engine_detail.append(f"已熔断: {halted[:60]}")
    if cooldowns:
        engine_detail.append(f"{len(cooldowns)} 个标的处于冷却")
    guard_remaining = max(0, int(state.get_float("sl_guard_until", 0) - __import__("time").time()))
    if guard_remaining > 0:
        engine_detail.append(f"止损熔断中, 剩余 {guard_remaining}s")
    steps.append(
        _step(
            "engine",
            "引擎与交易时段",
            engine_ok,
            "; ".join(engine_detail) if engine_detail else "引擎运行中, 处于允许开仓的时段",
            level="warn",
            action="#/strategy?group=engine",
            action_text="调整时段/间隔",
        )
    )

    # ---------------------------------------------------------- 标的可交易性
    symbols: list[dict[str, Any]] = []
    tickers: dict[str, Any] = {}
    if enabled_pool:
        try:
            tickers = await binance_client.tickers(enabled_pool)
        except (BinanceError, NoCredentials):
            tickers = {}
    for symbol in enabled_pool:
        filters = symbol_service.filters_for(symbol)
        ticker = tickers.get(symbol)
        if filters is None or ticker is None:
            symbols.append(
                {
                    "symbol": symbol,
                    "tradeable": False,
                    "reason": "缺少交易规则或行情",
                    "max_leverage": 0,
                    # 下面三项缺失时也要占位：后面的"最低门槛"提示会读它们，
                    # 缺字段会直接 KeyError 把整个体检打成 500
                    "min_notional": None,
                    "leverage_used": 0,
                    "required_margin": 0.0,
                }
            )
            continue
        # 与真实开仓一致：杠杆下限/上限取全局风控，交易所上限更低时以交易所为准
        lev_upper = max(1, min(int(risk_cfg.max_leverage), int(filters.max_leverage or 100), 100))
        leverage = max(1, min(int(risk_cfg.default_leverage), lev_upper))
        qty, note = estimate_qty(
            equity, ticker.last, leverage, risk_cfg, 0.005, filters,
            # 账户接口不可用时不要把"可用 0"误判成余额不足
            available=available if account else None,
        )
        # 引擎按「交易所下限 × 缓冲」下单，体检必须用同一个门槛，
        # 否则面板说"够开"、引擎却因为差几分钱拒绝，用户只能看到莫名奇妙的跳过。
        min_notional = filters.effective_min_notional(ticker.last) * min_notional_buffer(risk_cfg)
        symbols.append(
            {
                "symbol": symbol,
                "tradeable": qty > 0,
                "reason": "" if qty > 0 else note,
                "price": ticker.last,
                "max_leverage": filters.max_leverage,
                "leverage_used": leverage,
                "min_qty": filters.effective_min_qty(ticker.last),
                "min_notional": round(min_notional, 4),
                # 该杠杆下开最小仓所需的保证金 (名义额 / 杠杆), 余额门槛就是它
                "required_margin": round(min_notional / max(1.0, leverage), 4),
                # 用该标的允许的最高杠杆时的保证金下限: 余额只有零点几个 U 也能开的场景
                "max_leverage_used": lev_upper,
                "required_margin_at_max": round(min_notional / max(1.0, lev_upper), 4),
                "allocatable_notional": round(allocatable, 4),
                "qty_if_open": qty,
                "notional_if_open": round(qty * ticker.last, 4) if qty else 0.0,
            }
        )

    tradeable = [s for s in symbols if s["tradeable"]]
    priced = [s for s in symbols if isinstance(s.get("min_notional"), (int, float))]
    cheapest = min(priced, key=lambda s: s["min_notional"]) if priced else None
    tip = (
        f"；最低门槛 {cheapest['symbol']} 名义额 {cheapest['min_notional']} U、"
        f"{cheapest['leverage_used']}x 下需保证金 {cheapest['required_margin']} U"
        if cheapest
        else ""
    )
    steps.append(
        _step(
            "symbols",
            "可交易标的",
            bool(tradeable),
            (
                f"{len(tradeable)}/{len(symbols)} 个标的在当前权益下可下单: "
                + ", ".join(s["symbol"] for s in tradeable[:5])
                + tip
            )
            if tradeable
            else (
                "当前权益不足以满足任何标的的最小名义额"
                + (f"（最低门槛 {cheapest['symbol']} 需 {cheapest['min_notional']} U 名义额 / "
                   f"{cheapest['required_margin']} U 保证金）" if cheapest else "")
                + "; 提高杠杆/保证金比例, 或加入 DOGE 等小面值标的"
                if symbols
                else "标的池为空"
            ),
            level="warn",
            action="#/symbols",
            action_text="优化标的池",
        )
    )

    blocking = [s for s in steps if not s["ok"] and s["level"] == "fail"]
    warnings = [s for s in steps if not s["ok"] and s["level"] == "warn"]
    score = round(sum(1 for s in steps if s["ok"]) / len(steps), 3)

    return {
        "ok": not blocking,
        "can_open": bool(tradeable) and not blocking and engine_cfg.enabled,
        "score": score,
        "mode": mode,
        "steps": steps,
        "blocking": [s["key"] for s in blocking],
        "warnings": [s["key"] for s in warnings],
        "account": {
            "equity": round(equity, 6),
            "available": round(available, 6),
            "positions": open_count,
            "max_positions": engine_cfg.max_positions,
        },
        "params": {
            "risk_per_trade_pct": risk_cfg.risk_per_trade_pct,
            "margin_pct": risk_cfg.margin_pct,
            "default_leverage": risk_cfg.default_leverage,
            "max_leverage": risk_cfg.max_leverage,
            "sl_estimate_pct": 0.5,
            "timeframe": strat_cfg.timeframe,
            "entry_threshold": strat_cfg.entry_threshold,
        },
        "symbols": symbols,
        "tradeable_count": len(tradeable),
        "evaluated_at": int(__import__("time").time()),
    }
