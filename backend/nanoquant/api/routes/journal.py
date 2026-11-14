"""决策日志 → 打标 → RAG 经验库 的只读/触发接口."""
from __future__ import annotations

from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query

from ...journal import (
    archive_labeled,
    delete_experience as remove_experience,
    experience_stats,
    journal_stats,
    label_due,
    list_experiences,
    list_journal,
    retrieve_top_k,
)
from ...journal.experience import derive_scenario, derive_tags
from ...services import events
from ..deps import current_user

router = APIRouter(tags=["journal"])


@router.get("/journal")
def journal_list(
    limit: int = Query(default=200, ge=1, le=1000),
    status: str = Query(default="", pattern="^(pending|labeled|skipped|)$"),
    symbol: str = "",
    mode: str = Query(default="", pattern="^(demo|live|)$"),
    user: str = Depends(current_user),
) -> dict:
    return {
        "items": list_journal(limit=limit, status=status, symbol=symbol, mode=mode),
        "stats": journal_stats(mode=mode),
    }


@router.get("/journal/stats")
def journal_summary(user: str = Depends(current_user)) -> dict:
    return {"journal": journal_stats(), "experience": experience_stats()}


@router.post("/journal/label")
async def journal_label(user: str = Depends(current_user)) -> dict:
    """手动触发回访打标（正常情况下由后台循环自动执行）。"""
    result = await label_due()
    events.record("info", "journal", f"{user} 手动触发决策打标: {result.get('labeled', 0)} 条")
    return result


@router.post("/journal/archive")
def journal_archive(user: str = Depends(current_user)) -> dict:
    """手动触发经验归档（正常情况下由后台循环自动执行）。"""
    result = archive_labeled()
    events.record(
        "info", "journal",
        f"{user} 手动触发经验归档: 新增 {result.get('archived', 0)} 条",
    )
    return result


@router.get("/journal/experiences")
def experiences(
    limit: int = Query(default=300, ge=1, le=1000),
    symbol: str = "",
    user: str = Depends(current_user),
) -> dict:
    return {
        "items": list_experiences(limit=limit, symbol=symbol),
        "stats": experience_stats(),
    }


@router.get("/journal/retrieve")
def retrieve_preview(
    symbol: str = Query(..., description="合约"),
    rsi: float | None = Query(default=None, ge=0, le=100),
    adx: float | None = Query(default=None, ge=0, le=100),
    atr_pct: float | None = Query(default=None, ge=0, le=50),
    user: str = Depends(current_user),
) -> dict[str, Any]:
    """预览某标的在给定场景下会检索到哪些经验（与决策注入同一套打分）。"""
    snapshot = {k: v for k, v in {"rsi": rsi, "adx": adx, "atr_pct": atr_pct}.items() if v is not None}
    tags = derive_tags(symbol, snapshot)
    scenario = derive_scenario(snapshot)
    return {
        "symbol": symbol,
        "tags": tags,
        "scenario": scenario,
        "items": retrieve_top_k(symbol, scenario, tags),
    }


@router.delete("/journal/experiences/{experience_id}")
def delete_experience(experience_id: int, user: str = Depends(current_user)) -> dict:
    if not remove_experience(experience_id):
        raise HTTPException(status_code=404, detail="经验不存在")
    events.record("warn", "journal", f"{user} 删除经验 #{experience_id}")
    return {"ok": True}
