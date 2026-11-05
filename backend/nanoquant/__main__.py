"""python -m nanoquant 启动入口."""
from __future__ import annotations

import uvicorn

from .config import DEBUG, HOST, PORT
from .logging_setup import setup_logging


def main() -> None:
    setup_logging("DEBUG" if DEBUG else "INFO")
    uvicorn.run(
        "nanoquant.main:app",
        host=HOST,
        port=PORT,
        reload=DEBUG,
        log_level="info",
    )


if __name__ == "__main__":
    main()
