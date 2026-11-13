"""备份模块: 任意配置/表/文件 -> 加密 zip, 支持校验与还原.

设计成"选择性备份": 前端可以勾选要备份的配置分组、数据表与附加文件,
备份包内 manifest.json 记录范围与校验信息, 还原前会自动做一次 pre-restore 备份.

另外提供**每日自动备份**: 配置里的 ``backup.auto_daily`` / ``backup.last_run_ts``
曾长期是"只存不读"的死开关——面板上默认开着, 但没有任何代码去执行,
于是 data/backups/ 一直为空。``run_if_due()`` 补上这条链路, 由 lifespan 里的
后台任务每 15 分钟检查一次。
"""
from __future__ import annotations

import asyncio
import hashlib
import io
import json
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
from .settings import GROUPS, get_backup, get_group, invalidate, update_group

log = get_logger("backup")

#: 自动备份的最小间隔（一天一次）
AUTO_INTERVAL_SEC = 24 * 3600
#: 失败后的重试间隔（别等到第二天，也别每分钟刷屏）
AUTO_RETRY_SEC = 3600
#: 后台检查周期
AUTO_CHECK_SEC = 900
_AUTO_TASK: asyncio.Task | None = None

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
    suffix = ".zip.enc" if encrypt else ".zip"
    out_path = paths.BACKUPS / f"nanoquant-{safe_label}-{ts}{suffix}"
    blob = _fernet().encrypt(raw) if encrypt else raw
    out_path.write_bytes(blob)
    # 摘要必须取**落盘字节**：以前算的是加密前的 zip，于是所有加密备份在列表里
    # 都显示 sha256 校验失败（默认就是加密），把真正的文件损坏淹掉了。
    digest = hashlib.sha256(blob).hexdigest()

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


def run_if_due(now: int | None = None) -> dict[str, Any] | None:
    """到点则跑一次自动备份；未开启或未到点返回 None.

    先占位再执行: 备份大表可能要几秒, 并发检查不能各跑一遍。
    失败时把 ``last_run_ts`` 回拨到"1 小时后再试", 而不是留给下一轮循环重试。
    """
    cfg = get_backup()
    if not cfg.auto_daily:
        return None
    ts = int(time.time()) if now is None else int(now)
    last = int(cfg.last_run_ts or 0)
    if ts - last < AUTO_INTERVAL_SEC:
        return None
    label = time.strftime("auto-%Y%m%d", time.localtime(ts))
    update_group("backup", {"last_run_ts": ts})
    try:
        result = create_backup(name=label, created_by="auto")
    except Exception as exc:  # noqa: BLE001
        # 回拨到 1 小时后可重试的位置
        update_group("backup", {"last_run_ts": ts - AUTO_INTERVAL_SEC + AUTO_RETRY_SEC})
        events.record("error", "backup", f"自动备份失败: {str(exc)[:180]}")
        log.warning("自动备份失败: %s", exc)
        return {"ok": False, "error": str(exc)[:200]}
    events.record("info", "backup", f"自动备份 #{result['job_id']} 完成 ({result['size'] / 1024:.1f} KB)")
    log.info("自动备份完成 #%s", result["job_id"])
    return result


async def _auto_loop(interval: int = AUTO_CHECK_SEC) -> None:
    while True:
        try:
            await asyncio.to_thread(run_if_due)
        except Exception as exc:  # noqa: BLE001
            log.warning("自动备份检查异常: %s", exc)
        await asyncio.sleep(interval)


def start_auto() -> None:
    global _AUTO_TASK
    if _AUTO_TASK is not None and not _AUTO_TASK.done():
        return
    _AUTO_TASK = asyncio.create_task(_auto_loop(), name="nq-auto-backup")


def stop_auto() -> None:
    global _AUTO_TASK
    if _AUTO_TASK is not None:
        _AUTO_TASK.cancel()
        _AUTO_TASK = None


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
    # 完整性校验：坏包还原会把半截数据写进库，比不还原更糟。
    # 先按落盘字节比；老版本存的是明文 zip 摘要，解密后再比一次（保持旧包可还原）。
    disk_digest = hashlib.sha256(raw).hexdigest()
    digest_ok = not job.sha256 or disk_digest == job.sha256
    if job.encrypted:
        try:
            raw = _fernet().decrypt(raw)
        except Exception as exc:  # noqa: BLE001
            if not digest_ok:
                return {
                    "ok": False,
                    "error": f"备份文件校验失败（sha256 不符，密文可能被截断或改动）: {path.name}",
                }
            return {"ok": False, "error": f"解密失败: {exc}"}
        if not digest_ok and hashlib.sha256(raw).hexdigest() == job.sha256:
            digest_ok = True  # 旧版本备份：摘要算的是明文 zip
    if not digest_ok:
        return {
            "ok": False,
            "error": f"备份文件校验失败（sha256 不符，可能被截断或改动）: {path.name}",
        }

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
