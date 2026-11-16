"""AI 诊断探针：推理模型需要足够的 max_tokens，否则永远报「基础对话失败」.

用户实测：deepseek-v4-pro 诊断时 max_tokens=32，思考链没结束就被截断，
`finish_reason=length` → output_problem → 诊断失败。修复后探针用 4096。
"""
import inspect

from nanoquant.ai.client import output_problem
from nanoquant.ai import diagnostics


def test_diagnostic_probes_use_generous_budget():
    """从源码 AST 读实参：两个探针的 max_tokens 都必须 >= 2048。"""
    import ast

    src = inspect.getsource(diagnostics.diagnose)
    tree = ast.parse(src)
    budgets = []
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and getattr(node.func, "attr", "") == "chat":
            for kw in node.keywords:
                if kw.arg == "max_tokens" and isinstance(kw.value, ast.Constant):
                    budgets.append(kw.value.value)
    assert len(budgets) >= 2, f"应有两个诊断探针，实际 {budgets}"
    assert all(b >= 2048 for b in budgets), f"探针预算太小: {budgets}"


def test_small_budget_truncation_hint_is_actionable():
    msg = output_problem("", "length", 32)
    assert "max_tokens=32" in msg
    assert "探针预算偏小" in msg, msg
    assert "升级到最新版" in msg


def test_large_budget_truncation_keeps_original_hint():
    msg = output_problem("", "length", 4096)
    assert "max_tokens=4096" in msg
    assert "提到 3000+" in msg


def test_empty_reasoning_unchanged():
    msg = output_problem("", "stop", 4096)
    assert "thinking/reasoning" in msg
