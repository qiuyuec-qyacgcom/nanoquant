"""交易: 引擎控制 / 持仓 / 订单 / 成交 / 信号 / 权益曲线."""
from __future__ import annotations

import asyncio
import time

from fastapi import APIRouter, Depends, HTTPException, Query

from ...db import session_scope
from ...engine.worker import engine
from ...evolution.review import trade_stats
from ...exchange.broker import get_broker
from ...exchange.client import BinanceError
from ...exchange.client import client as binance_client
from ...models import EquityPoint, OrderRecord, PositionRecord, Signal, TradeRecord
from ...security import decrypt_secret
from ...services import events, position_view, state
from ...services.settings import get_data, get_engine, normalize_mode, update_group
from ..deps import current_user
from ..schemas import ClosePositionIn, EngineControl, TradeModeIn

router = APIRouter(tags=["trade"])

#: 状态聚合缓存：顶栏 5s + 总览 8s + 跟单 5s 轮询、多标签页叠加时，
#: 2 秒内只向交易所取一次 account/positionRisk（单飞锁防并发击穿）。
_status_cache: dict | None = None
_status_ts = 0.0
_status_lock = asyncio.Lock()


def invalidate_status_cache() -> None:
    """控制/平仓/立即执行一轮后，让下一次 status 立即取新快照."""
    global _status_cache, _status_ts
    _status_cache = None
    _status_ts = 0.0


async def _status_snapshot() -> dict:
    global _status_cache, _status_ts
    now = time.time()
    if _status_cache is not None and now - _status_ts < 2.0:
        return dict(_status_cache)
    async with _status_lock:
        if _status_cache is not None and time.time() - _status_ts < 2.0:
            return dict(_status_cache)
        result = await engine.status()
        _status_cache = result
        _status_ts = time.time()
        return dict(result)


@router.get("/engine/status")
async def status(user: str = Depends(current_user)) -> dict:
    return await _status_snapshot()


@router.get("/engine/admission")
async def admission(user: str = Depends(current_user)) -> dict:
    """开仓准入体检: 6 项前置条件 + 每个标的的可下单性."""
    from ...services import admission as admission_service

    return await admission_service.evaluate()


@router.get("/engine/trade-mode")
async def trade_mode(user: str = Depends(current_user)) -> dict:
    """当前持仓模式与保证金模式体检（双向持仓 / 逐仓）."""
    from ...services import symbols as symbol_service

    broker = get_broker(normalize_mode(get_engine().mode))
    return await broker.trade_mode_status(sorted(symbol_service.known_symbols()))


@router.post("/engine/trade-mode")
async def switch_trade_mode(payload: TradeModeIn, user: str = Depends(current_user)) -> dict:
    """引导式切换为双向持仓 + 逐仓；有持仓时会被交易所拒绝，返回可操作原因."""
    broker = get_broker(normalize_mode(get_engine().mode))
    result = await broker.switch_trade_mode(hedge=payload.hedge, isolated=payload.isolated)
    events.record(
        "info" if result.get("ok") else "warn",
        "engine",
        f"{user} 切换交易模式: 双向={'开' if payload.hedge else '不变'}，逐仓={'开' if payload.isolated else '不变'}",
        result,
    )
    invalidate_status_cache()
    return result


