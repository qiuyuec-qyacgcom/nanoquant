"""备份: 范围选项 / 创建 / 列表 / 下载 / 上传 / 还原 / 删除."""
from __future__ import annotations

from fastapi import APIRouter, Depends, HTTPException, Query, Request
from fastapi.responses import FileResponse

from ...services import backup as backup_service
from ...services import events
from ..deps import current_user
from ..schemas import BackupIn, RestoreIn

router = APIRouter(tags=["backup"])


@router.get("/backup/options")
def options(user: str = Depends(current_user)) -> dict:
    cfg = backup_service.get_backup()
    auto = {
        "enabled": cfg.auto_daily,
        "last_run_ts": cfg.last_run_ts,
        "interval_sec": backup_service.AUTO_INTERVAL_SEC,
        # 从未跑过时给出"下一次检查就会执行"，而不是 0（前端要能区分"没开"和"还没到点"）
        "next_run_ts": (cfg.last_run_ts + backup_service.AUTO_INTERVAL_SEC) if cfg.last_run_ts else 0,
    }
    return {
        **backup_service.scope_options(),
        "usage": backup_service.disk_usage(),
        "auto": auto,
    }


@router.post("/backup/create")
def create(payload: BackupIn, user: str = Depends(current_user)) -> dict:
    result = backup_service.create_backup(
        name=payload.name,
        settings_groups=payload.settings_groups,
        tables=payload.tables,
        files=payload.files,
        encrypt=payload.encrypt,
        created_by=user,
    )
    events.record("info", "backup", f"{user} 创建备份: {result.get('path')}", {"job_id": result.get("job_id")})
    return result


@router.get("/backup/jobs")
def jobs(user: str = Depends(current_user)) -> dict:
    return {"items": backup_service.list_jobs(), "usage": backup_service.disk_usage()}


@router.post("/backup/upload")
async def upload(
    request: Request,
    filename: str = Query(default="", description="原始文件名, 仅用于命名与留痕"),
    user: str = Depends(current_user),
) -> dict:
    """上传外部备份文件并登记为可还原的 job.

    用原始请求体承载文件字节（单文件场景不需要 multipart），文件名走 query。
    上传成功后会出现在备份记录里，可直接对其执行还原。
    """
    data = await request.body()
    result = backup_service.upload_backup(filename=filename, data=data, created_by=user)
    if not result.get("ok"):
        raise HTTPException(status_code=400, detail=result.get("error"))
    events.record("info", "backup", f"{user} 上传备份 {filename or '(未命名)'}", {"job_id": result.get("job_id")})
    return result


@router.post("/backup/{job_id}/restore")
def restore(job_id: int, payload: RestoreIn, user: str = Depends(current_user)) -> dict:
    result = backup_service.restore(job_id, restore_settings=payload.restore_settings, restore_tables=payload.restore_tables)
    if not result.get("ok"):
        raise HTTPException(status_code=400, detail=result.get("error"))
    events.record("warn", "backup", f"{user} 从备份 #{job_id} 还原", result)
    return result


@router.delete("/backup/{job_id}")
def delete(job_id: int, user: str = Depends(current_user)) -> dict:
    if not backup_service.delete_job(job_id):
        raise HTTPException(status_code=404, detail="备份不存在")
    events.record("warn", "backup", f"{user} 删除备份 #{job_id}")
    return {"ok": True}


@router.get("/backup/{job_id}/download")
def download(job_id: int, user: str = Depends(current_user)) -> FileResponse:
    path = backup_service.file_of(job_id)
    if path is None:
        raise HTTPException(status_code=404, detail="备份文件不存在")
    return FileResponse(path, filename=path.name, media_type="application/octet-stream")
