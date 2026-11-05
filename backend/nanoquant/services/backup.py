"""备份模块: 任意配置/表/文件 -> 加密 zip, 支持校验与还原.

设计成"选择性备份": 前端可以勾选要备份的配置分组、数据表与附加文件,
备份包内 manifest.json 记录范围与校验信息, 还原前会自动做一次 pre-restore 备份.
"""
from __future__ import annotations

import hashlib
import io
import json
import os
import shutil
import time
import zipfile
from pathlib import Path
from typing import Any

from sqlalchemy import delete, insert, select

from ..config import paths
from ..db import migrate_legacy_demo_naming, session_scope
from ..logging_setup import get_logger
from ..models import BackupJob, Setting
from ..security import _fernet
from . import events
from .settings import GROUPS, get_backup, get_group, invalidate

log = get_logger("backup")

# 可备份的表 -> 模型
def _table_map() -> dict[str, Any]:
    from .. import models

    return {
        "app_settings": models.Setting,
        "ai_providers": models.AiProvider,
        "trading_symbols": models.TradingSymbol,
        "signals": models.Signal,
        "orders": models.OrderRecord,
        "positions": models.PositionRecord,
        "trades": models.TradeRecord,
        "mantras": models.Mantra,
        "evolution_runs": models.EvolutionRun,
        "news_items": models.NewsItem,
        "equity_points": models.EquityPoint,
        "cooldowns": models.Cooldown,
        "engine_events": models.EngineEvent,
        "backup_jobs": models.BackupJob,
    }


DEFAULT_TABLES = ["app_settings", "ai_providers", "trading_symbols", "mantras", "trades", "positions"]


def scope_options() -> dict[str, Any]:
    return {
        "settings_groups": list(GROUPS.keys()),
        "tables": list(_table_map().keys()),
        "files": _list_files(),
        "default_tables": DEFAULT_TABLES,
    }


def _list_files() -> list[str]:
    out: list[str] = []
    search_dirs = [paths.DATA]
    if get_backup().include_logs:
        search_dirs.append(paths.LOGS)
    for base in search_dirs:
        if not base.exists():
            continue
        for p in base.rglob("*"):
            if not p.is_file():
                continue
            if "backups" in p.parts:
                continue
            if p.suffix in {".db", ".db-wal", ".db-shm", ".lock", ".heartbeat"}:
                continue
            # 主密钥用于解密 API Key, 不进备份包 (避免"备份泄露 = 密钥泄露")
            if p.name.startswith(".master_key"):
                continue
            if p.stat().st_size > 20 * 1024 * 1024:
                continue
            out.append(str(p.relative_to(paths.HOME)).replace("\\", "/"))
    return sorted(out)[:200]


def _row_to_dict(row: Any, model: Any) -> dict[str, Any]:
    data: dict[str, Any] = {}
    for col in model.__table__.columns:
        value = getattr(row, col.name)
        if isinstance(value, (bytes, bytearray)):
            value = value.decode("utf-8", "ignore")
        data[col.name] = value
    return data


def create_backup(
    *,
    name: str = "",
    settings_groups: list[str] | None = None,
    tables: list[str] | None = None,
    files: list[str] | None = None,
    encrypt: bool | None = None,
    created_by: str = "manual",
) -> dict[str, Any]:
    cfg = get_backup()
    encrypt = cfg.encrypt if encrypt is None else encrypt
    settings_groups = settings_groups if settings_groups is not None else list(GROUPS.keys())
    tables = tables if tables is not None else DEFAULT_TABLES
    files = files or []
    ts = int(time.time())
    label = name.strip() or time.strftime("%Y%m%d-%H%M%S", time.localtime(ts))
    safe_label = "".join(c for c in label if c.isalnum() or c in "-_")[:60] or "backup"

    manifest: dict[str, Any] = {
        "version": 1,
        "created_at": ts,
        "created_by": created_by,
        "settings_groups": settings_groups,
        "tables": tables,
        "files": files,
        "encrypted": encrypt,
    }
    buffer = io.BytesIO()
    counts: dict[str, int] = {}

    with zipfile.ZipFile(buffer, "w", zipfile.ZIP_DEFLATED) as zf:
        for group in settings_groups:
            if group not in GROUPS:
                continue
            zf.writestr(f"settings/{group}.json", json.dumps(get_group(group, use_cache=False), ensure_ascii=False, indent=2))
        tmap = _table_map()
        for table in tables:
            model = tmap.get(table)
            if model is None:
                continue
            with session_scope() as s:
                rows = s.execute(select(model)).scalars().all()
                data = [_row_to_dict(r, model) for r in rows]
            counts[table] = len(data)
            zf.writestr(f"tables/{table}.json", json.dumps(data, ensure_ascii=False, default=str))
        for rel in files:
            target = (paths.HOME / rel).resolve()
            if not str(target).startswith(str(paths.HOME.resolve())) or not target.is_file():
                continue
            zf.write(target, f"files/{rel}")
        manifest["row_counts"] = counts
        zf.writestr("manifest.json", json.dumps(manifest, ensure_ascii=False, indent=2))

    raw = buffer.getvalue()
    digest = hashlib.sha256(raw).hexdigest()
    suffix = ".zip.enc" if encrypt else ".zip"
    out_path = paths.BACKUPS / f"nanoquant-{safe_label}-{ts}{suffix}"
    if encrypt:
        out_path.write_bytes(_fernet().encrypt(raw))
    else:
        out_path.write_bytes(raw)

    with session_scope() as s:
        job = BackupJob(
            ts=ts, name=safe_label, scope=manifest, file_path=str(out_path),
            size=out_path.stat().st_size, sha256=digest, encrypted=encrypt,
            status="ok", message=f"配置组 {len(settings_groups)} 个, 表 {len(tables)} 张",
            created_by=created_by,
        )
        s.add(job)
        s.flush()
        job_id = job.id

    _apply_retention()
    events.record("info", "backup", f"备份完成 #{job_id} ({out_path.stat().st_size / 1024:.1f} KB)")
    return {"ok": True, "job_id": job_id, "path": str(out_path), "size": out_path.stat().st_size, "sha256": digest, "encrypted": encrypt, "manifest": manifest}


