"""心法健康度：用**真实平仓证据**重算分数，而不是采信模型自报的 confidence.

借鉴 astra-quant-agent 的 lesson_health，但适配 NanoQuant 的 SQLite 模型 (health ∈ [0,1])：

- 从心法正文析出适用人群 cohort（标的 / 方向 / 离场机制），只析出正文真的点名的维度；
- 按该人群的胜率 + 归一化期望值打分，并按样本量向 0.5 回归；
- 开仓现场快照可观测性不足时再折价一半；
- 样本不足返回 ``None`` = **未知**，绝不改分、绝不归档；
- 归档 = ``enabled=False`` + 留痕，**绝不物理删除**。

全部为纯函数（不读全局配置、不碰 DB），便于契约测试。
"""
from __future__ import annotations

import re
import time
from typing import Any

MIN_HEALTH_SAMPLE = 10
ARCHIVE_HEALTH_THRESHOLD = 0.45

_LONG_WORDS = ("做多", "多头", "追多", "低吸", "回踩买", "买入", "看涨")
_SHORT_WORDS = ("做空", "空头", "高空", "抛压", "反弹空", "卖出", "看跌")
_EXIT_WORDS = {
    "硬止损": "stop_loss", "移动止损": "trailing_stop", "阶梯止盈": "roi",
    "部分止盈": "partial_tp", "超时": "timeout", "手动": "manual", "同步": "venue_sync",
}
_SIDE_ALIASES = {"多": "long", "空": "short", "long": "long", "short": "short"}

#: 开仓现场快照的可观测因子（features 里真实存在的才算证据）。
_SNAPSHOT_FIELDS = (
    "rsi", "atr_pct", "vol_ratio", "macd_hist", "ema_gap", "funding",
    "open_interest", "change_24h", "quote_volume", "ret_1", "ret_5",
)


def snapshot_observability(features: dict[str, Any] | None) -> str:
    """逐单标注证据链可观测性：FULL / PARTIAL / PRICE_ONLY / NONE."""
    f = features if isinstance(features, dict) else {}
    observed = sum(1 for k in _SNAPSHOT_FIELDS if f.get(k) is not None)
    if observed >= 6:
        return "FULL"
    if observed >= 3:
        return "PARTIAL"
    if observed >= 1:
        return "PRICE_ONLY"
    return "NONE"


def exit_quality(row: dict[str, Any]) -> dict[str, float]:
    """MFE / MAE / 吐回比 / 兑现率（有 peak_price 才算，否则返回全 0）."""
    entry = float(row.get("entry_price") or 0)
    exit_px = float(row.get("exit_price") or 0)
    peak = float(row.get("peak_price") or 0)
    if entry <= 0 or exit_px <= 0 or peak <= 0:
        return {"mfe_pct": 0.0, "mae_pct": 0.0, "give_back_r": 0.0, "exit_efficiency_pct": 0.0}
    direction = 1 if row.get("side") == "long" else -1
    mfe = max(0.0, (peak - entry) * direction / entry * 100)
    mae = max(0.0, (entry - min(peak, exit_px) if direction == 1 else max(peak, exit_px) - entry) / entry * 100)
    realized = (exit_px - entry) * direction / entry * 100
    efficiency = (realized / mfe * 100) if mfe > 0 else 0.0
    return {
        "mfe_pct": round(mfe, 4),
        "mae_pct": round(mae, 4),
        "give_back_r": round(max(0.0, mfe - realized), 4),
        "exit_efficiency_pct": round(max(0.0, min(100.0, efficiency)), 2),
    }


def _num(value: Any) -> float | None:
    if isinstance(value, bool):
        return None
    try:
        out = float(value)
    except (TypeError, ValueError):
        return None
    return out if out == out and out not in (float("inf"), float("-inf")) else None


def extract_cohort(rule_text: str, *, available_symbols: set[str]) -> dict[str, Any]:
    """从正文析出适用人群；没点名的维度不加过滤（"所有人"）."""
    text = str(rule_text or "")
    symbols = [
        s for s in sorted(available_symbols)
        if re.search(rf"(?<![A-Za-z0-9]){re.escape(s)}(?![A-Za-z0-9])", text, re.I)
    ]
    long_hit = any(w in text for w in _LONG_WORDS)
    short_hit = any(w in text for w in _SHORT_WORDS)
    side = "long" if long_hit and not short_hit else "short" if short_hit and not long_hit else ""
    causes = sorted({label for word, label in _EXIT_WORDS.items() if word in text})
    return {"symbols": symbols, "side": side, "exit_causes": causes}


def select_cohort(rows: list[dict[str, Any]], cohort: dict[str, Any]) -> list[dict[str, Any]]:
    symbols = set(cohort.get("symbols") or [])
    side = cohort.get("side") or ""
    causes = set(cohort.get("exit_causes") or [])
    out = []
    for row in rows or []:
        if not isinstance(row, dict) or _num(row.get("pnl")) is None:
            continue
        if symbols and row.get("symbol") not in symbols:
            continue
        if side and _SIDE_ALIASES.get(str(row.get("side") or "").lower(), "") != side:
            continue
        if causes and row.get("exit_reason") not in causes:
            continue
        out.append(row)
    return out