@router.post("/engine/control")
async def control(payload: EngineControl, user: str = Depends(current_user)) -> dict:
    """引擎开关与交易环境切换.

    模拟盘(demo) 与实盘(live) 是唯一的交易环境开关:
    - demo 对接币安模拟盘 (testnet), live 对接币安实盘, **两者都必须配置各自的 API 凭证**;
    - 切到实盘必须带确认短语 `confirm="LIVE"`;
    - 切换后立即探测目标环境接口, 不可达时回传警告 (引擎在恢复前不会开新仓).
    """
    patch: dict = {}
    warning = ""
    if payload.mode is not None:
        target = normalize_mode(payload.mode, default="")
        if target not in {"demo", "live"}:
            raise HTTPException(status_code=400, detail="交易环境只能是 demo(模拟盘) 或 live(实盘)")
        current = normalize_mode(get_engine().mode)
        if target != current:
            cfg = get_data()
            if target == "live":
                if payload.confirm.strip().upper() != "LIVE":
                    raise HTTPException(status_code=400, detail="切换到实盘需要输入确认短语 LIVE")
                if not decrypt_secret(cfg.live_api_key_enc) or not decrypt_secret(cfg.live_api_secret_enc):
                    raise HTTPException(status_code=400, detail="未配置实盘环境的币安 API Key/Secret, 无法切到实盘")
            else:
                if not decrypt_secret(cfg.demo_api_key_enc) or not decrypt_secret(cfg.demo_api_secret_enc):
                    raise HTTPException(
                        status_code=400, detail="未配置模拟盘(币安测试网)环境的 API Key/Secret, 无法切到模拟盘"
                    )
        patch["mode"] = target
    if payload.enabled:
        # 当前环境必须已有凭证, 否则引擎每轮都拿不到账户, 直接拒绝启动
        cfg = get_data()
        if normalize_mode(get_engine().mode) == "live":
            has_creds = bool(decrypt_secret(cfg.live_api_key_enc)) and bool(decrypt_secret(cfg.live_api_secret_enc))
        else:
            has_creds = bool(decrypt_secret(cfg.demo_api_key_enc)) and bool(decrypt_secret(cfg.demo_api_secret_enc))
        if not has_creds:
            raise HTTPException(status_code=400, detail="当前交易环境未配置 API 凭证，无法启动引擎")
    if payload.enabled is not None:
        patch["enabled"] = payload.enabled
    if patch:
        update_group("engine", patch)
        state.set_many({k: str(v).lower() for k, v in patch.items()})
        action = "启动" if payload.enabled else "停止" if payload.enabled is False else "调整"
        events.record(
            "warn" if patch.get("enabled") is False else "info",
            "engine",
            f"{user} {action}引擎 {patch}",
        )
        if payload.enabled:
            await engine.start()
        elif payload.enabled is False:
            await engine.stop()
        if patch.get("mode"):
            try:
                reachable = await binance_client.ping()
            except Exception:  # noqa: BLE001
                reachable = False
            if not reachable:
                warning = "已切换交易环境, 但该环境的币安接口暂时不可达; 恢复前引擎不会开新仓"
    if patch:
        invalidate_status_cache()
    return {"ok": True, "warning": warning, "status": await _status_snapshot()}


@router.get("/engine/execution")
def execution_metrics(user: str = Depends(current_user)) -> dict:
    """下单关键路径的分段耗时 (最近 N 笔): p50/p95/p99/max.

    内存滚动窗口，不落库——把写库塞进下单路径本身就是 p99 的来源之一。
    """
    from ...services.exec_metrics import exec_metrics

    return exec_metrics.summary()


@router.get("/engine/quotes")
async def quotes(user: str = Depends(current_user)) -> dict:
    """当前交易环境标的池的 24h 快照 (后台 K 线工作站列表用)."""
    from ...services.symbols import ordered_symbols

    pool = ordered_symbols(enabled_only=True)
    if not pool:
        return {"symbols": [], "tickers": {}, "updated_at": int(time.time())}
    try:
        rows = await binance_client.tickers(pool)
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=503, detail=f"行情暂不可用: {exc}") from exc
    return {
        "symbols": pool,
        "tickers": {s: {"last": t.last, "change_pct": t.change_pct} for s, t in rows.items()},
        "updated_at": int(time.time()),
    }


@router.get("/chart/klines")
async def chart_klines(
    symbol: str = Query(..., description="合约, 必须在标的池中"),
    interval: str = Query(default="15m"),
    limit: int = Query(default=600, ge=20, le=1000),
    user: str = Depends(current_user),
) -> dict:
    """后台 K 线工作站取数: 与前台同源 (只认标的池), 但不需要公开站点开启."""
    from ...services.symbols import ordered_symbols

    target = symbol.upper()
    pool = ordered_symbols(enabled_only=True)
    if target not in pool:
        raise HTTPException(status_code=404, detail=f"{target} 不在标的池中")
    try:
        candles = await binance_client.klines(target, interval, limit, fresh=True)
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=503, detail=f"币安行情暂不可用: {exc}") from exc
    step = {
        "1m": 60_000, "3m": 180_000, "5m": 300_000, "15m": 900_000,
        "30m": 1_800_000, "1h": 3_600_000, "4h": 14_400_000, "1d": 86_400_000,
    }.get(interval, 60_000)
    items = [
        {
            "t": int(c.open_time),
            "o": float(c.open),
            "h": float(c.high),
            "l": float(c.low),
            "c": float(c.close),
            "v": float(c.volume),
        }
        for c in candles
    ]
    last = candles[-1] if candles else None
    return {
        "symbol": target,
        "interval": interval,
        "items": items,
        "count": len(items),
        "next_close_at": int(last.open_time) + step if last else 0,
        "server_time": int(time.time() * 1000),
    }


@router.post("/engine/cycle-now")
async def cycle_now(user: str = Depends(current_user)) -> dict:
    result = await engine.cycle()
    invalidate_status_cache()
    return {"ok": True, "result": result}


