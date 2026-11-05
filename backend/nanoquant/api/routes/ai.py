"""AI 模型接入配置 / 诊断 / 单币分析 / 用量."""
from __future__ import annotations

import time
from typing import Any

from fastapi import APIRouter, Depends, HTTPException, Query

from ...ai.client import ai_client, list_models
from ...ai.decision import analyze
from ...ai.diagnostics import diagnose
from ...db import session_scope
from ...exchange.client import BinanceError, NoCredentials
from ...exchange.client import client as binance_client
from ...models import AiCallLog, AiProvider
from ...news import service as news_service
from ...security import decrypt_secret, encrypt_secret, mask_secret
from ...services import events
from ...services.settings import get_ai, update_group
from ...engine.strategy import build_features
from ...services.settings import get_strategy
from ..deps import current_user
from ..schemas import AnalyzeIn, ProviderIn, ProviderPatch, TestProviderIn

router = APIRouter(tags=["ai"])


def _serialize(p: AiProvider) -> dict[str, Any]:
    raw_key = decrypt_secret(p.api_key_enc)
    return {
        "id": p.id,
        "name": p.name,
        "protocol": p.protocol,
        "base_url": p.base_url,
        "model": p.model,
        "temperature": p.temperature,
        "max_tokens": p.max_tokens,
        "timeout_sec": p.timeout_sec,
        "json_mode": p.json_mode,
        "enabled": p.enabled,
        "priority": p.priority,
        "role": p.role,
        "api_key_masked": mask_secret(raw_key),
        "has_key": bool(raw_key),
        "extra": p.extra or {},
        "created_at": p.created_at,
        "updated_at": p.updated_at,
    }


@router.get("/ai/providers")
def list_providers(user: str = Depends(current_user)) -> dict:
    with session_scope() as s:
        rows = s.query(AiProvider).order_by(AiProvider.priority.asc(), AiProvider.id.asc()).all()
        items = [_serialize(p) for p in rows]
    cfg = get_ai()
    return {"items": items, "active": {"mode": cfg.mode, "provider_id": cfg.provider_id, "fallbacks": cfg.fallback_provider_ids, "enabled": cfg.enabled}}


@router.post("/ai/providers")
def create_provider(payload: ProviderIn, user: str = Depends(current_user)) -> dict:
    with session_scope() as s:
        if s.query(AiProvider).filter(AiProvider.name == payload.name).first():
            raise HTTPException(status_code=400, detail="同名模型配置已存在")
        row = AiProvider(
            name=payload.name,
            protocol=payload.protocol,
            base_url=payload.base_url.rstrip("/"),
            api_key_enc=encrypt_secret(payload.api_key),
            model=payload.model,
            temperature=payload.temperature,
            max_tokens=payload.max_tokens,
            timeout_sec=payload.timeout_sec,
            json_mode=payload.json_mode,
            enabled=payload.enabled,
            priority=payload.priority,
            role=payload.role,
            extra=payload.extra,
        )
        s.add(row)
        s.flush()
        provider_id = row.id
    cfg = get_ai()
    if not cfg.provider_id:
        update_group("ai", {"provider_id": provider_id})
    events.record("info", "ai", f"{user} 新增 AI 模型 {payload.name} ({payload.model})")
    return {"ok": True, "id": provider_id}


@router.patch("/ai/providers/{provider_id}")
def patch_provider(provider_id: int, payload: ProviderPatch, user: str = Depends(current_user)) -> dict:
    data = payload.model_dump(exclude_unset=True)
    api_key = data.pop("api_key", None)
    with session_scope() as s:
        row = s.get(AiProvider, provider_id)
        if row is None:
            raise HTTPException(status_code=404, detail="模型配置不存在")
        for k, v in data.items():
            if v is not None:
                setattr(row, k, v)
        if api_key is not None and api_key != "":
            row.api_key_enc = encrypt_secret(api_key)
    events.record("info", "ai", f"{user} 更新 AI 模型 #{provider_id}")
    return {"ok": True}


