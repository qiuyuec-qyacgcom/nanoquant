"""结构化决策: JSON 提取 -> 容错修复 -> 强校验 -> 多模型议会聚合.

fail-closed 原则: 解析不出来就当 hold, 绝不"猜"一个仓.
"""
from __future__ import annotations

import json
import re
from dataclasses import dataclass, field
from typing import Any

from pydantic import BaseModel, Field, ValidationError

from ..logging_setup import get_logger
from ..services.settings import get_ai
from ..services import events
from .client import ai_client
from .compact import build_decision_messages, is_compact_payload, localize

log = get_logger("ai.decision")

_FENCE_RE = re.compile(r"```(?:json)?\s*(.*?)```", re.S | re.I)
_TRAILING_COMMA_RE = re.compile(r",\s*([}\]])")


class AiDecision(BaseModel):
    action: str = "hold"
    confidence: float = 0.0
    leverage: float = 0.0
    sl_pct: float = 0.0
    tp_pct: float = 0.0
    reason: str = ""
    risk_flags: list[str] = Field(default_factory=list)
    invalidation: str = ""

    def normalized(self) -> AiDecision:
        action = (self.action or "hold").strip().lower()
        mapping = {
            "long": "open_long",
            "buy": "open_long",
            "openlong": "open_long",
            "short": "open_short",
            "sell": "open_short",
            "openshort": "open_short",
            "no_trade": "hold",
            "none": "hold",
            "wait": "hold",
        }
        action = mapping.get(action, action)
        if action not in {"open_long", "open_short", "hold", "close"}:
            action = "hold"
        return AiDecision(
            action=action,
            confidence=max(0.0, min(1.0, float(self.confidence or 0))),
            leverage=max(0.0, min(125.0, float(self.leverage or 0))),
            sl_pct=max(0.0, min(50.0, float(self.sl_pct or 0))),
            tp_pct=max(0.0, min(100.0, float(self.tp_pct or 0))),
            reason=str(self.reason or "")[:800],
            risk_flags=[str(x)[:120] for x in (self.risk_flags or [])][:6],
            invalidation=str(self.invalidation or "")[:400],
        )


@dataclass
class DecisionOutcome:
    decision: AiDecision
    ok: bool = False
    error: str = ""
    provider: str = ""
    model: str = ""
    latency_ms: int = 0
    cost_usd: float = 0.0
    agreement: float = 0.0
    raw_text: str = ""
    schema_version: int = 1
    votes: list[dict[str, Any]] = field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        return {
            "schema_version": self.schema_version,
            "raw_text": self.raw_text[:2000],
            "ok": self.ok,
            "action": self.decision.action,
            "confidence": self.decision.confidence,
            "leverage": self.decision.leverage,
            "sl_pct": self.decision.sl_pct,
            "tp_pct": self.decision.tp_pct,
            "reason": self.decision.reason,
            "risk_flags": self.decision.risk_flags,
            "invalidation": self.decision.invalidation,
            "provider": self.provider,
            "model": self.model,
            "latency_ms": self.latency_ms,
            "cost_usd": self.cost_usd,
            "agreement": round(self.agreement, 4),
            "error": self.error,
            "votes": self.votes,
        }


def extract_json(text: str) -> dict[str, Any]:
    """从模型输出中提取 JSON 对象, 允许代码围栏与前后散文."""
    if not text or not text.strip():
        raise ValueError("模型返回为空")
    raw = text.strip()
    fence = _FENCE_RE.search(raw)
    if fence:
        raw = fence.group(1).strip()
    try:
        data = json.loads(raw)
        if isinstance(data, dict):
            return data
    except json.JSONDecodeError:
        pass
    start = raw.find("{")
    end = raw.rfind("}")
    if start >= 0 and end > start:
        candidate = raw[start : end + 1]
        for attempt in (candidate, _TRAILING_COMMA_RE.sub(r"\1", candidate)):
            try:
                data = json.loads(attempt)
                if isinstance(data, dict):
                    return data
            except json.JSONDecodeError:
                continue
    raise ValueError("无法从模型输出中解析出 JSON 对象")


def parse_decision(text: str, ctx: dict[str, Any] | None = None) -> AiDecision:
    """v1 中文详细 JSON / v2 英文紧凑 JSON 自动探测，统一产出中文 AiDecision."""
    data = extract_json(text)
    if is_compact_payload(data):
        data = localize(data, ctx)
    try:
        return AiDecision(**data).normalized()
    except ValidationError as exc:
        raise ValueError(f"决策字段校验失败: {exc.errors()[0].get('msg')}") from exc


def _direction(action: str) -> int:
    return 1 if action == "open_long" else (-1 if action == "open_short" else 0)


def _aggregate(votes: list[tuple[float, AiDecision]]) -> tuple[AiDecision, float]:
    total_w = sum(w for w, _ in votes) or 1.0
    score = sum(w * _direction(d.action) * max(0.05, d.confidence) for w, d in votes) / total_w
    if score >= 0.25:
        action = "open_long"
    elif score <= -0.25:
        action = "open_short"
    else:
        action = "hold"
    same = [d for _, d in votes if d.action == action]
    agreement = sum(w for w, d in votes if d.action == action) / total_w
    conf = sum(d.confidence for d in same) / len(same) if same else 0.0
    leverages = [d.leverage for d in same if d.leverage > 0]
    sls = [d.sl_pct for d in same if d.sl_pct > 0]
    tps = [d.tp_pct for d in same if d.tp_pct > 0]
    merged = AiDecision(
        action=action,
        confidence=round(conf * (0.6 + 0.4 * agreement), 4),
        leverage=round(sum(leverages) / len(leverages), 2) if leverages else 0.0,
        sl_pct=round(sum(sls) / len(sls), 3) if sls else 0.0,
        tp_pct=round(sum(tps) / len(tps), 3) if tps else 0.0,
        reason=(same[0].reason if same else "议会未形成一致意见")[:800],
        risk_flags=sorted({f for _, d in votes for f in d.risk_flags})[:6],
        invalidation=(same[0].invalidation if same else ""),
    )
    return merged, round(agreement, 4)