def _apply_retention() -> None:
    cfg = get_backup()
    with session_scope() as s:
        jobs = s.query(BackupJob).order_by(BackupJob.ts.desc()).all()
        for job in jobs[cfg.retention :]:
            try:
                Path(job.file_path).unlink(missing_ok=True)
            except OSError:
                pass
            s.delete(job)


def list_jobs(limit: int = 50) -> list[dict[str, Any]]:
    with session_scope() as s:
        rows = s.query(BackupJob).order_by(BackupJob.ts.desc()).limit(limit).all()
        out = []
        for r in rows:
            exists = Path(r.file_path).exists()
            out.append(
                {
                    "id": r.id,
                    "ts": r.ts,
                    "name": r.name,
                    "scope": r.scope,
                    "size": r.size,
                    "sha256": r.sha256,
                    "encrypted": r.encrypted,
                    "status": r.status,
                    "message": r.message,
                    "created_by": r.created_by,
                    "exists": exists,
                    "file_path": r.file_path,
                    "sha256_ok": (_sha256_file(Path(r.file_path)) == r.sha256) if exists else None,
                }
            )
        return out


def _sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def restore(job_id: int, *, restore_settings: bool = True, restore_tables: bool = True) -> dict[str, Any]:
    with session_scope() as s:
        job = s.get(BackupJob, job_id)
        if job is None:
            return {"ok": False, "error": "备份不存在"}
        path = Path(job.file_path)
        scope = job.scope or {}
    if not path.exists():
        return {"ok": False, "error": "备份文件已丢失"}
    raw = path.read_bytes()
    if job.encrypted:
        try:
            raw = _fernet().decrypt(raw)
        except Exception as exc:  # noqa: BLE001
            return {"ok": False, "error": f"解密失败: {exc}"}

    # 还原前自保
    create_backup(name=f"pre-restore-{job_id}", created_by="system")

    zf = zipfile.ZipFile(io.BytesIO(raw))
    restored: dict[str, int] = {}
    if restore_settings:
        for group in scope.get("settings_groups", []):
            member = f"settings/{group}.json"
            if member not in zf.namelist():
                continue
            data = json.loads(zf.read(member))
            with session_scope() as s:
                row = s.get(Setting, group)
                if row:
                    row.value = json.dumps(data, ensure_ascii=False)
                else:
                    s.add(Setting(key=group, value=json.dumps(data, ensure_ascii=False)))
            restored[f"settings:{group}"] = 1
        invalidate()
    if restore_tables:
        tmap = _table_map()
        for table in scope.get("tables", []):
            member = f"tables/{table}.json"
            model = tmap.get(table)
            if model is None or member not in zf.namelist():
                continue
            rows = json.loads(zf.read(member))
            with session_scope() as s:
                s.execute(delete(model))
                if rows:
                    s.execute(insert(model), rows)
            restored[table] = len(rows)
    if restore_tables or restore_settings:
        # 旧备份里可能仍是 paper 命名, 还原后就地迁移到 demo(模拟盘)
        migrate_legacy_demo_naming()
    events.record("warn", "backup", f"已从备份 #{job_id} 还原: {restored}")
    return {"ok": True, "restored": restored, "note": "已自动创建还原前备份"}


def delete_job(job_id: int) -> bool:
    with session_scope() as s:
        job = s.get(BackupJob, job_id)
        if job is None:
            return False
        try:
            Path(job.file_path).unlink(missing_ok=True)
        except OSError:
            pass
        s.delete(job)
        return True


def file_of(job_id: int) -> Path | None:
    with session_scope() as s:
        job = s.get(BackupJob, job_id)
        if job is None:
            return None
        p = Path(job.file_path)
        return p if p.exists() else None


def disk_usage() -> dict[str, Any]:
    total = sum(p.stat().st_size for p in paths.BACKUPS.glob("*") if p.is_file())
    free = 0
    try:
        free = shutil.disk_usage(paths.BACKUPS).free
    except OSError:
        pass
    return {"files": len(list(paths.BACKUPS.glob("*"))), "bytes": total, "free_bytes": free}