@router.delete("/ai/providers/{provider_id}")
def delete_provider(provider_id: int, user: str = Depends(current_user)) -> dict:
    with session_scope() as s:
        row = s.get(AiProvider, provider_id)
        if row is None:
            raise HTTPException(status_code=404, detail="模型配置不存在")
        s.delete(row)
    cfg = get_ai()
    if cfg.provider_id == provider_id:
        update_group("ai", {"provider_id": 0, "fallback_provider_ids": [i for i in cfg.fallback_provider_ids if i != provider_id]})
    events.record("warn", "ai", f"{user} 删除 AI 模型 #{provider_id}")
    return {"ok": True}


@router.post("/ai/providers/{provider_id}/test")
async def test_provider(provider_id: int, payload: TestProviderIn, user: str = Depends(current_user)) -> dict:
    with session_scope() as s:
        provider = s.get(AiProvider, provider_id)
        if provider is None:
            raise HTTPException(status_code=404, detail="模型配置不存在")
        s.expunge(provider)
    result = await ai_client.chat(
        [{"role": "user", "content": payload.prompt}],
        provider=provider,
        purpose="test",
        json_mode=False,
        retries=0,
    )
    return {
        "ok": result.ok,
        "text": result.text[:2000],
        "error": result.error,
        "latency_ms": result.latency_ms,
        "model": result.model,
        "prompt_tokens": result.prompt_tokens,
        "completion_tokens": result.completion_tokens,
        "cost_usd": result.cost_usd,
        "attempts": result.attempts,
    }


@router.get("/ai/providers/{provider_id}/diagnose")
async def diagnose_provider(provider_id: int, deep: bool = True, user: str = Depends(current_user)) -> dict:
    return await diagnose(provider_id, deep=deep)


@router.get("/ai/providers/{provider_id}/models")
async def provider_models(provider_id: int, user: str = Depends(current_user)) -> dict:
    with session_scope() as s:
        provider = s.get(AiProvider, provider_id)
        if provider is None:
            raise HTTPException(status_code=404, detail="模型配置不存在")
        s.expunge(provider)
    ok, models = await list_models(provider)
    return {"ok": ok, "models": models if ok else [], "error": "" if ok else str(models)}


@router.post("/ai/analyze")
async def analyze_symbol(payload: AnalyzeIn, user: str = Depends(current_user)) -> dict:
    symbol = payload.symbol.upper()
    strat = get_strategy()
    try:
        candles = await binance_client.klines(symbol, payload.interval or strat.timeframe, 120)
        tickers = await binance_client.tickers([symbol])
        funding = (await binance_client.funding_rates([symbol])).get(symbol, 0.0)
    except (BinanceError, NoCredentials) as exc:
        raise HTTPException(status_code=503, detail=f"币安行情不可用: {exc}") from exc
    if len(candles) < 30:
        raise HTTPException(status_code=400, detail="K 线数据不足, 无法分析")
    ticker = tickers.get(symbol)
    if ticker is None:
        raise HTTPException(status_code=400, detail="行情不可用")
    features = build_features(
        candles, strat, funding=funding, change_24h=ticker.change_pct, quote_volume=ticker.quote_volume
    )
    snapshot = {
        "price": ticker.last,
        "change_24h": ticker.change_pct,
        "quote_volume": ticker.quote_volume,
        "features": features,
        "recent_bars": [
            {"t": c.open_time, "o": c.open, "h": c.high, "l": c.low, "c": c.close, "v": c.volume} for c in candles[-30:]
        ],
        "source": "binance",
    }
    news_items = news_service.digest(8) if payload.include_news else []
    result = await analyze(symbol, snapshot, news_items)
    return {"symbol": symbol, "snapshot": snapshot, **result}


