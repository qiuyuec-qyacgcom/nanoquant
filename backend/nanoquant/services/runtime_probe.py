"""运行时抖动探针: 事件循环延迟 + GC 停顿.

为什么需要它:
    端到端 p99 里, 网络 RTT 占大头, 但这只能说明"平均值";
    Rust 与否的真正判据是 **Python 自身的尾部抖动**——
    一次 GC / 一次长任务会让某个协程多等 50–200ms, 这才是 p99.9 的来源。

用法:
    await start()   # 在事件循环里启动
    summary()       # 取 p50/p95/p99/max 与 GC 停顿统计
"""
from __future__ import annotations

import asyncio
import gc
import time
from collections import deque
from typing import Any

_LAG: deque[float] = deque(maxlen=4000)
_GC: deque[float] = deque(maxlen=500)
_TASK: asyncio.Task | None = None
_STARTED = 0.0


_GC_START = 0.0


def _record_gc(phase: str, info: dict[str, Any]) -> None:
    """记录每次 GC 的停顿时长 (stop - start), 这是 OS 无关的真实抖动来源."""
    global _GC_START
    if phase == "start":
        _GC_START = time.perf_counter()
        return
    if _GC_START:
        _GC.append((time.perf_counter() - _GC_START) * 1000)
        _GC_START = 0.0


async def _loop(interval: float = 0.05) -> None:
    """每 50ms 唤醒一次, 实际唤醒时刻与预期之差就是事件循环被占用的时间."""
    while True:
        t0 = time.perf_counter()
        await asyncio.sleep(interval)
        drift = max(0.0, (time.perf_counter() - t0 - interval) * 1000)
        _LAG.append(drift)


def start() -> None:
    global _TASK, _STARTED
    if _TASK is not None and not _TASK.done():
        return
    _STARTED = time.time()
    gc.callbacks.append(_record_gc)
    _TASK = asyncio.create_task(_loop(), name="nq-loop-probe")


def stop() -> None:
    global _TASK
    if _TASK is not None:
        _TASK.cancel()
        _TASK = None
    try:
        gc.callbacks.remove(_record_gc)
    except ValueError:
        pass


def _pct(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    return round(ordered[min(len(ordered) - 1, int(len(ordered) * q))], 2)


def summary() -> dict[str, Any]:
    lags = list(_LAG)
    gcs = list(_GC)
    return {
        "uptime_sec": int(time.time() - _STARTED) if _STARTED else 0,
        "samples": len(lags),
        "loop_lag_ms": {
            "p50": _pct(lags, 0.50),
            "p95": _pct(lags, 0.95),
            "p99": _pct(lags, 0.99),
            "max": round(max(lags), 2) if lags else 0.0,
        },
        "gc": {
            "cycles": len(gcs),
            "pause_ms_p50": _pct(gcs, 0.50),
            "pause_ms_p99": _pct(gcs, 0.99),
            "pause_ms_max": round(max(gcs), 2) if gcs else 0.0,
            "pause_total_ms": round(sum(gcs), 1),
        },
        # 注意: Windows 定时器粒度约 15.6ms, 基线 lag 会偏高, 看 max 与 >50ms 更有意义
        "timer_note": "Windows 默认定时器粒度 ~15.6ms, p50 偏高属正常",
        "blocked_over_20ms": sum(1 for x in lags if x > 20),
        "blocked_over_50ms": sum(1 for x in lags if x > 50),
    }
