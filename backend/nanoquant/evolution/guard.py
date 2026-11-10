"""心法宪法 Linter: 入库前的**代码级**硬约束.

模型很容易"学会"放宽风控 (加大杠杆、取消止损、下调阈值), 这类结论一旦入库会
持续毒化决策。质量判断不放在 prompt 里当软要求，而是由宿主代码逐句判定：

- 分句判定 + 「禁止式子句」剥离：避免把"严禁放宽止损"这种安全知识误杀;
- 变更动词 × 风险名词共现兜底：覆盖词表未枚举的改写变体;
- 退役/不可观测因子门禁：引用系统根本不采集的因子 ⇒ 心法"不可评估", 会退化成恒真否决律;
- 绝对金额禁令：资金预算必须以 R / 百分比 / 动态风险预算表达;
- 样本量下限：单笔偶发事件不得写入长期心法 (借鉴 astra-quant-agent)。

保留 ``guard_strict=False`` 时退回纯形式校验，供存量数据与特殊场景使用。
"""
from __future__ import annotations

import re
from typing import Any

from ..services.settings import get_evolution

#: 明确的高危短语（含原因），逐句匹配后直接拒收。
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

#: 宪法红线：(正则, 拒绝原因, 是否允许"禁止式"豁免)。
#: 第三列 True 表示"严禁放宽止损"这类**禁止风险扩张**的表述属安全知识。
POISON_PATTERNS: list[tuple[str, str, bool]] = [
    (r"(永远不|绝对不|严禁|彻底禁止).*(做多|开多|买入)", "EXTREME_DIRECTIONAL_BIAS (极端方向偏见)", False),
    (r"(永远不|绝对不|严禁|彻底禁止).*(做空|开空|卖出)", "EXTREME_DIRECTIONAL_BIAS (极端方向偏见)", False),
    (r"(扩大|放宽|取消|不设|移除|关闭|删除).*(止损|SL)", "RISK_EXPANSION (放大止损/抗单)", True),
    (r"(加倍|翻倍|加仓|重仓|梭哈).*(亏损|抗单|摊平)", "MARTINGALE_POISONING (亏损摊平)", True),
    (r"(提高|提升|放大|增加|调高|拉高|加大|上调|放宽|解除).*(杠杆|leverage)", "RISK_EXPANSION (抬升杠杆)", True),
    (r"(杠杆|leverage)[^。；;\n]{0,24}(提高|提升|放大|增加|调高|拉高|加大|上调|放宽|解除)",
     "RISK_EXPANSION (抬升杠杆)", True),
    (r"(提高|提升|放大|增加|调高|拉高|加大|上调|放宽|解除|取消).*(保证金|仓位上限|持仓上限|风险预算|敞口|单笔上限)",
     "RISK_BUDGET_EXPANSION (放宽保证金/仓位/风险预算)", True),
    (r"(降低|下调|放宽|取消|豁免).*(置信度|门槛|阈值|标准|审查|复核|风控参数)",
     "RISK_THRESHOLD_LOWERING (下调风控阈值/审查标准)", True),
    (r"(忽视|不看|废弃|无视|忽略|跳过|绕过|关闭|禁用|停用|移除|删除|突破).*(4H|宏观|ATR|风控|拦截器|熔断|门禁)",
     "GOVERNANCE_OVERRIDE (企图推翻硬风控)", True),
]

#: 变更类动词 × 风险名词的共现兜底（同句即拒），覆盖词表未枚举的改写变体。
_RISK_CHANGE_VERBS = r"(提高|提升|放大|增加|调高|拉高|加大|上调|放宽|解除|取消|关闭|禁用|停用|移除|删除|跳过|绕过|突破|降低|下调|豁免)"
_RISK_NOUNS = r"(杠杆|leverage|保证金|仓位上限|持仓上限|风险预算|敞口|止损|风控|熔断|拦截器|置信度|门槛|阈值|硬约束)"
#: 同句含禁止类词 = 在**禁止**风险扩张（合法），豁免共现兜底。
_RISK_PROHIBITIONS = r"(严禁|禁止|不得|绝不|杜绝|防止|防范|不允许|拒绝)"

#: 系统当前不采集、因而**不可评估**的因子词。引用它们的心法会退化成恒真否决律。
RETIRED_FIELD_TOKENS = (
    "velocity", "power_regime", "continuation_prob_pct", "energy_integral",
    "KINETIC_ACCELERATING", "KINETIC_EXHAUSTION", "动能加速度", "条件延续概率",
    "deviation_area_integral", "calculus_dynamics", "orderbook_imbalance", "cvd_delta",
)