@router.post("/engine/reset-anchors")
async def reset_anchors(user: str = Depends(current_user)) -> dict:
    """把「日亏基准 / 回撤峰值」对齐到当前权益.

    什么时候需要：给模拟盘充值、重置资金、换账户之后。旧基准留着的话，系统会拿
    新资金和旧基准相比，算出 -99% 的假日内亏损并直接熔断（这个坑真实发生过两次）。

    只清理「日内亏损」类熔断——日亏基准本身被重置了，那条判定已经失效；
    回撤、连亏、止损熔断属于真实风控状态，这里不碰。
    """
    mode = normalize_mode(get_engine().mode)
    try:
        account = await get_broker(mode).account()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(400, f"读取账户权益失败: {str(exc)[:120]}") from exc
    equity = float(getattr(account, "equity", 0.0) or 0.0)
    if equity <= 0:
        raise HTTPException(400, "当前权益为 0，不能作为日亏基准")

    before = {
        "day_start_equity": state.get("day_start_equity"),
        "equity_peak": state.get("equity_peak"),
        "halted": state.get("halted"),
    }
    state.set_many(
        {
            "day_start_equity": f"{equity:.8f}",
            "equity_peak": f"{equity:.8f}",
        }
    )
    cleared = ""
    if "日内亏损" in (before["halted"] or ""):
        cleared = before["halted"]
        state.set("halted", "")
    invalidate_status_cache()
    events.record(
        "warn",
        "risk",
        f"日亏基准与回撤峰值已重置为 {equity:.4f}",
        {"equity": equity, "cleared_halt": cleared},
    )
    return {
        "ok": True,
        "equity": round(equity, 8),
        "before": before,
        "cleared_halt": cleared,
    }


@router.get("/engine/positions")
async def positions(
    include_closed: bool = False,
    limit: int = Query(default=50, ge=1, le=500),
    user: str = Depends(current_user),
) -> dict:
    mode = normalize_mode(get_engine().mode)
    with session_scope() as s:
        q = (
            s.query(PositionRecord)
            .filter(PositionRecord.mode == mode)
            .order_by(PositionRecord.opened_at.desc())
        )
        if not include_closed:
            q = q.filter(PositionRecord.status == "open")
        rows = q.limit(limit).all()
        items = [
            {
                "id": p.id,
                "symbol": p.symbol,
                "side": p.side,
                "qty": p.qty,
                "entry_price": p.entry_price,
                "mark_price": p.mark_price,
                "leverage": p.leverage,
                "margin": p.margin,
                "sl_price": p.sl_price,
                "tp_price": p.tp_price,
                "peak_price": p.peak_price,
                "opened_at": p.opened_at,
                "closed_at": p.closed_at,
                "exit_price": p.exit_price,
                "exit_reason": p.exit_reason,
                "realized_pnl": p.realized_pnl,
                "fees": p.fees,
                "ai_confidence": p.ai_confidence,
                "ai_reason": p.ai_reason,
                "signal_score": p.signal_score,
                "mode": p.mode,
                "status": p.status,
                "trade_id": p.trade_id,
                **position_view.derived(p),
            }
            for p in rows
        ]
    return {"items": items}


@router.post("/engine/positions/{position_id}/protect")
async def protect_position(position_id: int, user: str = Depends(current_user)) -> dict:
    """给没有保护单的仓位按 ATR 补挂止损/止盈（手动开仓接管用）."""
    outcome = await engine.protect_position(position_id, reason=f"{user} 手动补挂")
    if not outcome.get("ok"):
        raise HTTPException(status_code=400, detail=outcome.get("reason") or "补挂失败")
    invalidate_status_cache()
    return outcome


@router.post("/engine/positions/{position_id}/close")
async def close_position(position_id: int, payload: ClosePositionIn, user: str = Depends(current_user)) -> dict:
    with session_scope() as s:
        pos = s.get(PositionRecord, position_id)
        if pos is None or pos.status != "open":
            raise HTTPException(status_code=404, detail="仓位不存在或已平")
        s.expunge(pos)
    broker = get_broker(pos.mode)
    result = await broker.close_position(position=pos, reason=payload.reason or "manual")
    if not result.ok:
        raise HTTPException(status_code=400, detail=result.message)
    events.record("warn", "trade", f"{user} 手动平仓 {pos.symbol} {pos.side}", {"position_id": position_id})
    invalidate_status_cache()
    return {"ok": True, "price": result.price, "qty": result.qty, "message": result.message}


