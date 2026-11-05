"""日志: 控制台 + 按大小轮转的文件."""
from __future__ import annotations

import logging
import logging.handlers
import sys

from .config import APP_NAME, paths

_CONFIGURED = False

FMT = "%(asctime)s %(levelname)-7s [%(name)s] %(message)s"


def setup_logging(level: str = "INFO") -> None:
    global _CONFIGURED
    if _CONFIGURED:
        return
    _CONFIGURED = True
    root = logging.getLogger()
    root.setLevel(getattr(logging, level.upper(), logging.INFO))

    console = logging.StreamHandler(sys.stdout)
    console.setFormatter(logging.Formatter(FMT, "%H:%M:%S"))
    root.addHandler(console)

    file_handler = logging.handlers.RotatingFileHandler(
        paths.LOGS / "nanoquant.log", maxBytes=8 * 1024 * 1024, backupCount=5, encoding="utf-8"
    )
    file_handler.setFormatter(logging.Formatter(FMT))
    root.addHandler(file_handler)

    for noisy in ("httpx", "httpcore", "uvicorn.access"):
        logging.getLogger(noisy).setLevel(logging.WARNING)


def get_logger(name: str) -> logging.Logger:
    return logging.getLogger(f"{APP_NAME}.{name}")