def score_cohort(rows: list[dict[str, Any]], *, min_sample: int = MIN_HEALTH_SAMPLE) -> dict[str, Any] | None:
    nets = [n for n in (_num(r.get("pnl")) for r in rows) if n is not None]
    n = len(nets)
    if n <= 0:
        return None
    wins = sum(1 for x in nets if x > 0)
    win_rate = wins / n
    avg = sum(nets) / n
    avg_abs = sum(abs(x) for x in nets) / n
    expectancy = (avg / avg_abs) if avg_abs > 0 else 0.0  # ∈ [-1,1], 与资金规模无关
    raw = 0.5 + (win_rate - 0.5) * 0.5 + expectancy * 0.3
    raw = max(0.0, min(1.0, raw))
    confidence = min(1.0, n / (2.0 * max(1, min_sample)))
    health = 0.5 + (raw - 0.5) * confidence
    observable = sum(1 for r in rows if r.get("observability") in ("FULL", "PARTIAL"))
    low_confidence = observable / len(rows) < 0.5
    if low_confidence:
        health = 0.5 + (health - 0.5) * 0.5
    return {
        "health": round(max(0.0, min(1.0, health)), 4),
        "sample_size": n,
        "win_rate_pct": round(win_rate * 100, 2),
        "net_pnl": round(sum(nets), 4),
        "expectancy": round(expectancy, 4),
        "low_confidence": low_confidence,
    }


def score_lesson_against_trades(
    rule_text: str,
    *,
    closed_trades: list[dict[str, Any]],
    min_sample: int = MIN_HEALTH_SAMPLE,
) -> dict[str, Any] | None:
    """给一条心法算健康分；证据不足返回 None（调用方必须保留原分）."""
    rows = [r for r in (closed_trades or []) if isinstance(r, dict)]
    if not rows:
        return None
    symbols = {str(r.get("symbol") or "") for r in rows if r.get("symbol")}
    cohort = extract_cohort(rule_text, available_symbols=symbols)
    subset = select_cohort(rows, cohort)
    if len(subset) < max(1, int(min_sample or 1)):
        return None
    scored = score_cohort(subset, min_sample=min_sample)
    if not scored:
        return None
    parts = []
    if cohort["symbols"]:
        parts.append("/".join(cohort["symbols"]))
    if cohort["side"]:
        parts.append("多头" if cohort["side"] == "long" else "空头")
    if cohort["exit_causes"]:
        parts.append("+".join(cohort["exit_causes"]))
    scored["cohort"] = cohort
    scored["basis"] = "·".join(parts) if parts else "全样本"
    return scored


def decide_archive(
    item: dict[str, Any],
    scored: dict[str, Any] | None,
    *,
    threshold: float = ARCHIVE_HEALTH_THRESHOLD,
    min_sample: int = MIN_HEALTH_SAMPLE,
) -> tuple[bool, str]:
    """低分 + 样本达标才归档；证据不足永不归档。"""
    if not scored:
        return False, "证据不足，不改变状态"
    if int(scored.get("sample_size") or 0) < min_sample:
        return False, "样本不足，不改变状态"
    health = _num(scored.get("health"))
    if health is None:
        return False, "无健康分，不改变状态"
    if health >= threshold:
        return False, f"健康分 {health:.3f} ≥ {threshold}，保持启用"
    return True, f"健康分 {health:.3f} < {threshold}（样本 {scored['sample_size']} 笔，依据 {scored['basis']}）：归档停用留痕"


def refresh_lesson_health(
    lessons: list[dict[str, Any]],
    *,
    closed_trades: list[dict[str, Any]],
    min_sample: int = MIN_HEALTH_SAMPLE,
    threshold: float = ARCHIVE_HEALTH_THRESHOLD,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """纯函数：返回 (更新后的心法 dict 列表, 变更明细)。不写库、不删除条目."""
    updated: list[dict[str, Any]] = []
    changes: list[dict[str, Any]] = []
    for item in lessons or []:
        row = {**item}
        scored = score_lesson_against_trades(
            row.get("rule") or "", closed_trades=closed_trades, min_sample=min_sample
        )
        if scored:
            old_health = _num(row.get("health"))
            row["health"] = scored["health"]
            evidence = dict(row.get("evidence") or {})
            evidence["health_basis"] = {
                "sample_size": scored["sample_size"],
                "win_rate_pct": scored["win_rate_pct"],
                "expectancy": scored["expectancy"],
                "basis": scored["basis"],
                "low_confidence": scored["low_confidence"],
            }
            should_archive, reason = decide_archive(row, scored, threshold=threshold, min_sample=min_sample)
            if should_archive and row.get("enabled"):
                row["enabled"] = False
                evidence["archived_reason"] = reason
                evidence["archived_at"] = int(time.time())
                changes.append({"id": row.get("id"), "action": "archive", "reason": reason})
            else:
                changes.append({
                    "id": row.get("id"), "action": "rescore",
                    "old_health": old_health, "new_health": scored["health"],
                    "sample_size": scored["sample_size"], "basis": scored["basis"],
                })
            row["evidence"] = evidence
            row["health_scored"] = True
            row["sample_size"] = scored["sample_size"]
            row["basis"] = scored["basis"]
        updated.append(row)
    return updated, changes


def trade_rows(trades: list[Any]) -> list[dict[str, Any]]:
    """TradeRecord ORM → 纯 dict（供打分与复盘输入共用）."""
    rows = []
    for t in trades or []:
        features = dict(getattr(t, "features", None) or {})
        rows.append({
            "symbol": getattr(t, "symbol", "") or "",
            "side": getattr(t, "side", "") or "",
            "pnl": float(getattr(t, "pnl", 0) or 0),
            "exit_reason": getattr(t, "exit_reason", "") or "",
            "entry_price": float(getattr(t, "entry_price", 0) or 0),
            "exit_price": float(getattr(t, "exit_price", 0) or 0),
            "peak_price": float(features.get("peak_price") or 0),
            "hold_seconds": int(getattr(t, "hold_seconds", 0) or 0),
            "observability": snapshot_observability(features),
            "features": features,
        })
    return rows