@router.get("/engine/orders")
def orders(limit: int = Query(default=80, ge=1, le=500), user: str = Depends(current_user)) -> dict:
    mode = normalize_mode(get_engine().mode)
    with session_scope() as s:
        rows = (
            s.query(OrderRecord)
            .filter(OrderRecord.mode == mode)
            .order_by(OrderRecord.ts.desc())
            .limit(limit)
            .all()
        )
        items = [
            {
                "id": o.id,
                "ts": o.ts,
                "symbol": o.symbol,
                "side": o.side,
                "order_type": o.order_type,
                "qty": o.qty,
                "price": o.price,
                "status": o.status,
                "purpose": o.purpose,
                "client_order_id": o.client_order_id,
                "exchange_order_id": o.exchange_order_id,
                "reduce_only": o.reduce_only,
                "mode": o.mode,
            }
            for o in rows
        ]
    return {"items": items}


@router.get("/engine/protection")
async def protection_orders(user: str = Depends(current_user)) -> dict:
    """交易所侧的保护单(Algo 条件单).

    这部分本地数据库看不到: 下单后挂在交易所, 只有触发/撤销才消失.
    """
    try:
        rows = await binance_client.open_algo_orders()
    except BinanceError as exc:
        # 限频冷却期间降级为空列表而不是 5xx: 前端每 8s 轮询, 报错只会刷屏;
        # 保护单本身仍挂在交易所, 冷却结束后的下一次轮询即可恢复展示。
        if exc.code == -1003:
            return {"items": [], "degraded": True, "reason": f"交易所限频冷却中: {exc.message}"}
        raise HTTPException(status_code=503, detail=f"查询保护单失败: {exc.message}") from exc

    symbols = sorted({str(r.get("symbol")) for r in rows if isinstance(r, dict) and r.get("symbol")})
    quotes: dict[str, tuple[float, float]] = {}
    if symbols:
        try:
            quotes = await binance_client.book_tickers(symbols)
        except BinanceError:
            quotes = {}

    items: list[dict] = []
    for r in rows:
        if not isinstance(r, dict):
            continue
        sym = str(r.get("symbol") or "")
        tick = quotes.get(sym)
        mid = (tick[0] + tick[1]) / 2 if tick else 0.0
        trigger = float(r.get("triggerPrice") or r.get("stopPrice") or 0)
        items.append(
            {
                "algo_id": str(r.get("algoId") or ""),
                "client_algo_id": str(r.get("clientAlgoId") or ""),
                "symbol": sym,
                "side": str(r.get("side") or ""),
                "type": str(r.get("orderType") or r.get("type") or ""),
                "trigger_price": trigger,
                "mark_price": round(mid, 6),
                "distance_pct": round((trigger - mid) / mid * 100, 3) if mid and trigger else 0.0,
                "status": str(r.get("algoStatus") or r.get("status") or ""),
                "close_position": bool(r.get("closePosition")),
            }
        )
    items.sort(key=lambda x: (x["symbol"], x["type"]))
    return {"items": items, "count": len(items), "mode": normalize_mode(get_engine().mode)}


@router.delete("/engine/protection/{algo_id}")
async def cancel_protection(algo_id: str, user: str = Depends(current_user)) -> dict:
    """撤销一条交易所保护单."""
    try:
        await binance_client.cancel_algo(algo_id)
    except BinanceError as exc:
        raise HTTPException(status_code=400, detail=f"撤单失败: {exc.message}") from exc
    events.record("warn", "engine", f"手动撤销保护单 {algo_id}", {"algo_id": algo_id})
    return {"ok": True, "algo_id": algo_id}


@router.get("/engine/trades")
def trades(limit: int = Query(default=60, ge=1, le=500), user: str = Depends(current_user)) -> dict:
    mode = normalize_mode(get_engine().mode)
    with session_scope() as s:
        rows = (
            s.query(TradeRecord)
            .filter(TradeRecord.mode == mode)
            .order_by(TradeRecord.closed_at.desc())
            .limit(limit)
            .all()
        )
        items = [
            {
                "id": t.id,
                "symbol": t.symbol,
                "side": t.side,
                "qty": t.qty,
                "entry_price": t.entry_price,
                "exit_price": t.exit_price,
                "leverage": t.leverage,
                "notional": t.notional,
                "pnl": round(t.pnl, 8),
                "pnl_pct": round(t.pnl_pct, 4),
                "roi_pct": round(t.roi_pct, 4),
                "fees": round(t.fees, 8),
                "opened_at": t.opened_at,
                "closed_at": t.closed_at,
                "hold_seconds": t.hold_seconds,
                "exit_reason": t.exit_reason,
                "ai_confidence": t.ai_confidence,
                "ai_reason": t.ai_reason,
                "signal_score": t.signal_score,
                "mode": t.mode,
                "reviewed": t.reviewed,
                "review_note": t.review_note,
                "mantra_ids": t.mantra_ids or [],
            }
            for t in rows
        ]
    return {"items": items, "stats": trade_stats(mode=mode)}


