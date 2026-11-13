"""参数变更审计：谁、什么时候、把哪个字段从什么改成什么.

杠杆与风控参数直接决定风险敞口。出事后如果只能翻 `engine_events`，看到的是
"更新配置组 risk" + 一整份 patch，既没有 diff、也查不到"改前是多少"。
这里落结构化记录，并对密钥/密码类字段一律脱敏——审计日志本身不能变成泄密源。
"""
from __future__ import annotations

import time
from typing import Any

from ..db import session_scope
from ..models import SettingsAudit

#: 命中这些词的字段一律写 "***"（只保留"改过"这一信息）
_SENSITIVE_HINTS = ("secret", "password", "token", "api_key", "_enc")


def _mask(key: str, value: Any) -> Any:
    if any(hint in key.lower() for hint in _SENSITIVE_HINTS):
        return "***"
    return value


def diff(before: dict[str, Any] | None, after: dict[str, Any] | None) -> dict[str, dict[str, Any]]:
    """只记真正变化的字段（含脱敏）。"""
    changes: dict[str, dict[str, Any]] = {}
    for key, new in (after or {}).items():
        old = (before or {}).get(key)
        if old == new:
            continue
        changes[key] = {"from": _mask(key, old), "to": _mask(key, new)}
    return changes


def record(
    user: str,
    group: str,
    before: dict[str, Any] | None,
    after: dict[str, Any] | None,
    *,
    ip: str = "",
) -> dict[str, dict[str, Any]] | None:
    """记一条变更；没有实际变化时返回 None（不产生噪声记录）。"""
    changes = diff(before, after)
    if not changes:
        return None
    with session_scope() as s:
        s.add(
            SettingsAudit(
                ts=int(time.time()),
                user=(user or "")[:64],
                group_name=group[:32],
                changes=changes,
                ip=(ip or "")[:64],
            )
        )
    return changes


def recent(group: str | None = None, limit: int = 50) -> list[dict[str, Any]]:
    """最近的变更记录（新的在前）。"""
    from sqlalchemy import select

    limit = max(1, min(int(limit), 200))
    with session_scope() as s:
        stmt = select(SettingsAudit)
        if group:
            stmt = stmt.where(SettingsAudit.group_name == group)
        rows = s.execute(stmt.order_by(SettingsAudit.ts.desc(), SettingsAudit.id.desc()).limit(limit)).scalars().all()
        return [
            {
                "id": row.id,
                "ts": row.ts,
                "user": row.user,
                "group": row.group_name,
                "changes": row.changes or {},
                "ip": row.ip,
            }
            for row in rows
        ]
