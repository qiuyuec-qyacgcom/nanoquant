"""AI 接口诊断: 连通性 / 鉴权 / 模型列表 / JSON 模式 / 时延 / 成本探测."""
from __future__ import annotations

import time
from typing import Any

from ..db import session_scope
from ..models import AiProvider
from ..security import decrypt_secret
from .client import _join_url, ai_client, estimate_tokens, list_models


def _check(name: str, ok: bool, detail: str, latency_ms: int = 0, hint: str = "") -> dict[str, Any]:
    return {"name": name, "ok": ok, "detail": detail, "latency_ms": latency_ms, "hint": hint}


async def diagnose(provider_id: int, deep: bool = True) -> dict[str, Any]:
    with session_scope() as s:
        provider = s.get(AiProvider, provider_id)
        if provider is None:
            return {"ok": False, "error": "模型配置不存在"}
        s.expunge(provider)

    checks: list[dict[str, Any]] = []
    api_key = decrypt_secret(provider.api_key_enc)
    checks.append(_check("凭据", bool(api_key), "已配置 API Key" if api_key else "缺少 API Key", hint="在「AI 模型」页填写 key"))
    try:
        url = _join_url(provider.base_url, provider.protocol)
        checks.append(_check("接口地址", True, url))
    except Exception as exc:  # noqa: BLE001
        checks.append(_check("接口地址", False, str(exc), hint="base_url 需要形如 https://api.deepseek.com/v1"))
        return {"ok": False, "provider": provider.name, "checks": checks}

    checks.append(_check("模型", bool(provider.model), provider.model or "未填写模型名"))

    if deep:
        started = time.perf_counter()
        result = await ai_client.chat(
            [{"role": "user", "content": "只回答两个字: 正常"}],
            provider=provider,
            purpose="diagnose",
            max_tokens=32,
            temperature=0,
            json_mode=False,
            retries=0,
        )
        latency = int((time.perf_counter() - started) * 1000)
        checks.append(
            _check(
                "基础对话",
                result.ok,
                f"响应 {latency}ms, 返回: {(result.text or result.error)[:80]}",
                latency_ms=latency,
                hint="" if result.ok else "检查网络/代理、模型名与余额",
            )
        )

        if result.ok:
            json_started = time.perf_counter()
            json_result = await ai_client.chat(
                [{"role": "user", "content": '输出 JSON: {"ok":true}'}],
                provider=provider,
                purpose="diagnose",
                max_tokens=48,
                temperature=0,
                json_mode=True,
                retries=0,
            )
            json_latency = int((time.perf_counter() - json_started) * 1000)
            structured_ok = False
            if json_result.ok:
                try:
                    from .decision import extract_json

                    data = extract_json(json_result.text)
                    structured_ok = isinstance(data, dict)
                except ValueError:
                    structured_ok = False
            checks.append(
                _check(
                    "JSON 结构化输出",
                    structured_ok,
                    "支持 response_format=json_object" if structured_ok else (json_result.error or "返回内容不是合法 JSON"),
                    latency_ms=json_latency,
                    hint="" if structured_ok else "系统会自动降级为纯文本 + 容错解析",
                )
            )

        ok, models = await list_models(provider)
        checks.append(
            _check(
                "/models 列表",
                ok,
                f"发现 {len(models)} 个模型" if ok else str(models)[:160],
                hint="" if ok else "部分服务商不提供该接口, 不影响调用",
            )
        )

        checks.append(
            _check(
                "用量估算",
                True,
                f"单次决策约 {estimate_tokens('x' * 4200)} prompt tokens (按 4.2K 字符上下文估算)",
            )
        )

    overall = all(c["ok"] for c in checks if c["name"] in {"凭据", "接口地址", "模型", "基础对话"})
    return {
        "ok": overall,
        "provider": provider.name,
        "model": provider.model,
        "protocol": provider.protocol,
        "checks": checks,
        "checked_at": int(time.time()),
    }
