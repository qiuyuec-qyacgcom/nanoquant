"""备份: 范围选项 / 创建 / 列表 / 下载 / 还原 / 删除."""
from __future__ import annotations

from fastapi import APIRouter, Depends, HTTPException
from fastapi.responses import FileResponse

from ...services import backup as backup_service
from ...services import events
from ..deps import current_user
from ..schemas import BackupIn, RestoreIn

router = APIRouter(tags=["backup"])


@router.get("/backup/options")
def options(user: str = Depends(current_user)) -> dict:
    return {**backup_service.scope_options(), "usage": backup_service.disk_usage()}


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
