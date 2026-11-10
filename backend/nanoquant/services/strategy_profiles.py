"""标的粗筛策略: 引擎轮询标的池**之前**先按策略过滤，命中才拉 K 线做深度分析.

设计要点:
- 一条启用的策略都没有 ⇒ 默认全池（保持旧行为，用户不加策略就不会被拦住）；
- 有策略时按 ``priority`` 升序匹配，命中第一条即放行；全部不命中 ⇒ 本轮不分析该标的；
- 粗筛只用**已有批量 ticker**（24h 涨跌/成交额/最新价）与可选的盘口点差，
  不额外消耗 K 线权重；``max_spread_bps>0`` 时才拉盘口（有 2s 缓存）；
- 均为纯函数 + 轻量 CRUD，便于契约测试。
"""
from __future__ import annotations

from typing import Any

from ..db import session_scope
from ..models import StrategyProfile

SIDES = ("any", "long", "short")


class StrategyError(ValueError):
    """策略校验失败（路由层转 400）."""

    def __init__(self, detail: str) -> None:
        super().__init__(detail)
        self.detail = detail


def normalize_filters(raw: Any) -> dict[str, Any]:
    """把任意输入收敛成固定结构，越界值钳回合法区间."""
    src = raw if isinstance(raw, dict) else {}
    symbols = src.get("symbols") or []
    if isinstance(symbols, str):
        symbols = [s for s in symbols.replace("，", ",").split(",")]
    symbols = sorted({str(s).strip().upper() for s in symbols if str(s).strip()})

    side = str(src.get("side") or "any").strip().lower()
    if side not in SIDES:
        side = "any"

    def num(key: str, default: float, lo: float | None = None, hi: float | None = None) -> float:
        try:
            value = float(src.get(key, default))
        except (TypeError, ValueError):
            value = default
        if lo is not None:
            value = max(lo, value)
        if hi is not None:
            value = min(hi, value)
        return value

    chg_min = num("change_24h_min", -100.0, -100.0, 100.0)
    chg_max = num("change_24h_max", 100.0, -100.0, 100.0)
    if chg_min > chg_max:
        chg_min, chg_max = chg_max, chg_min

    return {
        "symbols": symbols,
        "side": side,
        "min_quote_volume": num("min_quote_volume", 0.0, 0.0),
        "min_price": num("min_price", 0.0, 0.0),
        "max_price": num("max_price", 0.0, 0.0),
        "change_24h_min": chg_min,
        "change_24h_max": chg_max,
        "max_spread_bps": num("max_spread_bps", 0.0, 0.0, 500.0),
    }


def summarize(filters: dict[str, Any]) -> str:
    """一句话条件摘要（列表页展示用）."""
    f = normalize_filters(filters)
    parts: list[str] = []
    if f["symbols"]:
        parts.append("/".join(f["symbols"][:4]) + ("…" if len(f["symbols"]) > 4 else ""))
    else:
        parts.append("全池")
    if f["side"] != "any":
        parts.append("只做多" if f["side"] == "long" else "只做空")
    if f["min_quote_volume"] > 0:
        parts.append(f"成交额≥{f['min_quote_volume'] / 1e6:.1f}M")
    if f["min_price"] > 0 or f["max_price"] > 0:
        parts.append(f"价格 {f['min_price'] or 0:g}–{f['max_price'] or '∞'}")
    if f["change_24h_min"] > -100 or f["change_24h_max"] < 100:
        parts.append(f"24h涨跌 {f['change_24h_min']:g}%~{f['change_24h_max']:g}%")
    if f["max_spread_bps"] > 0:
        parts.append(f"点差≤{f['max_spread_bps']:g}bps")
    return " · ".join(parts)


def serialize(row: StrategyProfile) -> dict[str, Any]:
    filters = normalize_filters(row.filters)
    return {
        "id": row.id,
        "name": row.name,
        "description": row.description or "",
        "enabled": bool(row.enabled),
        "priority": int(row.priority or 100),
        "filters": filters,
        "summary": summarize(filters),
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    }


def _order(query):
    return query.order_by(StrategyProfile.priority.asc(), StrategyProfile.id.asc())


def list_profiles() -> list[dict[str, Any]]:
    with session_scope() as s:
        return [serialize(r) for r in _order(s.query(StrategyProfile)).all()]


def active_profiles() -> list[dict[str, Any]]:
    """启用的策略（按优先级）；空列表 = 默认全池."""
    with session_scope() as s:
        rows = (
            _order(s.query(StrategyProfile).filter(StrategyProfile.enabled.is_(True))).all()
        )
        return [serialize(r) for r in rows]


def stats() -> dict[str, Any]:
    with session_scope() as s:
        rows = s.query(StrategyProfile).all()
        enabled = [r for r in rows if r.enabled]
        return {
            "total": len(rows),
            "enabled": len(enabled),
            "default": len(enabled) == 0,
            "by_side": {side: sum(1 for r in enabled if (r.filters or {}).get("side") == side) for side in SIDES},
        }


