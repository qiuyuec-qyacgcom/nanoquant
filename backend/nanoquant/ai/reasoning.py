"""思考强度（reasoning effort）到各家协议请求参数的映射.

参考本地 `astra-quant-agent` 的模型设置：
- Anthropic `/messages`：`thinking = {type: enabled, budget_tokens: N}`，
  N = low 2048 / medium 8000 / high 16000 / xhigh 32000 / max 64000；
  `none` 显式 disabled；开启思考时 `max_tokens` 必须大于 budget。
- OpenAI 兼容：`reasoning_effort`（o1/o3/o4/gpt-5/gpt-6/gemini/qwen3/qwq/
  deepseek-v4/kimi-k/glm-5 等推理型模型才注入，普通模型传了会被 400）。
- `auto` 不注入，交给服务端默认。

本项目只做「注入 + 不可用时降级」两件事；是否真的产生思考由模型决定。
"""
from __future__ import annotations

from typing import Any

EFFORT_LEVELS = ("auto", "none", "minimal", "low", "medium", "high", "xhigh", "max")

#: Anthropic thinking budget（tokens）
ANTHROPIC_BUDGET = {
    "minimal": 1024,
    "low": 2048,
    "medium": 8000,
    "high": 16000,
    "xhigh": 32000,
    "max": 64000,
}
#: Anthropic 单次 max_tokens 上限保护，避免 budget + 输出预算超过模型上限
ANTHROPIC_MAX_TOKENS_CAP = 64000

#: 只有这些模型族/网关认 reasoning_effort；其余模型传了会 400
_STANDARD_HINTS = (
    "o1", "o3", "o4", "gpt-5", "gpt-6", "chatgpt-6",
    "gemini", "qwen3", "qwen-3", "qwq",
    "deepseek-v4", "v4.1", "deepseek-r1", "reasoner",
    "kimi-k", "glm-5",
)


def normalize(value: Any) -> str:
    text = str(value or "").strip().lower()
    return text if text in EFFORT_LEVELS else "auto"


def supports_effort(model: str) -> bool:
    low = (model or "").lower()
    return any(hint in low for hint in _STANDARD_HINTS)


def apply_effort(
    payload: dict[str, Any],
    *,
    protocol: str,
    model: str,
    effort: str,
    base_max_tokens: int,
) -> list[str]:
    """就地修改 payload，返回注入的键（供 400/422 时整体回退）。

    - `auto`：不动
    - `anthropic_messages`：thinking + 调整 max_tokens
    - `openai_responses`：`reasoning: {effort}`
    - `openai_chat`：`reasoning_effort`，仅对识别为推理模型的才加
    """
    level = normalize(effort)
    if level == "auto":
        return []

    if protocol == "anthropic_messages":
        if level == "none":
            payload["thinking"] = {"type": "disabled"}
            return ["thinking"]
        budget = ANTHROPIC_BUDGET.get(level, ANTHROPIC_BUDGET["medium"])
        # Anthropic 要求 max_tokens > thinking.budget_tokens；触到上限时下调 budget 而不是超发
        wanted = budget + max(256, int(base_max_tokens or 0))
        max_tokens = min(wanted, ANTHROPIC_MAX_TOKENS_CAP)
        if max_tokens <= budget:
            budget = max(1024, max_tokens - 256)
        payload["thinking"] = {"type": "enabled", "budget_tokens": budget}
        payload["max_tokens"] = max_tokens
        return ["thinking", "max_tokens"]

    if protocol == "openai_responses":
        mapped = {"xhigh": "high", "max": "high", "minimal": "low"}.get(level, level)
        payload["reasoning"] = {"effort": mapped}
        return ["reasoning"]

    # openai_chat 及兼容网关
    if not supports_effort(model):
        return []
    payload["reasoning_effort"] = level
    return ["reasoning_effort"]
