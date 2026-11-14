"""闭环循环: 先打标到期决策, 再归档已打标决策.

由 main.lifespan 启动后台任务周期性调用；单轮异常只记日志不中断循环。
"""
from __future__ import annotations

from typing import Any

from ..logging_setup import get_logger
from . import experience, label

log = get_logger("journal.loop")


async def run_cycle() -> dict[str, Any]:
    """执行一轮闭环：label_due → archive_labeled。"""
    result: dict[str, Any] = {"label": None, "archive": None}
    try:
        result["label"] = await label.label_due()
    except Exception as exc:  # noqa: BLE001
        log.warning("决策打标失败: %s", exc)
        result["label"] = {"ok": False, "error": str(exc)[:160]}
    try:
        result["archive"] = experience.archive_labeled()
    except Exception as exc:  # noqa: BLE001
        log.warning("经验归档失败: %s", exc)
        result["archive"] = {"ok": False, "error": str(exc)[:160]}
    return result