@router.post("/ai/decision-preview")
async def decision_preview(payload: AnalyzeIn, user: str = Depends(current_user)) -> dict:
    """不触发下单的决策演练: 用真实上下文跑一遍 AI 决策, 便于调参."""
    from ...ai.decision import decide
    from ...services.settings import get_ai, get_risk

    symbol = payload.symbol.upper()
    strat = get_strategy()
    try:
        candles = await binance_client.klines(symbol, payload.interval or strat.timeframe, 120)
        tickers = await binance_client.tickers([symbol])
    except (BinanceError, NoCredentials) as exc:
        raise HTTPException(status_code=503, detail=f"币安行情不可用: {exc}") from exc
    if len(candles) < 30:
        raise HTTPException(status_code=400, detail="K 线数据不足")
    ticker = tickers.get(symbol)
    if ticker is None:
        raise HTTPException(status_code=400, detail="行情不可用")
    try:
        funding = (await binance_client.funding_rates([symbol])).get(symbol, 0.0)
    except BinanceError:
        funding = 0.0
    features = build_features(candles, strat, funding=funding, change_24h=ticker.change_pct, quote_volume=ticker.quote_volume)
    from ...evolution import mantra as mantra_store

    ai_cfg = get_ai()
    risk_cfg = get_risk()
    ctx = {
        "symbol": symbol,
        "price": ticker.last,
        "score": features.get("ema_gap_pct", 0) / 0.22,
        "side_hint": "long" if features.get("ema_fast", 0) > features.get("ema_slow", 0) else "short",
        "quant_reasons": list(features.keys())[:6],
        "features": features,
        "klines": [{"o": c.open, "h": c.high, "l": c.low, "c": c.close} for c in candles[-14:]],
        "funding": funding,
        "change_24h": ticker.change_pct,
        "quote_volume": ticker.quote_volume,
        "account": {"equity": 1.0, "available": 1.0, "open_positions": 0},
        "constraints": {"max_leverage": risk_cfg.max_leverage, "min_confidence": ai_cfg.min_confidence},
        "news": news_service.digest(6) if payload.include_news else [],
        "mantras": [{"title": m["title"], "rule": m["rule"]} for m in mantra_store.rank_lessons()],
    }
    outcome = await decide(ctx, purpose="preview")
    return {"symbol": symbol, "context_score": round(ctx["score"], 4), **outcome.as_dict()}


@router.get("/ai/logs")
def ai_logs(limit: int = Query(default=60, ge=1, le=500), purpose: str = "", user: str = Depends(current_user)) -> dict:
    with session_scope() as s:
        q = s.query(AiCallLog).order_by(AiCallLog.ts.desc())
        if purpose:
            q = q.filter(AiCallLog.purpose.like(f"{purpose}%"))
        rows = q.limit(limit).all()
        items = [
            {
                "id": r.id,
                "ts": r.ts,
                "provider": r.provider,
                "model": r.model,
                "purpose": r.purpose,
                "ok": r.ok,
                "latency_ms": r.latency_ms,
                "prompt_tokens": r.prompt_tokens,
                "completion_tokens": r.completion_tokens,
                "cost_usd": r.cost_usd,
                "error": r.error,
            }
            for r in rows
        ]
    return {"items": items}


@router.get("/ai/usage")
def ai_usage(user: str = Depends(current_user)) -> dict:
    now = int(time.time())
    since_24h = now - 86400
    since_30d = now - 30 * 86400
    with session_scope() as s:
        rows = s.query(AiCallLog).filter(AiCallLog.ts >= since_30d).all()
    by_day: dict[str, dict[str, Any]] = {}
    by_provider: dict[str, dict[str, Any]] = {}
    total_cost = 0.0
    for r in rows:
        day = time.strftime("%Y-%m-%d", time.localtime(r.ts))
        d = by_day.setdefault(day, {"calls": 0, "ok": 0, "cost": 0.0, "tokens": 0})
        d["calls"] += 1
        d["ok"] += 1 if r.ok else 0
        d["cost"] = round(d["cost"] + r.cost_usd, 6)
        d["tokens"] += r.prompt_tokens + r.completion_tokens
        p = by_provider.setdefault(r.provider or "unknown", {"calls": 0, "ok": 0, "cost": 0.0, "errors": 0})
        p["calls"] += 1
        p["ok"] += 1 if r.ok else 0
        p["errors"] += 0 if r.ok else 1
        p["cost"] = round(p["cost"] + r.cost_usd, 6)
        total_cost += r.cost_usd
    cfg = get_ai()
    return {
        "by_day": [{"date": k, **v} for k, v in sorted(by_day.items())],
        "by_provider": [{"provider": k, **v} for k, v in sorted(by_provider.items(), key=lambda x: -x[1]["calls"])],
        "total_30d": {
            "calls": len(rows),
            "cost_usd": round(total_cost, 6),
            "calls_24h": sum(1 for r in rows if r.ts >= since_24h),
        },
        "budget": {"monthly_usd": cfg.monthly_budget_usd, "used_usd": round(total_cost, 6), "max_calls_per_hour": cfg.max_calls_per_hour},
    }