#: 硬编码绝对资金金额（USDT/USD/万U）。资金预算必须用 R / ATR / 百分比表达。
_ABSOLUTE_CAPITAL_RE = re.compile(r"\b\d+(?:\.\d+)?\s*(?:USDT|USD|美元|美金|万\s*U)\b", re.I)

CATEGORY_WHITELIST = {"entry", "exit", "risk", "timing", "position", "market"}


def _sentences(text: str) -> list[str]:
    return [s for s in re.split(r"[。；;\n]", text or "") if s.strip()]


def _strip_prohibition_clauses(sentence: str) -> str:
    """剔除「严禁逆势加仓」这类禁止式子子句，保留其余（"严禁放宽止损，但可加大杠杆" → "但可加大杠杆"）."""
    clauses = [c for c in re.split(r"[，,]", sentence or "") if c.strip()]
    return "，".join(c for c in clauses if not re.search(_RISK_PROHIBITIONS, c))


def effective_text(text: str) -> str:
    """剥离所有禁止式子句后的文本：安全知识（严禁放宽止损）不参与红线匹配."""
    return " ".join(_strip_prohibition_clauses(s) for s in _sentences(text)).strip()


def _retired_field_hits(text: str) -> list[str]:
    hits = []
    for token in RETIRED_FIELD_TOKENS:
        pattern = rf"(?<![A-Za-z0-9_]){re.escape(token)}(?![A-Za-z0-9_])" if re.match(r"^[A-Za-z_]", token) else re.escape(token)
        if re.search(pattern, text or "", re.IGNORECASE if re.match(r"^[A-Za-z_]", token) else 0):
            hits.append(token)
    return hits


def audit(
    title: str,
    rule: str,
    category: str,
    confidence: float,
    *,
    sample_size: int | None = None,
    min_sample: int | None = None,
) -> tuple[bool, str]:
    """返回 (是否放行, 拒绝原因).

    ``sample_size=None`` 表示不检查样本量（管理员手工录入）；传入数值时
    低于 ``min_sample``（默认取配置 ``min_lesson_sample``）一律拒收。
    """
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

    # 样本量门槛：单笔偶发事件/插针噪点不得写入长期心法
    if sample_size is not None:
        need = int(min_sample if min_sample is not None else cfg.min_lesson_sample)
        if sample_size < max(1, need):
            return False, f"样本量不足 ({sample_size} < {need}): 单笔偶发事件拒绝写入长期心法"

    if not cfg.guard_strict:
        return True, ""

    text = f"{title} {rule}"
    check_text = effective_text(text)

    dead = _retired_field_hits(rule)
    if dead:
        return False, f"不可评估因子门禁: 引用了系统不采集的字段 {dead}"

    if _ABSOLUTE_CAPITAL_RE.search(rule):
        return False, "量化原则拦截: 严禁硬编码绝对资金金额, 请用 R / ATR / 百分比表达"

    for pattern, reason in DANGER_PATTERNS:
        if re.search(pattern, check_text, re.I):
            return False, f"命中风险规则: {reason}"

    for sentence in _sentences(rule):
        exempt_text = _strip_prohibition_clauses(sentence)
        for pattern, reason, prohibitive_exempt in POISON_PATTERNS:
            target = exempt_text if prohibitive_exempt else sentence
            if target and re.search(pattern, target, re.IGNORECASE):
                return False, f"触发宪法红线: {reason}"

    for sentence in _sentences(rule):
        remainder = _strip_prohibition_clauses(sentence)
        if remainder and re.search(_RISK_CHANGE_VERBS, remainder) and re.search(_RISK_NOUNS, remainder, re.I):
            return False, "触发宪法红线: RISK_PARAMETER_TAMPERING (疑似修改杠杆/保证金/风控阈值)"

    if not re.search(r"当|若|如果|一旦|在.*时|when|if", text, re.I):
        return False, "缺少触发条件 (当/若/一旦...)"
    return True, ""


def sanitize(text: str) -> str:
    """清除控制字符与多余空白, 防止污染 prompt 结构."""
    text = re.sub(r"[\x00-\x08\x0b\x0c\x0e-\x1f]", "", text or "")
    return re.sub(r"\s+", " ", text).strip()


def audit_payload(item: dict[str, Any], **kwargs: Any) -> tuple[bool, str]:
    """从 AI 提案 dict 直接审计，供复盘与全局复盘共用."""
    return audit(
        str(item.get("title") or ""),
        str(item.get("rule") or ""),
        str(item.get("category") or "entry"),
        float(item.get("confidence") or 0.55),
        **kwargs,
    )
