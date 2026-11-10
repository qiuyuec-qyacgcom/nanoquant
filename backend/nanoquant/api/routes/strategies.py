"""策略模块: 标的粗筛策略 CRUD + 实时预览.

引擎每轮先按启用的策略过滤标的池，命中才拉 K 线做深度分析；
一条策略都没启用时默认全池扫描。
"""
from __future__ import annotations

from fastapi import APIRouter, Depends, HTTPException

from ...db import session_scope
from ...exchange.client import BinanceError, NoCredentials
from ...exchange.client import client as binance_client
from ...models import TradingSymbol
from ...services import events, state
from ...services import strategy_profiles as strategy_service
from ...services.settings import get_engine
from ..deps import current_user
from ..schemas import StrategyIn, StrategyPatch

router = APIRouter(tags=["strategies"])


@router.get("/strategies")
def list_strategies(user: str = Depends(current_user)) -> dict:
    return {
        "items": strategy_service.list_profiles(),
        "stats": strategy_service.stats(),
        "last_filter": __import__("json").loads(state.get("strategy_filter", "{}") or "{}"),
    }


@router.post("/strategies")
def create_strategy(payload: StrategyIn, user: str = Depends(current_user)) -> dict:
    try:
        item = strategy_service.create_profile(
            name=payload.name,
            description=payload.description,
            enabled=payload.enabled,
            priority=payload.priority,
            filters=payload.filters,
        )
    except strategy_service.StrategyError as exc:
        raise HTTPException(status_code=400, detail=exc.detail) from exc
    events.record("info", "strategy", f"{user} 新增策略: {item['name']}")
    return {"ok": True, "item": item}


@router.patch("/strategies/{profile_id}")
def patch_strategy(profile_id: int, payload: StrategyPatch, user: str = Depends(current_user)) -> dict:
    try:
        item = strategy_service.update_profile(profile_id, payload.model_dump(exclude_none=True))
    except strategy_service.StrategyError as exc:
        raise HTTPException(status_code=400, detail=exc.detail) from exc
    if item is None:
        raise HTTPException(status_code=404, detail="策略不存在")
    events.record("info", "strategy", f"{user} 更新策略: {item['name']}")
    return {"ok": True, "item": item}


@router.delete("/strategies/{profile_id}")
def delete_strategy(profile_id: int, user: str = Depends(current_user)) -> dict:
    if not strategy_service.delete_profile(profile_id):
        raise HTTPException(status_code=404, detail="策略不存在")
    events.record("warn", "strategy", f"{user} 删除策略 #{profile_id}")
    return {"ok": True}


@router.get("/strategies/preview")
async def preview_strategies(user: str = Depends(current_user)) -> dict:
    """用当前行情预览会放行哪些标的（只读，手动触发，不写库）."""
    engine_cfg = get_engine()
    with session_scope() as s:
        pool = [
            r.symbol
            for r in s.query(TradingSymbol).filter(TradingSymbol.enabled.is_(True)).all()
        ]
    pool = pool[: engine_cfg.max_symbols_per_cycle]
    profiles = strategy_service.active_profiles()
    if not pool:
        return {"ok": True, "default": not profiles, "items": [], "summary": {"kept": [], "dropped": []}}
    try:
        tickers = await binance_client.tickers(pool)
    except (BinanceError, NoCredentials) as exc:
        return {"ok": False, "error": f"行情取数失败: {str(exc)[:160]}", "items": [], "summary": {}}
    books: dict[str, tuple[float, float]] = {}
    if any(strategy_service.normalize_filters(p.get("filters"))["max_spread_bps"] > 0 for p in profiles):
        try:
            books = await binance_client.book_tickers(pool)
        except (BinanceError, NoCredentials):
            books = {}
    picked = strategy_service.prefilter(pool, tickers, books, profiles)
    kept = set(picked["kept"])
    items = []
    for symbol in pool:
        ticker = tickers.get(symbol)
        if ticker is None:
            continue
        items.append(
            {
                "symbol": symbol,
                "price": ticker.last,
                "change_24h_pct": ticker.change_pct,
                "quote_volume": ticker.quote_volume,
                "kept": symbol in kept,
                "side_bias": picked["side_bias"].get(symbol, "any"),
            }
        )
    return {
        "ok": True,
        "default": picked["default"],
        "items": items,
        "summary": {
            "kept": picked["kept"],
            "dropped": picked["dropped"],
            "profiles": picked["profiles"],
            "matched": picked["matched"],
        },
    }