@router.get("/engine/signals")
def signals(
    limit: int = Query(default=120, ge=1, le=1000),
    symbol: str = "",
    decision: str = "",
    direction: str = "",
    user: str = Depends(current_user),
) -> dict:
    """信号审计: 每轮扫描的量化信号与最终裁决 (可解释的拒绝原因)."""
    from collections import Counter

    mode = normalize_mode(get_engine().mode)
    with session_scope() as s:
        # 只审计当前交易环境的信号: 模拟盘与实盘不能混在同一条流水里
        base = s.query(Signal).filter(Signal.mode == mode)
        if symbol:
            base = base.filter(Signal.symbol == symbol.upper())
        if direction:
            base = base.filter(Signal.direction == direction)

        from sqlalchemy import func

        summary = {
            "total": base.count(),
            "opened": base.filter(Signal.decision == "opened").count(),
            "rejected": base.filter(Signal.decision == "rejected").count(),
            "flat": base.filter(Signal.decision == "flat").count(),
            "pending": base.filter(Signal.decision == "pending").count(),
            # flat 信号会合并重复扫描，scans 才是真实扫描次数
            "scans": int(base.with_entities(func.coalesce(func.sum(Signal.repeats), 0)).scalar() or 0),
        }
        reason_rows = (
            base.filter(Signal.decision == "rejected", Signal.reject_reason != "")
            .with_entities(Signal.reject_reason, Signal.repeats)
            .all()
        )
        weighted: Counter[str] = Counter()
        for reason, repeats in reason_rows:
            weighted[reason] += max(1, int(repeats or 1))
        summary["top_reasons"] = [
            {"reason": reason, "count": count} for reason, count in weighted.most_common(8)
        ]

        # 信号密度按小时分布 (近 7 天): 合并行按 repeats 加权，仍是"真实扫描次数"
        since = int(time.time()) - 7 * 86400
        hour_rows = (
            base.filter(Signal.ts >= since)
            .with_entities(
                func.strftime("%H", func.datetime(Signal.ts, "unixepoch")).label("hour"),
                func.coalesce(func.sum(Signal.repeats), 0),
            )
            .group_by("hour")
            .all()
        )
        buckets = {int(hour): int(count) for hour, count in hour_rows if hour is not None}
        summary["by_hour"] = [{"hour": h, "count": buckets.get(h, 0)} for h in range(24)]

        q = base
        if decision:
            q = q.filter(Signal.decision == decision)
        rows = q.order_by(Signal.ts.desc()).limit(limit).all()
        items = [
            {
                "id": s_.id,
                "ts": s_.ts,
                "symbol": s_.symbol,
                "direction": s_.direction,
                "score": s_.score,
                "features": s_.features,
                "ai_action": s_.ai_action,
                "ai_confidence": s_.ai_confidence,
                "ai_reason": s_.ai_reason,
                "decision": s_.decision,
                "reject_reason": s_.reject_reason,
                "repeats": int(s_.repeats or 1),
            }
            for s_ in rows
        ]
    return {"items": items, "summary": summary, "limit": limit}


@router.get("/engine/equity")
def equity(
    hours: int = Query(default=72, ge=1, le=720),
    user: str = Depends(current_user),
) -> dict:
    since = int(time.time()) - hours * 3600
    mode = normalize_mode(get_engine().mode)
    with session_scope() as s:
        rows = (
            s.query(EquityPoint)
            .filter(EquityPoint.ts >= since, EquityPoint.mode == mode)
            .order_by(EquityPoint.ts.asc())
            .all()
        )
        points = [
            {
                "ts": p.ts,
                "wallet": round(p.wallet, 6),
                "available": round(p.available, 6),
                "unrealized": round(p.unrealized, 6),
                "margin_used": round(p.margin_used, 6),
                "positions": p.positions,
                "mode": p.mode,
            }
            for p in rows
        ]
    return {"items": points, "hours": hours}


@router.get("/engine/stats")
def stats(user: str = Depends(current_user)) -> dict:
    mode = normalize_mode(get_engine().mode)
    data = trade_stats(mode=mode)
    data["cooldowns"] = state.active_cooldowns(int(time.time()))
    data["engine"] = {
        "enabled": get_engine().enabled,
        "mode": mode,
        "last_cycle_ts": state.get_int("last_cycle_ts", 0),
        "last_error": state.get("last_error"),
    }
    return data
