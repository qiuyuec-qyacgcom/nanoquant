"""大模型调用层: 多协议 / 重试 / 回退链 / 成本与用量留痕.

刻意不依赖官方 SDK: 只要兼容 OpenAI Chat Completions 的国产模型/自建网关
(DeepSeek、Qwen、Kimi、GLM、vLLM、OneAPI...) 都能直接接.
"""
from __future__ import annotations

import json
import time
from dataclasses import dataclass, field
from typing import Any

import httpx

from ..db import session_scope
from ..logging_setup import get_logger
from ..models import AiCallLog, AiProvider
from ..security import decrypt_secret
from ..services import state
from ..services.settings import get_ai

log = get_logger("ai.client")

# 常用模型参考单价 (USD / 1K tokens), 仅用于成本估算
PRICE_TABLE: dict[str, tuple[float, float]] = {
    "gpt-4o": (0.0025, 0.01),
    "gpt-4o-mini": (0.00015, 0.0006),
    "gpt-4.1": (0.002, 0.008),
    "gpt-4.1-mini": (0.0004, 0.0016),
    "o3-mini": (0.0011, 0.0044),
    "deepseek-chat": (0.00027, 0.0011),
    "deepseek-reasoner": (0.00055, 0.0022),
    "qwen-plus": (0.0004, 0.0012),
    "qwen-max": (0.0016, 0.0064),
    "glm-4-flash": (0.0, 0.0),
    "glm-4-plus": (0.0007, 0.0007),
    "moonshot-v1-8k": (0.0017, 0.0017),
    "claude-3-5-sonnet": (0.003, 0.015),
    "claude-3-haiku": (0.00025, 0.00125),
}


def estimate_tokens(text: str) -> int:
    """粗略估算: 中英混排约 2.6 字符/token."""
    if not text:
        return 0
    return max(1, int(len(text) / 2.6))


def estimate_cost(model: str, prompt_tokens: int, completion_tokens: int) -> float:
    key = next((k for k in PRICE_TABLE if model.lower().startswith(k)), "")
    if not key:
        return 0.0
    pin, pout = PRICE_TABLE[key]
    return round(prompt_tokens / 1000 * pin + completion_tokens / 1000 * pout, 8)


@dataclass
class ChatResult:
    ok: bool
    text: str = ""
    provider: str = ""
    provider_id: int = 0
    model: str = ""
    latency_ms: int = 0
    prompt_tokens: int = 0
    completion_tokens: int = 0
    cost_usd: float = 0.0
    error: str = ""
    attempts: list[dict[str, Any]] = field(default_factory=list)


def _join_url(base: str, protocol: str) -> str:
    base = (base or "").strip().rstrip("/")
    if not base:
        raise ValueError("base_url 为空")
    if protocol == "anthropic_messages":
        return base if base.endswith("/v1/messages") else f"{base}/v1/messages"
    if base.endswith("/chat/completions"):
        return base
    if protocol == "openai_responses":
        return base if base.endswith("/responses") else f"{base}/responses"
    # openai_chat: 若已带版本段则不再补 /v1
    tail = base.rsplit("/", 1)[-1]
    if tail.startswith("v") and tail[1:].isdigit():
        return f"{base}/chat/completions"
    if base.count("/") > 2:  # 已有自定义路径前缀 (如 /openai/v1)
        return f"{base}/chat/completions"
    return f"{base}/v1/chat/completions"


