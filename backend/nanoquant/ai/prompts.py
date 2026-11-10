"""Prompt 模板 (集中管理, 便于版本化与审计)."""
from __future__ import annotations

import json
from typing import Any

DECISION_SYSTEM = """你是「NanoQuant」系统的合约交易决策官, 服务于极小资金 (总权益可能只有 1~50 USDT) 的
币安 U 本位永续合约超短线交易. 你的判断将与量化信号共同决定是否开仓.

铁律:
1. 只输出一个 JSON 对象, 不要 Markdown 代码块, 不要任何解释性前后缀.
2. 资金极小, 手续费与滑点占比极高: 预期波动不足以覆盖双边手续费时必须 hold.
3. 杠杆 1-100x 可选, 但爆仓距离必须显著大于止损距离; 宁可降杠杆也不要贴着强平价.
4. 缺少数据或信号矛盾时, 必须 hold. 禁止猜测.
5. 你只能"否决"或"降杠杆/收窄风险", 不能放宽系统风控参数.
6. 必须给出可被证伪的 reason (引用具体数值), 以及 invalidation (什么情况证明你错了).

输出 JSON 字段:
{
  "action": "open_long" | "open_short" | "hold" | "close",
  "confidence": 0.0-1.0,
  "leverage": 1-100 的整数,
  "sl_pct": 止损百分比(相对开仓价, 0.1-3.0),
  "tp_pct": 止盈百分比(相对开仓价, 0.1-10.0),
  "reason": "60-160 字, 必须引用具体数值",
  "risk_flags": ["爆仓距离不足" 等风险点, 可为空数组],
  "invalidation": "40-100 字, 证伪条件"
}"""

COUNCIL_PERSONA_SUFFIX = """
你现在只代表「{persona}」这一个视角, 关注点: {focus}.
仍然输出同样的 JSON 结构; 如果你认为证据不足, action 直接给 hold 并说明缺什么数据."""

#: 决策真正用得到的特征白名单；原始 OHLC / 重复派生素不进 prompt
FEATURE_KEYS = (
    "rsi", "atr_pct", "vol_ratio", "ema_gap_pct", "range_pct",
    "ret_1", "ret_5", "ret_15", "macd_hist", "funding",
    "open_interest", "oi_change_pct", "change_24h", "quote_volume",
)


def clean_features(features: dict[str, Any] | None) -> dict[str, Any]:
    """去掉 `_bars` 等冗余，只保留白名单数值（v1 / v2 共用）.

    worker 把最近 14 根 K 线放在 features["_bars"]，同时又作为 ctx["klines"] 传入；
    旧实现两边都序列化，等于把同一份 OHLC 发了两遍（约 875 字符 / 330 tokens）。
    """
    src = features or {}
    out: dict[str, Any] = {}
    for key in FEATURE_KEYS:
        value = src.get(key)
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            out[key] = round(float(value), 6)
    return out


def format_snapshot(ctx: dict[str, Any]) -> str:
    """压缩行情与账户上下文, 控制 token 消耗."""
    klines = ctx.get("klines") or []
    bars = [[round(k.get("o", 0), 8), round(k.get("h", 0), 8), round(k.get("l", 0), 8), round(k.get("c", 0), 8)] for k in klines[-14:]]
    news = [
        {"title": str(n.get("title") or "")[:40], "sentiment": n.get("sentiment"), "symbols": n.get("symbols")}
        for n in (ctx.get("news") or [])[:3] if isinstance(n, dict)
    ]
    mantras = [
        {"title": str(m.get("title") or "")[:20], "health": m.get("health")}
        for m in (ctx.get("mantras") or [])[:4] if isinstance(m, dict)
    ]
    payload = {
        "symbol": ctx.get("symbol"),
        "price": ctx.get("price"),
        "quant": {
            "score": ctx.get("score"),
            "direction_hint": ctx.get("side_hint"),
            "reasons": ctx.get("quant_reasons") or [],
        },
        "features": clean_features(ctx.get("features")),
        "recent_bars_ohlc": bars,
        "market": {
            "funding_rate": ctx.get("funding"),
            "change_24h_pct": ctx.get("change_24h"),
            "quote_volume_24h": ctx.get("quote_volume"),
        },
        "account": ctx.get("account") or {},
        "constraints": ctx.get("constraints") or {},
        "news": news,
        "mantras": mantras,
    }
    return json.dumps(payload, ensure_ascii=False, separators=(",", ":"))


def decision_messages(ctx: dict[str, Any], persona: str = "", focus: str = "") -> list[dict[str, str]]:
    system = DECISION_SYSTEM
    if persona:
        system += "\n\n" + COUNCIL_PERSONA_SUFFIX.format(persona=persona, focus=focus or "综合研判")
    user = (
        "以下是当前交易上下文 (JSON). 请基于它给出你的决策 JSON:\n"
        + format_snapshot(ctx)
    )
    return [{"role": "system", "content": system}, {"role": "user", "content": user}]


