"""资讯: 列表 / 刷新 / 情绪总览 / AI 综述 / 源诊断."""
from __future__ import annotations

from fastapi import APIRouter, Depends, Query

from ...news import service as news_service
from ..deps import current_user

router = APIRouter(tags=["news"])


@router.get("/news")
def list_news(
    limit: int = Query(default=60, ge=1, le=300),
    source: str = "",
    symbol: str = "",
    since: int = 0,
    user: str = Depends(current_user),
) -> dict:
    return {
        "items": news_service.list_items(limit=limit, source=source, symbol=symbol.upper(), since=since),
        "sources": news_service.source_catalog(),
    }


@router.post("/news/refresh")
async def refresh(user: str = Depends(current_user)) -> dict:
    return await news_service.refresh(force=True)


@router.get("/news/overview")
def overview(user: str = Depends(current_user)) -> dict:
    return news_service.overview()


@router.get("/news/diagnostics")
async def diagnostics(user: str = Depends(current_user)) -> dict:
    """逐个源体检: 状态码/耗时/条数/失败原因/修复建议."""
    return await news_service.diagnostics()


@router.post("/news/ai-summary")
async def ai_summary(user: str = Depends(current_user)) -> dict:
    return await news_service.ai_summary()
