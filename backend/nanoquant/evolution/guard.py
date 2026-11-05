"""心法防污染审计.

模型很容易"学会"放宽风控 (加大杠杆、取消止损), 这类结论一旦入库会持续毒化决策,
因此在写库前做硬性拦截, 并保留 rejected 记录供人工复核.
"""
from __future__ import annotations

import re
from typing import Any

from ..services.settings import get_evolution

DANGER_PATTERNS: list[tuple[str, str]] = [
    (r"加大杠杆|提高杠杆|杠杆拉满|满杠杆", "试图提高杠杆"),
    (r"取消止损|不要止损|不设止损|移除止损|撤销止损", "试图取消止损"),
    (r"放宽止损|扩大止损|止损放大", "试图放宽止损"),
    (r"满仓|梭哈|all[- ]?in|重仓|全仓压上", "试图重仓"),
    (r"无视风控|忽略风控|绕过风控|关闭风控", "试图绕过风控"),
    (r"稳赚|必涨|必跌|保证盈利|百分百|零风险", "绝对化承诺"),
    (r"加仓摊平|亏损加仓", "亏损摊平"),
    (r"追涨杀跌|追高", "情绪化追单"),
]

CATEGORY_WHITELIST = {"entry", "exit", "risk", "timing", "position", "market"}


def audit(title: str, rule: str, category: str, confidence: float) -> tuple[bool, str]:
    cfg = get_evolution()
    title = (title or "").strip()
    rule = (rule or "").strip()
    if len(title) < 4:
        return False, "标题过短"
    if len(rule) < 12:
        return False, "规则过短, 缺少可执行条件"
    if len(rule) > cfg.max_rule_chars:
        return False, f"规则超过 {cfg.max_rule_chars} 字"
    if category not in CATEGORY_WHITELIST:
        return False, f"未知分类 {category}"
    if confidence < cfg.min_confidence:
        return False, f"置信度 {confidence:.2f} 低于阈值 {cfg.min_confidence}"
    if not cfg.guard_strict:
        return True, ""
    text = f"{title} {rule}"
    for pattern, reason in DANGER_PATTERNS:
        if re.search(pattern, text, re.I):
            return False, f"命中风险规则: {reason}"
    # 规则必须包含可判定的触发条件词
    if not re.search(r"当|若|如果|一旦|在.*时|when|if", text, re.I):
        return False, "缺少触发条件 (当/若/一旦...)"
    return True, ""


def sanitize(text: str) -> str:
    """清除控制字符与多余空白, 防止污染 prompt 结构."""
    text = re.sub(r"[\x00-\x08\x0b\x0c\x0e-\x1f]", "", text or "")
    return re.sub(r"\s+", " ", text).strip()
