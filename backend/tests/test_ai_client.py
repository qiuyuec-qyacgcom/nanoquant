"""AI 调用层的纯函数：URL 拼接、响应抽取、成本估算、截断归因、模型端点探测.

这些是"改错一位就多收钱 / 解析必失败"的地方，且都能离线验证，不碰网络。
"""
from __future__ import annotations

import pytest


def test_estimate_tokens_bounds():
    from nanoquant.ai.client import estimate_tokens

    assert estimate_tokens("") == 0
    assert estimate_tokens("x") == 1, "空/短文本下限是 1"
    assert estimate_tokens("hello world") == 4  # int(11 / 2.6)


def test_estimate_cost_lookup_and_unknown():
    from nanoquant.ai.client import estimate_cost

    # gpt-4o: 0.0025 in / 0.01 out
    assert estimate_cost("gpt-4o", 1000, 1000) == pytest.approx(0.0125, abs=1e-8)
    # 前缀匹配：deepseek-chat 命中，deepseek-reasoner 是另一个价
    assert estimate_cost("deepseek-chat", 1000, 1000) == pytest.approx(0.00137, abs=1e-8)
    assert estimate_cost("no-such-model", 1000, 1000) == 0.0


def test_join_url_variants():
    from nanoquant.ai.client import _join_url

    assert _join_url("https://api.openai.com", "openai_chat") == "https://api.openai.com/v1/chat/completions"
    # 已带 /v1 版本段：不再补 /v1
    assert _join_url("https://api.openai.com/v1", "openai_chat") == "https://api.openai.com/v1/chat/completions"
    # 已是完整 chat/completions 路径：原样返回
    assert _join_url("https://api.deepseek.com/v1/chat/completions", "openai_chat") == "https://api.deepseek.com/v1/chat/completions"
    # 自定义多段前缀（如 /api/paas/v4）：尾段是 v4，直接补 /chat/completions
    assert _join_url("https://open.bigmodel.cn/api/paas/v4", "openai_chat") == "https://open.bigmodel.cn/api/paas/v4/chat/completions"
    # Anthropic 走 /v1/messages
    assert _join_url("https://api.anthropic.com", "anthropic_messages") == "https://api.anthropic.com/v1/messages"
    assert _join_url("https://api.anthropic.com/v1/messages", "anthropic_messages") == "https://api.anthropic.com/v1/messages"
    # openai_responses
    assert _join_url("https://api.openai.com/v1", "openai_responses") == "https://api.openai.com/v1/responses"

    with pytest.raises(ValueError):
        _join_url("", "openai_chat")


def test_extract_text_openai_chat():
    from nanoquant.ai.client import _extract_text

    text, usage = _extract_text(
        {"choices": [{"message": {"content": '{"a":1}'}}], "usage": {"prompt_tokens": 12, "completion_tokens": 5}},
        "openai_chat",
    )
    assert text == '{"a":1}'
    assert usage == {"prompt_tokens": 12, "completion_tokens": 5}


def test_extract_text_anthropic_blocks():
    from nanoquant.ai.client import _extract_text

    text, usage = _extract_text(
        {"content": [{"type": "text", "text": "hello"}, {"type": "text", "text": " world"}],
         "usage": {"input_tokens": 7, "output_tokens": 3}},
        "anthropic_messages",
    )
    assert text == "hello world"
    assert usage["prompt_tokens"] == 7 and usage["completion_tokens"] == 3


def test_extract_text_responses_output_text():
    from nanoquant.ai.client import _extract_text

    text, _ = _extract_text(
        {"output_text": "answer", "usage": {"input_tokens": 1, "output_tokens": 2}},
        "openai_responses",
    )
    assert text == "answer"


def test_output_problem_attribution():
    from nanoquant.ai.client import output_problem

    assert output_problem('{"a":1', "length", 512) != "", "截断要明确归因"
    assert "max_tokens" in output_problem("", "length", 512)
    assert "thinking" in output_problem("", "stop", 512), "只有思考过程要提示"
    assert output_problem('{"a":1}', "stop", 512) == "", "正常输出不误报"


def test_models_endpoints_candidates():
    from nanoquant.ai.client import _models_endpoints

    urls = [u for u, _ in _models_endpoints("https://api.deepseek.com/v1", "openai_chat")]
    assert "https://api.deepseek.com/v1/models" in urls
    assert "https://api.deepseek.com/models" in urls

    # Anthropic 会带上版本头
    cands = _models_endpoints("https://api.anthropic.com", "anthropic_messages")
    assert any(h.get("anthropic-version") for _, h in cands)

    # 已以 /models 结尾则只保留原样
    urls = [u for u, _ in _models_endpoints("https://x.com/v1/models", "openai_chat")]
    assert urls == ["https://x.com/v1/models"]


def test_dump_json_is_compact():
    from nanoquant.ai.client import dump_json

    assert dump_json({"a": 1, "b": "中文"}) == '{"a":1,"b":"中文"}'
