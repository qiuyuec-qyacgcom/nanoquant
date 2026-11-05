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
    "consecutive_losses": "0",
    "last_cycle_ts": "0",
    "last_cycle_ms": "0",
    "last_error": "",
    "ai_calls_hour_bucket": "",
    "ai_calls_hour_count": "0",
    "cycle_timings": "{}",
}


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
