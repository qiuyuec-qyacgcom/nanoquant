"""标的池接口: 添加/删除规则迁移自 astra-quant-agent (instrument_pool).

- 候选与添加都实时校验币安在线合约表, 没有任何本地演示清单;
- 删除必须带确认短语 `REMOVE <SYMBOL>`, 有持仓一律拒绝;
- 池内规格 (tier / leverage / precision / tickSz / minSz / minNotional) 随添加落库.
"""
from __future__ import annotations

from fastapi import APIRouter, Body, Depends, HTTPException, Query

from ...db import session_scope
from ...models import TradingSymbol
from ...services.symbols import (
    POOL_ORDER,
    PoolError,
    add_many,
    add_symbol,
    candidates,
    delete_symbol,
    pool_state,
)
from ..deps import current_user
from ..schemas import SymbolBulkIn, SymbolDeleteIn, SymbolIn, SymbolPatch

router = APIRouter(tags=["symbols"])


def _row(item: TradingSymbol) -> dict:
    return {
        "symbol": item.symbol,
        "name": item.name or item.symbol[:-4],
        "tier": item.tier,
        "sl_atr_mult": item.sl_atr_mult,
        "enabled": item.enabled,
        "max_leverage": item.max_leverage,
        "precision": item.precision,
        "tick_sz": item.tick_sz,
        "min_sz": item.min_sz,
        "min_notional": item.min_notional,
        "ct_val": item.ct_val,
        "min_score": item.min_score,
        "note": item.note or "",
        "sort_order": int(item.sort_order or 0),
        "created_at": item.created_at,
        "updated_at": item.updated_at,
    }


@router.get("/symbols")
def list_symbols(user: str = Depends(current_user)) -> dict:
    with session_scope() as s:
        rows = s.query(TradingSymbol).order_by(*POOL_ORDER).all()
        items = [_row(r) for r in rows]
    state = pool_state()
    return {
        "items": items,
        "count": len(items),
        "state": state,
        "limits": state["limits"],
    }


@router.get("/symbols/available")
async def available_symbols(
    q: str = Query(default="", description="合约名关键词, 支持 btc / btc-usdt / BTCUSDT"),
    limit: int = Query(default=50, ge=1, le=200),
    user: str = Depends(current_user),
) -> dict:
    """候选合约: 来自币安在线合约表 (非行情), 带分层与最小约束."""
    try:
        items = await candidates(q, limit=limit)
    except PoolError as exc:
        raise HTTPException(status_code=exc.status, detail=exc.detail) from exc
    return {
        "items": items,
        "query": q,
        "hint": "" if items else "未匹配到可交易的币安 USDT 永续合约，请检查合约名",
    }


@router.post("/symbols")
async def add(payload: SymbolIn, user: str = Depends(current_user)) -> dict:
    try:
        row = await add_symbol(payload.symbol)
    except PoolError as exc:
        raise HTTPException(status_code=exc.status, detail=exc.detail) from exc
    return {"ok": True, "added": row, "count": pool_state()["count"]}


@router.post("/symbols/bulk")
async def add_bulk(payload: SymbolBulkIn, user: str = Depends(current_user)) -> dict:
    result = await add_many(payload.symbols)
    result["ok"] = True
    result["count"] = pool_state()["count"]
    return result


@router.patch("/symbols/{symbol}")
def patch_symbol(symbol: str, payload: SymbolPatch, user: str = Depends(current_user)) -> dict:
    upper = symbol.upper()
    with session_scope() as s:
        row = s.get(TradingSymbol, upper)
        if row is None:
            raise HTTPException(status_code=404, detail="标的未在池中")
        for key, value in payload.model_dump(exclude_none=True).items():
            setattr(row, key, value)
    return {"ok": True}


@router.delete("/symbols/{symbol}")
async def delete(
    symbol: str,
    payload: SymbolDeleteIn | None = Body(default=None),
    confirmation: str = Query(default="", description="确认短语: REMOVE <SYMBOL>"),
    user: str = Depends(current_user),
) -> dict:
    phrase = (payload.confirmation if payload else "") or confirmation
    try:
        result = await delete_symbol(symbol, phrase)
    except PoolError as exc:
        raise HTTPException(status_code=exc.status, detail=exc.detail) from exc
    return {"ok": True, **result}