def create_profile(
    *,
    name: str,
    description: str = "",
    enabled: bool = True,
    priority: int = 100,
    filters: Any = None,
) -> dict[str, Any]:
    name = (name or "").strip()
    if len(name) < 2:
        raise StrategyError("策略名称至少 2 个字")
    payload = normalize_filters(filters)
    with session_scope() as s:
        if s.query(StrategyProfile).filter(StrategyProfile.name == name).first():
            raise StrategyError(f"同名策略已存在: {name}")
        row = StrategyProfile(
            name=name,
            description=(description or "")[:200],
            enabled=bool(enabled),
            priority=int(priority),
            filters=payload,
        )
        s.add(row)
        s.flush()
        return serialize(row)


def update_profile(profile_id: int, patch: dict[str, Any]) -> dict[str, Any] | None:
    with session_scope() as s:
        row = s.get(StrategyProfile, profile_id)
        if row is None:
            return None
        if "name" in patch and patch["name"] is not None:
            name = str(patch["name"]).strip()
            if len(name) < 2:
                raise StrategyError("策略名称至少 2 个字")
            dup = (
                s.query(StrategyProfile)
                .filter(StrategyProfile.name == name, StrategyProfile.id != profile_id)
                .first()
            )
            if dup:
                raise StrategyError(f"同名策略已存在: {name}")
            row.name = name
        if "description" in patch and patch["description"] is not None:
            row.description = str(patch["description"])[:200]
        if "enabled" in patch and patch["enabled"] is not None:
            row.enabled = bool(patch["enabled"])
        if "priority" in patch and patch["priority"] is not None:
            row.priority = int(patch["priority"])
        if "filters" in patch and patch["filters"] is not None:
            row.filters = normalize_filters(patch["filters"])
        s.flush()
        return serialize(row)


def delete_profile(profile_id: int) -> bool:
    with session_scope() as s:
        row = s.get(StrategyProfile, profile_id)
        if row is None:
            return False
        s.delete(row)
        return True


# ------------------------------------------------------------------ 纯匹配逻辑
def _attr(obj: Any, key: str, default: float = 0.0) -> float:
    value = obj.get(key) if isinstance(obj, dict) else getattr(obj, key, default)
    try:
        return float(value or 0)
    except (TypeError, ValueError):
        return default


def match_profile(symbol: str, ticker: Any, book: tuple[float, float] | None, profile: dict[str, Any]) -> bool:
    """单标的是否满足一条策略；``ticker`` 支持 dataclass 或 dict."""
    if not profile.get("enabled", True):
        return False
    f = normalize_filters(profile.get("filters"))
    if f["symbols"] and symbol not in f["symbols"]:
        return False
    last = _attr(ticker, "last")
    if last <= 0:
        return False
    if f["min_price"] > 0 and last < f["min_price"]:
        return False
    if f["max_price"] > 0 and last > f["max_price"]:
        return False
    if f["min_quote_volume"] > 0 and _attr(ticker, "quote_volume") < f["min_quote_volume"]:
        return False
    change = _attr(ticker, "change_pct")
    if change < f["change_24h_min"] or change > f["change_24h_max"]:
        return False
    if f["max_spread_bps"] > 0 and book:
        bid, ask = float(book[0]), float(book[1])
        mid = (bid + ask) / 2
        if mid > 0 and (ask - bid) / mid * 10000 > f["max_spread_bps"]:
            return False
    return True


def prefilter(
    symbols: list[str],
    tickers: dict[str, Any],
    books: dict[str, tuple[float, float]] | None = None,
    profiles: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """按策略过滤标的池.

    返回 ``{"default": bool, "kept": [...], "dropped": [...], "side_bias": {...},
    "matched": {profile_id: n}, "profiles": n}``。
    ``default=True`` 表示没有启用策略，已全量放行。
    """
    active = [p for p in (profiles or []) if p.get("enabled", True)]
    symbols = list(symbols or [])
    if not active:
        return {
            "default": True, "kept": symbols, "dropped": [],
            "side_bias": {}, "matched": {}, "profiles": 0,
        }
    active = sorted(active, key=lambda p: (int(p.get("priority") or 100), int(p.get("id") or 0)))
    kept: list[str] = []
    dropped: list[str] = []
    side_bias: dict[str, str] = {}
    matched: dict[int, int] = {}
    for symbol in symbols:
        ticker = (tickers or {}).get(symbol)
        if ticker is None:
            dropped.append(symbol)
            continue
        hit = next(
            (p for p in active if match_profile(symbol, ticker, (books or {}).get(symbol), p)),
            None,
        )
        if hit is None:
            dropped.append(symbol)
            continue
        kept.append(symbol)
        pid = int(hit.get("id") or 0)
        matched[pid] = matched.get(pid, 0) + 1
        side = normalize_filters(hit.get("filters"))["side"]
        if side in ("long", "short"):
            side_bias[symbol] = side
    return {
        "default": False,
        "kept": kept,
        "dropped": dropped,
        "side_bias": side_bias,
        "matched": matched,
        "profiles": len(active),
    }
