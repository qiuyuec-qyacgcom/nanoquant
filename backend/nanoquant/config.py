"""全局路径与进程级配置.

只放"进程启动即确定"的东西; 可运行的业务参数全部在 DB 的 app_settings 中,
支持后台热更新, 无需重启.
"""
from __future__ import annotations

import os
from pathlib import Path

APP_NAME = "NanoQuant"


def _bool_env(key: str, default: bool) -> bool:
    raw = os.getenv(key)
    if raw is None:
        return default
    return raw.strip().lower() in {"1", "true", "yes", "on"}


class Paths:
    """运行目录布局. NANOQUANT_HOME 可整体迁移数据目录."""

    def __init__(self) -> None:
        default_home = Path(__file__).resolve().parents[2]
        self.HOME = Path(os.getenv("NANOQUANT_HOME", str(default_home))).resolve()
        self.DATA = Path(os.getenv("NANOQUANT_DATA_DIR", str(self.HOME / "data"))).resolve()
        self.LOGS = Path(os.getenv("NANOQUANT_LOG_DIR", str(self.HOME / "logs"))).resolve()
        self.BACKUPS = self.DATA / "backups"
        #: 备份镜像目录：指向**另一块盘**时每次备份会多抄一份.
        #: 主库、日志、备份全在 data/ 下，一块盘写满或损坏就是一起没，镜像才有意义。
        _mirror = (os.getenv("NANOQUANT_BACKUP_MIRROR") or "").strip()
        self.BACKUP_MIRROR: Path | None = Path(_mirror).resolve() if _mirror else None
        self.CACHE = self.DATA / "cache"
        self.DB_PATH = Path(os.getenv("NANOQUANT_DB", str(self.DATA / "nanoquant.db"))).resolve()
        self.FRONTEND_DIST = Path(
            os.getenv("NANOQUANT_FRONTEND", str(self.HOME / "frontend" / "dist"))
        ).resolve()
        self.MASTER_KEY = Path(os.getenv("NANOQUANT_KEY_FILE", str(self.DATA / ".master_key"))).resolve()
        self.LOCK = self.DATA / "engine.lock"
        self.HEARTBEAT = self.DATA / "engine.heartbeat"

    def ensure(self) -> None:
        for d in (self.DATA, self.LOGS, self.BACKUPS, self.CACHE):
            d.mkdir(parents=True, exist_ok=True)


paths = Paths()
paths.ensure()

HOST = os.getenv("NANOQUANT_HOST", "0.0.0.0")
PORT = int(os.getenv("NANOQUANT_PORT", "8899"))
DEBUG = _bool_env("NANOQUANT_DEBUG", False)
SECRET_SALT = os.getenv("NANOQUANT_SALT", "nanoquant-static-salt-v1")