REVIEW_SYSTEM = """你是交易复盘官. 输入是一笔已平仓交易的全过程数据. 你的任务是提炼一条
可迁移、可执行的「分析心法」, 而不是复述这笔交易.

心法要求:
1. 一句话标题 (<= 24 字) + 一条规则 (<= 200 字), 规则必须包含可判定的触发条件与动作.
2. 不得包含"加大杠杆/放宽止损/满仓/无视风控"等放大风险的建议.
3. 必须与具体交易解耦: 不写"这笔 BTC", 写"当 ATR 骤降且量比<1 时".
4. 如果这笔交易没有可提炼的教训, 返回 {"lessons": []}.
5. 如果新心法与 existing_mantras 中某条语义重复且你的版本更优, 在 lesson 里给出
   "replaces": "旧心法的 title（必须逐字复制）"; 不重复则留空字符串. 不要为了替代而硬凑.

输出 JSON:
{"lessons":[{"title":"","rule":"","category":"entry|exit|risk|timing|position","confidence":0.0-1.0,"evidence":"引用交易中的具体数值","replaces":""}],
 "verdict":"这笔交易的执行与信号是否一致, 一句话",
 "score": 0-100}"""


def review_messages(trade: dict[str, Any], mantras: list[dict[str, Any]] | None = None) -> list[dict[str, str]]:
    payload = {"trade": trade, "existing_mantras": mantras or []}
    return [
        {"role": "system", "content": REVIEW_SYSTEM},
        {"role": "user", "content": "交易数据 (JSON):\n" + json.dumps(payload, ensure_ascii=False, separators=(",", ":"))},
    ]


GLOBAL_REVIEW_SYSTEM = """你是量化系统的自我进化官. 输入是最近一段时间的交易统计与逐笔明细摘要,
以及现有心法库. 请识别系统性不足, 并产出新的心法; 对已经过时/被证伪的心法给出淘汰建议.

输出 JSON:
{"diagnosis":["不足点1","不足点2"],
 "lessons":[{"title":"","rule":"","category":"entry|exit|risk|timing|position","confidence":0.0-1.0,"evidence":"","replaces":""}],
 "retire":[{"title":"","reason":""}],
 "adjust":["建议调整的参数方向 (只描述方向, 不得给出放宽风控的建议)"]}

lessons 里的 "replaces" 填现有心法的 title（逐字复制）表示新版本要替代它；不替代则留空。"""


def global_review_messages(stats: dict[str, Any], trades: list[dict[str, Any]], mantras: list[dict[str, Any]]) -> list[dict[str, str]]:
    payload = {"stats": stats, "recent_trades": trades, "existing_mantras": mantras}
    return [
        {"role": "system", "content": GLOBAL_REVIEW_SYSTEM},
        {"role": "user", "content": "数据 (JSON):\n" + json.dumps(payload, ensure_ascii=False, separators=(",", ":"))},
    ]


MANTRA_DEDUPE_SYSTEM = """你是心法库管理员. 输入是全部生效中的分析心法的 id/title/category/confidence（已省略规则原文，按标题判断主题即可）.
请找出**语义确实重复、可以互相替代**的条目, 每组只保留一条最适合继续生效的 (通常是置信度更高、标题更具体的那条).

约束:
1. 只合并同一个交易主题的不同措辞; 主题不同 (如"手续费"与"止损") 绝不合并.
2. 每组必须给出 keep 与 replaced, replaced 里的 id 必须存在于输入中.
3. 不确定就不要合并, 返回空数组是允许的.
4. 不得改写规则内容, 只做取舍.

输出 JSON:
{"merge":[{"keep":1,"replaced":[2,3],"reason":"为什么这几条是同一主题"}]}"""


def mantra_dedupe_messages(items: list[dict[str, Any]]) -> list[dict[str, str]]:
    return [
        {"role": "system", "content": MANTRA_DEDUPE_SYSTEM},
        {"role": "user", "content": "心法库 (JSON):\n" + json.dumps(items, ensure_ascii=False, separators=(",", ":"))},
    ]


NEWS_SUMMARY_SYSTEM = """你是加密市场资讯分析员. 输入是一批新闻标题与摘要, 请输出:
{"summary":"120 字以内的市场情绪综述","sentiment":-1.0 到 1.0 的情绪分,"hot":["最多3个最需关注的主题"],"risk":"一句话风险提示"}
只输出 JSON."""


def news_summary_messages(items: list[dict[str, Any]]) -> list[dict[str, str]]:
    payload = [{"title": i.get("title"), "summary": (i.get("summary") or "")[:200], "source": i.get("source")} for i in items[:20]]
    return [
        {"role": "system", "content": NEWS_SUMMARY_SYSTEM},
        {"role": "user", "content": json.dumps(payload, ensure_ascii=False, separators=(",", ":"))},
    ]


ANALYZE_SYSTEM = """你是行情分析助手. 用户会给你一个币种的行情快照与新闻摘要.
请输出结构化分析 JSON:
{"bias":"bullish|bearish|neutral","confidence":0.0-1.0,"horizon":"15m|1h|4h",
 "key_levels":{"support":[价格],"resistance":[价格]},
 "drivers":["驱动因素"],"risks":["风险点"],
 "plan":"一段可执行的操作计划 (若不适合交易, 明确说明)"}
只输出 JSON, 价格必须是数字."""


def analyze_messages(symbol: str, snapshot: dict[str, Any], news: list[dict[str, Any]] | None = None) -> list[dict[str, str]]:
    payload = {"symbol": symbol, "snapshot": snapshot, "news": news or []}
    return [
        {"role": "system", "content": ANALYZE_SYSTEM},
        {"role": "user", "content": json.dumps(payload, ensure_ascii=False, separators=(",", ":"))},
    ]
