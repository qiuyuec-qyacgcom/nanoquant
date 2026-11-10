"""策略模块：AI 分析策略 CRUD + 适用范围预览.

策略是给 AI 的交易指令，在决策阶段注入（不是标的池预筛）。
未配置策略时 AI 按通用铁律分析，即默认行为。
"""
from __future__ import annotations

from fastapi import APIRouter, Depends, HTTPException

from ...db import session_scope
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
        "last_applied": state.get("strategy_applied", ""),
    }


@router.post("/strategies")
def create_strategy(payload: StrategyIn, user: str = Depends(current_user)) -> dict:
    try:
        item = strategy_service.create_profile(
            name=payload.name,
            description=payload.description,
            rule=payload.rule,
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
def preview_strategies(user: str = Depends(current_user)) -> dict:
    """按标的池预览每个标的会应用哪条策略（纯本地匹配，不拉行情/不写库）."""
    engine_cfg = get_engine()
    with session_scope() as s:
        pool = [
            r.symbol
            for r in s.query(TradingSymbol).filter(TradingSymbol.enabled.is_(True)).all()
        ]
    pool = pool[: engine_cfg.max_symbols_per_cycle]
    profiles = strategy_service.active_profiles()
    items = []
    for symbol in pool:
        picked = strategy_service.pick_from(symbol, profiles)
        filters = strategy_service.normalize_filters((picked or {}).get("filters"))
        items.append(
            {
                "symbol": symbol,
                "profile": picked["name"] if picked else "",
                "side": filters["side"] if picked else "any",
                "default": picked is None,
            }
        )
    return {
        "ok": True,
        "default": len(profiles) == 0,
        "profiles": len(profiles),
        "items": items,
    }