class AiClient:
    """无状态调用器, 每次按 provider 配置发请求."""

    def __init__(self) -> None:
        self._http: httpx.AsyncClient | None = None

    async def _client(self) -> httpx.AsyncClient:
        if self._http is None:
            self._http = httpx.AsyncClient(
                timeout=httpx.Timeout(180, connect=12),
                limits=httpx.Limits(max_connections=8, max_keepalive_connections=4),
            )
        return self._http

    async def close(self) -> None:
        if self._http is not None:
            await self._http.aclose()
            self._http = None

    # ------------------------------------------------------------ budget
    def budget_blocked(self) -> tuple[bool, str]:
        cfg = get_ai()
        hour = int(time.time() // 3600)
        bucket = state.get_int("ai_calls_hour_bucket", 0)
        count = state.get_int("ai_calls_hour_count", 0)
        if bucket != hour:
            state.set_many({"ai_calls_hour_bucket": str(hour), "ai_calls_hour_count": "0"})
            count = 0
        if cfg.max_calls_per_hour and count >= cfg.max_calls_per_hour:
            return True, f"本小时 AI 调用已达上限 {cfg.max_calls_per_hour}"
        if cfg.monthly_budget_usd > 0:
            spent = self._month_spend()
            if spent >= cfg.monthly_budget_usd:
                return True, f"本月 AI 预算已用尽 ({spent:.4f}/{cfg.monthly_budget_usd} USD)"
        return False, ""

    def _month_spend(self) -> float:
        start = int(time.time()) - 30 * 86400
        with session_scope() as s:
            rows = s.query(AiCallLog.cost_usd).filter(AiCallLog.ts >= start).all()
            return float(sum(r[0] or 0 for r in rows))

    def _bump_calls(self) -> None:
        hour = int(time.time() // 3600)
        if state.get_int("ai_calls_hour_bucket", 0) != hour:
            state.set_many({"ai_calls_hour_bucket": str(hour), "ai_calls_hour_count": "1"})
        else:
            state.set("ai_calls_hour_count", str(state.get_int("ai_calls_hour_count", 0) + 1))

    # ------------------------------------------------------------- call
    async def chat(
        self,
        messages: list[dict[str, str]],
        *,
        provider: AiProvider,
        purpose: str = "decision",
        temperature: float | None = None,
        max_tokens: int | None = None,
        json_mode: bool | None = None,
        retries: int = 2,
    ) -> ChatResult:
        cfg = get_ai()
        api_key = decrypt_secret(provider.api_key_enc)
        url = _join_url(provider.base_url, provider.protocol)
        want_json = provider.json_mode if json_mode is None else json_mode
        temp = provider.temperature if temperature is None else temperature
        mt = provider.max_tokens if max_tokens is None else max_tokens
        attempts: list[dict[str, Any]] = []
        last_error = ""

        payload: dict[str, Any] = {}
        headers: dict[str, str] = {"Content-Type": "application/json"}
        if provider.protocol == "anthropic_messages":
            system = "\n\n".join(m["content"] for m in messages if m["role"] == "system")
            payload = {
                "model": provider.model,
                "max_tokens": mt,
                "temperature": temp,
                "system": system,
                "messages": [m for m in messages if m["role"] != "system"],
            }
            headers.update({"x-api-key": api_key, "anthropic-version": "2023-06-01"})
        elif provider.protocol == "openai_responses":
            payload = {
                "model": provider.model,
                "input": "\n\n".join(f"[{m['role']}] {m['content']}" for m in messages),
                "temperature": temp,
            }
            headers["Authorization"] = f"Bearer {api_key}"
        else:
            payload = {
                "model": provider.model,
                "messages": messages,
                "temperature": temp,
                "max_tokens": mt,
                "stream": False,
            }
            if want_json:
                payload["response_format"] = {"type": "json_object"}
            headers["Authorization"] = f"Bearer {api_key}"

        for attempt in range(retries + 1):
            started = time.perf_counter()
            try:
                client = await self._client()
                r = await client.post(url, json=payload, headers=headers, timeout=provider.timeout_sec or cfg.timeout_sec)
                latency = int((time.perf_counter() - started) * 1000)
                if r.status_code >= 400:
                    body = r.text[:300]
                    # json_object 不被支持时降级重试一次
                    if r.status_code in (400, 422) and "response_format" in payload and attempt == 0:
                        payload.pop("response_format", None)
                        attempts.append({"status": r.status_code, "note": "服务端不支持 json_object, 降级为纯文本", "latency_ms": latency})
                        continue
                    attempts.append({"status": r.status_code, "note": body, "latency_ms": latency})
                    last_error = f"HTTP {r.status_code}: {body}"
                    if r.status_code in (401, 403):
                        break
                    await _sleep_backoff(attempt)
                    continue
                data = r.json()
                text, usage = _extract_text(data, provider.protocol)
                pt = int(usage.get("prompt_tokens") or 0) or estimate_tokens(" ".join(m["content"] for m in messages))
                ct = int(usage.get("completion_tokens") or 0) or estimate_tokens(text)
                cost = estimate_cost(provider.model, pt, ct)
                self._bump_calls()
                self._log(
                    provider=provider, purpose=purpose, ok=True, latency=latency,
                    pt=pt, ct=ct, cost=cost, error="",
                )
                attempts.append({"status": 200, "latency_ms": latency})
                return ChatResult(
                    ok=True, text=text, provider=provider.name, provider_id=provider.id,
                    model=provider.model, latency_ms=latency, prompt_tokens=pt,
                    completion_tokens=ct, cost_usd=cost, attempts=attempts,
                )
            except Exception as exc:  # noqa: BLE001
                latency = int((time.perf_counter() - started) * 1000)
                last_error = f"{type(exc).__name__}: {exc}"
                attempts.append({"status": -1, "note": last_error[:200], "latency_ms": latency})
                await _sleep_backoff(attempt)

        self._log(
            provider=provider, purpose=purpose, ok=False, latency=attempts[-1].get("latency_ms", 0) if attempts else 0,
            pt=0, ct=0, cost=0.0, error=last_error[:400],
        )
        return ChatResult(
            ok=False, provider=provider.name, provider_id=provider.id, model=provider.model,
            error=last_error[:400], attempts=attempts,
        )

    async def chat_with_fallback(
        self,
        messages: list[dict[str, str]],
        *,
        purpose: str = "decision",
        provider_id: int = 0,
        fallback_ids: list[int] | None = None,
        **kwargs: Any,
    ) -> ChatResult:
        blocked, reason = self.budget_blocked()
        if blocked:
            return ChatResult(ok=False, error=reason)
        chain = _provider_chain(provider_id, fallback_ids or [])
        if not chain:
            return ChatResult(ok=False, error="未配置可用的 AI 模型, 请先在「AI 模型」中接入并启用")
        merged_attempts: list[dict[str, Any]] = []
        for provider in chain:
            result = await self.chat(messages, provider=provider, purpose=purpose, **kwargs)
            merged_attempts.extend(result.attempts)
            result.attempts = merged_attempts
            if result.ok:
                return result
            log.warning("模型 %s 调用失败, 继续回退链: %s", provider.name, result.error[:160])
        return ChatResult(ok=False, error=result.error if chain else "无可用模型", attempts=merged_attempts)

    def _log(
        self, *, provider: AiProvider, purpose: str, ok: bool, latency: int,
        pt: int, ct: int, cost: float, error: str,
    ) -> None:
        try:
            with session_scope() as s:
                s.add(
                    AiCallLog(
                        provider=provider.name, model=provider.model, purpose=purpose,
                        ok=ok, latency_ms=latency, prompt_tokens=pt, completion_tokens=ct,
                        cost_usd=cost, error=error[:480],
                    )
                )
        except Exception as exc:  # pragma: no cover
            log.error("AI 调用留痕失败: %s", exc)


def providers_available() -> int:
    """已启用且配置了 key 的模型数量."""
    with session_scope() as s:
        rows = s.query(AiProvider).filter(AiProvider.enabled.is_(True)).all()
        return sum(1 for r in rows if decrypt_secret(r.api_key_enc))


def _provider_chain(primary_id: int, fallback_ids: list[int]) -> list[AiProvider]:
    with session_scope() as s:
        rows = s.query(AiProvider).filter(AiProvider.enabled.is_(True)).order_by(AiProvider.priority.asc()).all()
        by_id = {r.id: r for r in rows}
        chain: list[AiProvider] = []
        if primary_id and primary_id in by_id:
            chain.append(by_id[primary_id])
        for fid in fallback_ids:
            if fid in by_id and by_id[fid] not in chain:
                chain.append(by_id[fid])
        for row in rows:
            if row not in chain:
                chain.append(row)
        return chain[:5]


def _extract_text(data: dict[str, Any], protocol: str) -> tuple[str, dict[str, Any]]:
    if protocol == "anthropic_messages":
        blocks = data.get("content") or []
        text = "".join(b.get("text", "") for b in blocks if isinstance(b, dict))
        usage = {
            "prompt_tokens": (data.get("usage") or {}).get("input_tokens", 0),
            "completion_tokens": (data.get("usage") or {}).get("output_tokens", 0),
        }
        return text, usage
    if protocol == "openai_responses":
        text = data.get("output_text") or ""
        if not text:
            for item in data.get("output", []) or []:
                for part in item.get("content", []) or []:
                    text += part.get("text", "")
        usage = {
            "prompt_tokens": (data.get("usage") or {}).get("input_tokens", 0),
            "completion_tokens": (data.get("usage") or {}).get("output_tokens", 0),
        }
        return text, usage
    choices = data.get("choices") or []
    text = ""
    if choices:
        msg = choices[0].get("message") or {}
        text = msg.get("content") or ""
        if not text and msg.get("reasoning_content"):
            text = msg.get("reasoning_content") or ""
    usage = data.get("usage") or {}
    return text, {
        "prompt_tokens": usage.get("prompt_tokens", 0),
        "completion_tokens": usage.get("completion_tokens", 0),
    }


async def _sleep_backoff(attempt: int) -> None:
    import asyncio

    await asyncio.sleep(min(4.0, 0.8 * (2**attempt)))


ai_client = AiClient()


async def list_models(provider: AiProvider) -> tuple[bool, Any]:
    """拉取 /models 列表 (OpenAI 兼容协议)."""
    api_key = decrypt_secret(provider.api_key_enc)
    base = provider.base_url.rstrip("/")
    url = f"{base}/models" if "/v1" in base or base.count("/") > 2 else f"{base}/v1/models"
    try:
        async with httpx.AsyncClient(timeout=15) as client:
            r = await client.get(url, headers={"Authorization": f"Bearer {api_key}"})
            if r.status_code != 200:
                return False, f"HTTP {r.status_code}: {r.text[:160]}"
            data = r.json()
            items = data.get("data") or data.get("models") or []
            return True, sorted({str(i.get("id") or i.get("name")) for i in items if isinstance(i, dict)})
    except Exception as exc:  # noqa: BLE001
        return False, f"{type(exc).__name__}: {exc}"


def dump_json(data: Any) -> str:
    return json.dumps(data, ensure_ascii=False, separators=(",", ":"))