async def decide(ctx: dict[str, Any], *, purpose: str = "decision") -> DecisionOutcome:
    cfg = get_ai()
    if not cfg.enabled:
        return DecisionOutcome(decision=AiDecision(action="hold", reason="AI 未启用"), error="ai_disabled")

    if cfg.mode == "committee":
        votes: list[tuple[float, AiDecision]] = []
        meta: list[dict[str, Any]] = []
        total_latency = 0
        total_cost = 0.0
        provider = ""
        model = ""
        for persona in cfg.personas:
            if not persona.enabled or persona.weight <= 0:
                continue
            messages = build_decision_messages(ctx, version=cfg.decision_schema_version, persona=persona.name, focus=persona.focus)
            result = await ai_client.chat_with_fallback(
                messages,
                purpose=f"{purpose}:{persona.name}",
                provider_id=cfg.provider_id,
                fallback_ids=cfg.fallback_provider_ids,
                temperature=cfg.temperature,
            )
            total_latency += result.latency_ms
            total_cost += result.cost_usd
            if result.ok:
                provider = result.provider or provider
                model = result.model or model
                try:
                    vote = parse_decision(result.text, ctx)
                    votes.append((persona.weight, vote))
                    meta.append({"persona": persona.name, "weight": persona.weight, "action": vote.action, "confidence": vote.confidence})
                except ValueError as exc:
                    meta.append({"persona": persona.name, "weight": persona.weight, "error": str(exc)})
            else:
                meta.append({"persona": persona.name, "weight": persona.weight, "error": result.error})
        if not votes:
            return DecisionOutcome(
                decision=AiDecision(action="hold", reason="议会全部席位调用失败"),
                error="全部席位失败", latency_ms=total_latency, votes=meta,
                schema_version=cfg.decision_schema_version,
            )
        merged, agreement = _aggregate(votes)
        return DecisionOutcome(
            decision=merged, ok=True, provider=provider, model=model,
            latency_ms=total_latency, cost_usd=round(total_cost, 6),
            agreement=agreement, votes=meta, schema_version=cfg.decision_schema_version,
        )

    result = await ai_client.chat_with_fallback(
        build_decision_messages(ctx, version=cfg.decision_schema_version),
        purpose=purpose,
        provider_id=cfg.provider_id,
        fallback_ids=cfg.fallback_provider_ids,
        temperature=cfg.temperature,
    )
    if not result.ok:
        events.record("warn", "ai", f"AI 决策失败: {result.error[:160]}", {"purpose": purpose})
        return DecisionOutcome(
            decision=AiDecision(action="hold", reason="AI 调用失败, 按 fail-closed 处理"),
            error=result.error, latency_ms=result.latency_ms, provider=result.provider,
            model=result.model, votes=[{"error": result.error}],
            schema_version=cfg.decision_schema_version,
        )
    try:
        decision = parse_decision(result.text, ctx)
    except ValueError as exc:
        events.record("warn", "ai", f"AI 输出不可解析: {exc}", {"raw": result.text[:400], "purpose": purpose, "schema": cfg.decision_schema_version})
        return DecisionOutcome(
            decision=AiDecision(action="hold", reason="AI 输出不符合 JSON 契约, 按 fail-closed 处理"),
            error=str(exc), provider=result.provider, model=result.model,
            latency_ms=result.latency_ms, cost_usd=result.cost_usd,
            raw_text=result.text[:2000], votes=[{"error": str(exc)}],
            schema_version=cfg.decision_schema_version,
        )
    return DecisionOutcome(
        decision=decision, ok=True, provider=result.provider, model=result.model,
        latency_ms=result.latency_ms, cost_usd=result.cost_usd,
        agreement=1.0, raw_text=result.text[:2000], votes=[{"persona": "single", "action": decision.action, "confidence": decision.confidence}],
        schema_version=cfg.decision_schema_version,
    )


async def analyze(symbol: str, snapshot: dict[str, Any], news: list[dict[str, Any]] | None = None) -> dict[str, Any]:
    """面板上的单币种深度分析 (不参与自动交易)."""
    cfg = get_ai()
    from .prompts import analyze_messages

    result = await ai_client.chat_with_fallback(
        analyze_messages(symbol, snapshot, news),
        purpose="analyze",
        provider_id=cfg.provider_id,
        fallback_ids=cfg.fallback_provider_ids,
        temperature=min(0.7, cfg.temperature + 0.1),
    )
    if not result.ok:
        return {"ok": False, "error": result.error, "provider": result.provider}
    try:
        data = extract_json(result.text)
    except ValueError as exc:
        return {"ok": False, "error": str(exc), "raw": result.text[:1200], "provider": result.provider}
    return {
        "ok": True,
        "provider": result.provider,
        "model": result.model,
        "latency_ms": result.latency_ms,
        "cost_usd": result.cost_usd,
        "analysis": data,
    }
