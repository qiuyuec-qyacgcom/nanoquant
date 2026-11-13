"""引擎运行态 (enabled / mode / 熔断 / 日切锚点) 与冷却锁."""
from __future__ import annotations

import json

from ..db import session_scope
from ..models import Cooldown, EngineState

DEFAULT_STATE: dict[str, str] = {
    "enabled": "false",
    "mode": "demo",
    "halted": "",
    "day_anchor": "",
    "day_start_equity": "",
    #: 人工重置回撤基准的时刻；滚动峰值窗口不会越过它（否则重置形同虚设）
    "peak_anchor_ts": "0",
    "consecutive_losses": "0",
    "last_cycle_ts": "0",
    "last_cycle_ms": "0",
    "last_error": "",
    "ai_calls_hour_bucket": "",
    "ai_calls_hour_count": "0",
    "cycle_timings": "{}",
}


_HALT_PREFIX = "引擎已熔断:"
#: 这些是定时自愈状态，不能持久化为熔断（到期后仍会被判成熔断，永久锁死）
_SELF_HEALING_HALTS = ("止损熔断", "连亏", "冷静期")


def normalize_halted(raw: str) -> str:
    """清洗熔断原因：剥离历史层层拼接的「引擎已熔断: 」前缀，并丢弃自愈类状态.

    修复背景：旧实现把 gate.reason（含「熔断」二字）回写 ``halted``，
    而 ``global_gates`` 又会给已熔断状态再加一次前缀，导致每轮 +6 字符、
    且「止损熔断中」到期后仍被当成熔断，形成永久死锁。读取侧统一归一化即可自愈。
    """
    text = str(raw or "").strip()
    # 历史脏数据可能嵌套上百层前缀，循环剥到干净为止（带安全上限防死循环）
    depth = 0
    while text.startswith(_HALT_PREFIX) and depth < 10000:
        text = text[len(_HALT_PREFIX) :].strip()
        depth += 1
    if not text:
        return ""
    if any(token in text for token in _SELF_HEALING_HALTS):
        return ""
    return text[:200]


def get(key: str, default: str = "") -> str:
    with session_scope() as s:
        row = s.get(EngineState, key)
        if row is None:
            return DEFAULT_STATE.get(key, default)
        return row.value


def set(key: str, value: str) -> None:  # noqa: A001
    with session_scope() as s:
        row = s.get(EngineState, key)
        if row:
            row.value = str(value)
        else:
            s.add(EngineState(key=key, value=str(value)))


def get_many(keys: list[str]) -> dict[str, str]:
    return {k: get(k) for k in keys}


def set_many(mapping: dict[str, str]) -> None:
    with session_scope() as s:
        for k, v in mapping.items():
            row = s.get(EngineState, k)
            if row:
                row.value = str(v)
            else:
                s.add(EngineState(key=k, value=str(v)))


def get_float(key: str, default: float = 0.0) -> float:
    try:
        return float(get(key, str(default)) or default)
    except (TypeError, ValueError):
        return default


def get_int(key: str, default: int = 0) -> int:
    try:
        return int(float(get(key, str(default)) or default))
    except (TypeError, ValueError):
        return default


def as_dict() -> dict[str, str]:
    with session_scope() as s:
        rows = s.query(EngineState).all()
        data = {r.key: r.value for r in rows}
    for k, v in DEFAULT_STATE.items():
        data.setdefault(k, v)
    return data


def dump_payload(payload: dict) -> str:
    return json.dumps(payload, ensure_ascii=False)


# --------------------------------------------------------------- cooldowns
def set_cooldown(symbol: str, until_ts: int, reason: str, kind: str = "symbol") -> None:
    with session_scope() as s:
        row = s.get(Cooldown, symbol)
        if row:
            row.until_ts = int(until_ts)
            row.reason = reason[:120]
            row.kind = kind
        else:
            s.add(Cooldown(symbol=symbol, until_ts=int(until_ts), reason=reason[:120], kind=kind))


def active_cooldowns(now_ts: int) -> dict[str, dict]:
    with session_scope() as s:
        rows = s.query(Cooldown).filter(Cooldown.until_ts > now_ts).all()
        return {
            r.symbol: {"until": r.until_ts, "reason": r.reason, "kind": r.kind, "remaining": r.until_ts - now_ts}
            for r in rows
        }


def clear_cooldown(symbol: str) -> None:
    with session_scope() as s:
        row = s.get(Cooldown, symbol)
        if row:
            s.delete(row)
