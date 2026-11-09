"""思考强度（reasoning effort）到各家协议请求参数的映射 + 推理型模型库.

模型库参考本地 `astra-quant-agent` 的 `capabilities._detect_reasoning_type`
（2026-09 反馈：qwen3 全系、claude-4、kimi-k3、glm-5、minimax-m2、mimo-v2
都应按推理型处理），并补充 gpt-oss / grok 等 OpenAI 兼容网关常见模型。

协议映射：
- Anthropic `/messages`：`thinking = {type: enabled, budget_tokens: N}`，
  N = low 2048 / medium 8000 / high 16000 / xhigh 32000 / max 64000；
  `none` 显式 disabled；开启思考时 `max_tokens` 必须大于 budget。
- OpenAI 兼容：`reasoning_effort`（仅推理型模型才注入，普通模型传了会被 400）。
- `auto` 不注入，交给服务端默认。

库只用于「是否注入」与前端提示；服务端不认时由 client 自动移除重试。
"""
from __future__ import annotations

from dataclasses import dataclass
from typing import Any

EFFORT_LEVELS = ("auto", "none", "minimal", "low", "medium", "high", "xhigh", "max")
EXTREME_LEVELS = ("xhigh", "max")

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


@dataclass(frozen=True)
class ReasoningFamily:
    """一个推理模型族：命中任一 hint 即视为支持 reasoning_effort."""

    key: str
    label: str
    hints: tuple[str, ...]
    #: 是否支持 xhigh/max 两档（绝大多数模型只到 high）
    extreme: bool = False
    #: 备注，展示在前端「支持的模型族」里
    note: str = ""


#: 维护入口：新增模型族只改这张表（顺序无关，越具体的放越前）
REASONING_FAMILIES: tuple[ReasoningFamily, ...] = (
    ReasoningFamily("openai-gpt6", "OpenAI GPT-6 / ChatGPT-6", ("gpt-6", "chatgpt-6"), True),
    ReasoningFamily("openai-gpt5", "OpenAI GPT-5", ("gpt-5",), True),
    ReasoningFamily("openai-o-series", "OpenAI o1 / o3 / o4", ("o1", "o3", "o4"), True),
    ReasoningFamily("gpt-oss", "GPT-OSS 开源推理", ("gpt-oss",), False, "vLLM/Ollama 常见"),
    ReasoningFamily(
        "anthropic-claude",
        "Anthropic Claude 3.7+ / 4",
        ("claude-3-7", "claude-3.7", "claude-4", "claude-opus-4", "claude-sonnet-4"),
        False,
        "Anthropic 协议走 thinking 预算",
    ),
    ReasoningFamily("google-gemini", "Google Gemini 2.5 / 3", ("gemini-2.5", "gemini-2-5", "gemini-3"), False),
    ReasoningFamily("deepseek-v4", "DeepSeek V4 / V4.1", ("deepseek-v4", "v4.1"), False),
    ReasoningFamily("deepseek-r1", "DeepSeek R1 / Reasoner", ("deepseek-r1", "deepseek-reasoner", "-r1"), False),
    ReasoningFamily("qwen3", "Qwen3 全系 / QwQ", ("qwen3", "qwen-3", "qwq"), False, "含 qwen3-vl"),
    ReasoningFamily("kimi", "Kimi K2.7 / K3", ("kimi-k2.7", "kimi-k3"), False),
    ReasoningFamily("glm", "GLM-5", ("glm-5",), False),
    ReasoningFamily("minimax", "MiniMax M2", ("minimax-m2",), False),
    ReasoningFamily("mimo", "MiMo V2", ("mimo-v2",), False),
    ReasoningFamily("grok", "xAI Grok 3 mini / 4", ("grok-3-mini", "grok-4"), True),
)

#: 明确不支持 reasoning_effort 的模型族（避免子串误判，例如 qwen2.5 / gpt-4o）
_NON_REASONING_HINTS = (
    "gpt-4o", "gpt-4.1", "gpt-3", "qwen2.5", "qwen-2.5", "llama", "mistral",
    "-chat", "instruct",
)

_LABELS_ZH = {
    "auto": "自动 (auto)",
    "none": "关闭 (none)",
    "minimal": "极简 (minimal)",
    "low": "低 (low)",
    "medium": "中 (medium)",
    "high": "高 (high)",
    "xhigh": "超高 (xhigh)",
    "max": "极值 (max)",
}


def normalize(value: Any) -> str:
    text = str(value or "").strip().lower()
    return text if text in EFFORT_LEVELS else "auto"


def match_family(model: str) -> ReasoningFamily | None:
    low = (model or "").lower()
    if not low or any(hint in low for hint in _NON_REASONING_HINTS):
        return None
    for family in REASONING_FAMILIES:
        if any(hint in low for hint in family.hints):
            return family
    return None


def supports_effort(model: str) -> bool:
    return match_family(model) is not None


def supports_extreme_effort(model: str) -> bool:
    family = match_family(model)
    return bool(family and family.extreme)


def effort_options(model: str) -> list[dict[str, Any]]:
    """前端下拉用：标记每档是否可用（不支持时置灰而不是隐藏，避免用户困惑）."""
    extreme = supports_extreme_effort(model)
    return [
        {
            "value": value,
            "label": _LABELS_ZH[value],
            "requires_extreme": value in EXTREME_LEVELS,
            "enabled": value not in EXTREME_LEVELS or extreme,
        }
        for value in EFFORT_LEVELS
    ]


def library() -> dict[str, Any]:
    """给 `GET /ai/reasoning-models` 的只读快照."""
    return {
        "levels": list(EFFORT_LEVELS),
        "extreme_levels": list(EXTREME_LEVELS),
        "families": [
            {
                "key": f.key,
                "label": f.label,
                "hints": list(f.hints),
                "extreme": f.extreme,
                "note": f.note,
            }
            for f in REASONING_FAMILIES
        ],
        "non_reasoning_hints": list(_NON_REASONING_HINTS),
    }


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
