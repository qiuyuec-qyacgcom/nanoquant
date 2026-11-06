"""心法效果评估：采用 vs 未采用的观察性对照.

注意这不是随机 A/B：两组交易的行情、时段、标的不同，只能当"有没有明显异常"的
早期信号，不能当作因果结论。真正要做因果验证需要按笔随机开关注入并记录分组。

分组依据来自开仓时写入 `features` 的决策快照:
- `ai_used`: 这一笔是否真的走了 AI 决策（没走 AI 的交易不能拿来比心法）
- `injected_mantra_ids`: 决策时注入了哪些心法（旧数据没有该字段 → legacy 组）
"""
from __future__ import annotations

from typing import Any

from ..db import session_scope
from ..models import TradeRecord

MIN_SAMPLE = 10
LEGACY_NOTE = (
    "这是观察性对照，不是随机 A/B：两组行情/时段/标的不同，差异不等于心法的因果效果。"
    "旧成交缺少决策快照，单列为 legacy，不参与对照。"
)


def _ids(row: dict[str, Any]) -> list[int]:
    features = row.get("features") or {}
    if not isinstance(features, dict):
        return []
    raw = features.get("injected_mantra_ids") or []
    return [int(x) for x in raw if str(x).isdigit()]


def _summarize(rows: list[dict[str, Any]]) -> dict[str, Any]:
    n = len(rows)
    if not n:
        return {
            "trades": 0, "wins": 0, "losses": 0, "win_rate": 0.0,
            "pnl": 0.0, "avg_pnl": 0.0, "profit_factor": 0.0,
        }
    pnls = [float(r.get("pnl") or 0.0) for r in rows]
    wins = sum(1 for p in pnls if p > 0)
    gross_win = sum(p for p in pnls if p > 0)
    gross_loss = -sum(p for p in pnls if p < 0)
    profit_factor = round(gross_win / gross_loss, 3) if gross_loss > 0 else (round(gross_win, 3) if gross_win > 0 else 0.0)
    return {
        "trades": n,
        "wins": wins,
        "losses": sum(1 for p in pnls if p < 0),
        "win_rate": round(wins / n * 100, 2),
        "pnl": round(sum(pnls), 6),
        "avg_pnl": round(sum(pnls) / n, 6),
        "profit_factor": profit_factor,
    }


def compare_groups(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """按「AI 决策 + 是否注入心法」分组统计；输入是纯 dict，方便离线测试。"""
    injected: list[dict[str, Any]] = []
    control: list[dict[str, Any]] = []
    ai_off: list[dict[str, Any]] = []
    legacy: list[dict[str, Any]] = []
    per_mantra: dict[int, list[dict[str, Any]]] = {}

    for row in rows:
        features = row.get("features") if isinstance(row.get("features"), dict) else {}
        ai_used = features.get("ai_used")
        ids = _ids(row)
        if ai_used is None:
            legacy.append(row)
            continue
        if ai_used is False:
            ai_off.append(row)
            continue
        if ids:
            injected.append(row)
            for mid in ids:
                per_mantra.setdefault(mid, []).append(row)
        else:
            control.append(row)

    a = _summarize(injected)
    b = _summarize(control)
    sample_ok = a["trades"] >= MIN_SAMPLE and b["trades"] >= MIN_SAMPLE
    by_mantra = []
    for mid, group in sorted(per_mantra.items(), key=lambda kv: -len(kv[1]))[:20]:
        item = {"id": mid, **_summarize(group)}
        by_mantra.append(item)

    return {
        "injected": a,
        "control": b,
        "ai_off": _summarize(ai_off),
        "legacy": _summarize(legacy),
        "delta_win_rate_pct": round(a["win_rate"] - b["win_rate"], 2),
        "delta_avg_pnl": round(a["avg_pnl"] - b["avg_pnl"], 6),
        "min_sample": MIN_SAMPLE,
        "sample_ok": sample_ok,
        "verdict": (
            "样本不足，暂不下结论" if not sample_ok
            else "采用组胜率更高" if a["win_rate"] > b["win_rate"] + 5
            else "采用组胜率更低" if a["win_rate"] < b["win_rate"] - 5
            else "两组胜率接近，未见明显优势"
        ),
        "per_mantra": by_mantra,
        "note": LEGACY_NOTE,
    }


def evaluate(limit: int = 500, mode: str = "") -> dict[str, Any]:
    limit = max(1, min(int(limit), 2000))
    with session_scope() as s:
        query = s.query(TradeRecord)
        if mode in {"demo", "live"}:
            query = query.filter(TradeRecord.mode == mode)
        rows = query.order_by(TradeRecord.id.desc()).limit(limit).all()
        data = [
            {
                "pnl": float(r.pnl or 0.0),
                "features": dict(r.features or {}),
                "mode": r.mode,
            }
            for r in rows
        ]
    result = compare_groups(data)
    result["window"] = {"limit": limit, "mode": mode or "all", "rows": len(data)}
    return result
