"""历史数据保留策略: 先归档, 再分批删除.

背景:
- `signals` 只增不删, 长期会拖慢信号审计查询;
- `engine_events` 同理, 而配置里的 `event_retention_days` 之前根本没被使用.

策略:
- 按配置保留 N 天; 删除前把整批数据写成 gzip JSONL 落到 `data/archive/`,
  保证"删了还能查";
- 分批删除（每批 400 行）: 一次性 DELETE 上千行会长时间持有写锁,
  在 WAL 下可能把同进程的读请求拖到 busy_timeout。
"""
from __future__ import annotations

import gzip
import json
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

from sqlalchemy import delete, select

from ..config import paths
from ..db import session_scope
from ..logging_setup import get_logger
from ..models import AlertLog, Cooldown, EngineEvent, EquityPoint, Signal
from .settings import get_system

log = get_logger("retention")

ARCHIVE_DIR = paths.DATA / "archive"
BATCH = 400
#: 权益曲线降采样：48 小时内保留每分钟一点（看细节），更早的每 10 分钟一点（看趋势）。
#: 采样是一分钟一条，30 天就是 4.3 万行；降采样后约 3 千行，曲线外观几乎无差别。
EQUITY_RECENT_HOURS = 48
EQUITY_BUCKET_SEC = 600


def _archive(name: str, rows: list[dict[str, Any]]) -> Path | None:
    if not rows:
        return None
    ARCHIVE_DIR.mkdir(parents=True, exist_ok=True)
    path = ARCHIVE_DIR / f"{name}-{time.strftime('%Y%m%d')}.jsonl.gz"
    with gzip.open(path, "at", encoding="utf-8") as fh:
        for row in rows:
            fh.write(json.dumps(row, ensure_ascii=False, default=str) + "\n")
    return path


def _purge(
    model: Any,
    ts_field: Any,
    cutoff: int,
    serializer: Callable[[Any], dict[str, Any]],
    name: str,
) -> int:
    removed = 0
    while True:
        with session_scope() as s:
            rows = s.execute(select(model).where(ts_field < cutoff).limit(BATCH)).scalars().all()
            if not rows:
                break
            _archive(name, [serializer(row) for row in rows])
            ids = [row.id for row in rows]
            s.execute(delete(model).where(model.id.in_(ids)))
            removed += len(ids)
        time.sleep(0.05)  # 让出写锁, 避免把读请求拖到 busy_timeout
    return removed


def _downsample_equity(now: int, *, recent_hours: int = EQUITY_RECENT_HOURS,
                       bucket_sec: int = EQUITY_BUCKET_SEC) -> int:
    """把近期之外的权益采样压成每 bucket 一个点，返回删除行数.

    保留每个桶里**最早**的那条：曲线的拐点通常出现在桶首，取均值会抹平尖峰。
    """
    boundary = now - recent_hours * 3600
    with session_scope() as s:
        rows = s.execute(
            select(EquityPoint.id, EquityPoint.mode, EquityPoint.ts)
            .where(EquityPoint.ts < boundary)
            .order_by(EquityPoint.ts)
        ).all()
    kept: set[tuple[str, int]] = set()
    drop: list[int] = []
    for row_id, mode, ts in rows:
        key = (str(mode), int(ts) // bucket_sec)
        if key in kept:
            drop.append(int(row_id))
        else:
            kept.add(key)
    removed = 0
    for i in range(0, len(drop), BATCH):
        chunk = drop[i : i + BATCH]
        with session_scope() as s:
            removed += s.execute(delete(EquityPoint).where(EquityPoint.id.in_(chunk))).rowcount or 0
        time.sleep(0.02)
    return removed


def purge_expired(now: int | None = None) -> dict[str, int]:
    """按配置清理过期数据, 返回每类删除条数."""
    cfg = get_system()
    now = now or int(time.time())
    stats = {"signals": 0, "events": 0, "alerts": 0, "equity": 0, "equity_downsampled": 0, "cooldowns": 0}

    stats["signals"] = _purge(
        Signal,
        Signal.ts,
        now - cfg.signal_retention_days * 86400,
        lambda row: {
            "id": row.id,
            "ts": row.ts,
            "symbol": row.symbol,
            "direction": row.direction,
            "score": row.score,
            "decision": row.decision,
            "reject_reason": row.reject_reason,
            "ai_action": row.ai_action,
            "ai_confidence": row.ai_confidence,
            "mode": row.mode,
            "features": row.features,
        },
        "signals",
    )
    stats["events"] = _purge(
        EngineEvent,
        EngineEvent.ts,
        now - cfg.event_retention_days * 86400,
        lambda row: {
            "ts": row.ts,
            "level": row.level,
            "category": row.category,
            "message": row.message,
            "payload": row.payload,
        },
        "events",
    )

    stats["alerts"] = _purge(
        AlertLog,
        AlertLog.ts,
        now - cfg.event_retention_days * 86400,
        lambda row: {
            "ts": row.ts,
            "level": row.level,
            "category": row.category,
            "message": row.message,
            "channels": row.channels,
            "ok": row.ok,
            "error": row.error,
        },
        "alerts",
    )

    stats["equity_downsampled"] = _downsample_equity(now)
    with session_scope() as s:
        stats["equity"] = (
            s.execute(delete(EquityPoint).where(EquityPoint.ts < now - cfg.equity_retention_days * 86400)).rowcount or 0
        )
        stats["cooldowns"] = (
            s.execute(delete(Cooldown).where(Cooldown.until_ts < now - 86400)).rowcount or 0
        )
    return stats
