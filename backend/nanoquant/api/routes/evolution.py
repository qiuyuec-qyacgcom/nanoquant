"""自进化: 心法库 / 复盘执行 / 注入预览."""
from __future__ import annotations

from fastapi import APIRouter, Depends, HTTPException, Query

from ...evolution import mantra as mantra_store
from ...evolution import review as evolution_review
from ...services import events
from ..deps import current_user
from ..schemas import MantraIn, MantraPatch

router = APIRouter(tags=["evolution"])


@router.get("/evolution/mantras")
def list_mantras(
    include_disabled: bool = True,
    category: str = "",
    limit: int = Query(default=300, ge=1, le=1000),
    user: str = Depends(current_user),
) -> dict:
    return {
        "items": mantra_store.list_mantras(include_disabled=include_disabled, category=category, limit=limit),
        "stats": mantra_store.stats(),
    }


@router.post("/evolution/mantras")
def create_mantra(payload: MantraIn, user: str = Depends(current_user)) -> dict:
    result = mantra_store.upsert_lesson(
        title=payload.title,
        rule=payload.rule,
        category=payload.category,
        confidence=payload.confidence,
        tags=payload.tags,
        evidence={"source": "manual", "by": user},
        source="manual",
    )
    if result["action"] == "rejected":
        raise HTTPException(status_code=400, detail=result["reason"])
    events.record("info", "evolution", f"{user} 手工新增心法: {payload.title[:40]}")
    return result


@router.patch("/evolution/mantras/{mantra_id}")
def patch_mantra(mantra_id: int, payload: MantraPatch, user: str = Depends(current_user)) -> dict:
    data = mantra_store.update_mantra(mantra_id, payload.model_dump(exclude_none=True))
    if data is None:
        raise HTTPException(status_code=404, detail="心法不存在")
    return {"ok": True, "mantra": data}


@router.delete("/evolution/mantras/{mantra_id}")
def delete_mantra(mantra_id: int, user: str = Depends(current_user)) -> dict:
    if not mantra_store.delete_mantra(mantra_id):
        raise HTTPException(status_code=404, detail="心法不存在")
    events.record("warn", "evolution", f"{user} 删除心法 #{mantra_id}")
    return {"ok": True}


@router.post("/evolution/mantras/dedupe")
async def dedupe_evolution_mantras(use_ai: bool = True, user: str = Depends(current_user)) -> dict:
    """合并存量重复心法：默认由 AI 判断语义重复，失败回退字符相似度."""
    result = await evolution_review.dedupe_mantras(use_ai=use_ai)
    events.record(
        "info", "evolution",
        f"{user} 合并重复心法({result.get('mode')}): {len(result.get('replaced') or [])} 条并入 {result.get('kept')} 条",
        {"replaced": (result.get("replaced") or [])[:20]},
    )
    return result


@router.get("/evolution/injection")
def injection_preview(user: str = Depends(current_user)) -> dict:
    text, detail = mantra_store.injection()
    return {
        "text": text, "injected": detail, "count": len(detail),
        "stats": mantra_store.stats(), "report": mantra_store.injection_report(),
    }


@router.get("/evolution/runs")
def runs(limit: int = Query(default=40, ge=1, le=200), user: str = Depends(current_user)) -> dict:
    return {"items": evolution_review.list_runs(limit)}


@router.post("/evolution/review/{trade_id}")
async def review_trade(trade_id: int, force: bool = True, user: str = Depends(current_user)) -> dict:
    result = await evolution_review.review_trade(trade_id, force=force)
    if not result.get("ok"):
        raise HTTPException(status_code=400, detail=result.get("error") or "复盘失败")
    return result


@router.post("/evolution/review-batch")
async def review_batch(
    limit: int = Query(default=0, ge=0, le=100),
    force: bool = False,
    user: str = Depends(current_user),
) -> dict:
    """手动触发批量复盘：攒够样本才调模型；NO_CHANGE 与 AI 失败都不写心法."""
    result = await evolution_review.review_batch(limit=limit or None, force=force)
    if not result.get("ok"):
        raise HTTPException(status_code=400, detail=result.get("error") or "批量复盘失败")
    events.record(
        "info", "evolution",
        f"{user} 触发批量复盘: {result.get('sample_size', 0)} 笔, {result.get('change_status')}",
        {"applied": result.get("applied")},
    )
    return result


@router.post("/evolution/health-refresh")
def health_refresh(user: str = Depends(current_user)) -> dict:
    """按最近平仓证据重算全部心法健康分并按需归档（只读面板之外的人工触发入口）."""
    return mantra_store.refresh_health_from_evidence()


@router.post("/evolution/global-review")
async def global_review(user: str = Depends(current_user)) -> dict:
    result = await evolution_review.global_review()
    if not result.get("ok"):
        raise HTTPException(status_code=400, detail=result.get("error") or "全局复盘失败")
    return result


@router.get("/evolution/stats")
def stats(user: str = Depends(current_user)) -> dict:
    return {
        "mantras": mantra_store.stats(),
        "trades": evolution_review.trade_stats(),
    }


@router.get("/evolution/effectiveness")
def effectiveness(
    limit: int = Query(default=500, ge=1, le=2000),
    mode: str = Query(default="", pattern="^(demo|live|)$"),
    user: str = Depends(current_user),
) -> dict:
    """采用心法 vs 未采用的观察性对照（详见 evolution/effectiveness.py 的口径说明）."""
    from ...evolution.effectiveness import evaluate

    return evaluate(limit=limit, mode=mode)
