"""把下单/平仓的 SQLite 提交移出关键路径.

SQLite 的 commit 偶尔会撞上 WAL checkpoint 或 fsync，出现几十毫秒的尖刺
（本地压测 persist p99 0.085ms，但 max 74ms）。下单路径只负责把写任务塞进
有界队列（微秒级），由单独的写线程按顺序提交。

安全边界：
- 队列有界；满了就退化为同步写——宁可慢，不丢单；
- 写失败打印堆栈并写事件日志，不吞异常；
- 进程退出前 drain，保证已 ack 的订单落库；
- 崩溃窗口由启动时的交易所仓位对账兜底（worker._reconcile_live）。
"""
from __future__ import annotations

import queue
import threading
import time
from collections.abc import Callable
from typing import Any

from ..logging_setup import get_logger

log = get_logger("db_writer")

#: 积压 / 慢写阈值: 超过就写一条 warn 事件 (走告警通道)
BACKLOG_WARN = 200
SLOW_WRITE_MS = 800.0

#: 可重试的错误特征 (SQLite 锁竞争/瞬时 IO), 其它一律进死信
RETRYABLE_HINTS = ("database is locked", "table is locked", "database is busy", "disk i/o error")
MAX_ATTEMPTS = 3


def _is_retryable(error: str) -> bool:
    text = (error or "").lower()
    return any(hint in text for hint in RETRYABLE_HINTS)


def _dead_letter(fn: Callable[..., Any], args: tuple, kwargs: dict, error: str, attempts: int = 1) -> None:
    """把最终失败的写任务落到 write_failures, 供面板查看与重放."""
    try:
        from ..db import session_scope
        from ..models import WriteFailure

        with session_scope() as s:
            s.add(
                WriteFailure(
                    kind=getattr(fn, "__name__", "unknown"),
                    payload={"args": list(args), "kwargs": kwargs},
                    error=(error or "")[:500],
                    attempts=max(1, attempts),
                )
            )
    except Exception:  # noqa: BLE001
        log.exception("死信落库失败: %s", error)
    try:
        from . import events

        events.record("error", "db", f"写库最终失败, 已进死信队列: {(error or '')[:120]}")
    except Exception:  # noqa: BLE001
        pass


def _safe_call(fn: Callable[..., Any], args: tuple, kwargs: dict) -> tuple[bool, str]:
    """执行一次写任务; 失败只记日志, 由调用方决定重试还是进死信."""
    try:
        fn(*args, **kwargs)
        return True, ""
    except Exception as exc:  # noqa: BLE001
        log.warning("异步写库失败: %s", exc)
        return False, str(exc)[:200]


class DbWriter:
    def __init__(self, maxsize: int = 20000) -> None:
        self._queue: queue.Queue[tuple[Callable[..., Any], tuple, dict]] = queue.Queue(maxsize=maxsize)
        self._thread: threading.Thread | None = None
        self._lock = threading.Lock()
        self._stopped = threading.Event()
        self._stats_lock = threading.Lock()
        self._total = 0
        self._failed = 0
        self._slow = 0
        self._max_ms = 0.0
        self._last_ms = 0.0
        self._last_error = ""
        self._last_warn = 0.0

    # ------------------------------------------------------------------ public
    def submit(self, fn: Callable[..., Any], *args: Any, **kwargs: Any) -> None:
        """提交一个写任务；队列满时退化为同步执行。"""
        self._ensure_thread()
        try:
            self._queue.put_nowait((fn, args, kwargs))
        except queue.Full:
            log.error("写库队列已满 (%d), 退化为同步写入", self._queue.maxsize)
            started = time.perf_counter()
            ok, error = _safe_call(fn, args, kwargs)
            self._record(ok, (time.perf_counter() - started) * 1000, error)
            if not ok:
                _dead_letter(fn, args, kwargs, error, 1)

    @property
    def pending(self) -> int:
        return self._queue.qsize()

    def stats(self) -> dict[str, Any]:
        """写队列快照 (给诊断页/告警用)."""
        with self._stats_lock:
            return {
                "pending": self._queue.qsize(),
                "unfinished": self._queue.unfinished_tasks,
                "total": self._total,
                "failed": self._failed,
                # 慢写（> SLOW_WRITE_MS）次数与历史最大值: 用来判断死信是否由磁盘抖动引起
                "slow": self._slow,
                "max_ms": round(self._max_ms, 2),
                "last_ms": round(self._last_ms, 2),
                "last_error": self._last_error,
            }

    def drain(self, timeout: float = 8.0) -> bool:
        """停机前排空队列，返回是否完全排空。

        注意不能用 `empty()` 判断: 队列项一旦被 `get()` 取走就为空,
        但此时写线程可能还在提交事务。必须等 `unfinished_tasks`
        (由 `task_done()` 递减) 归零, 才是真的落库完成。
        """
        if self._thread is None:
            return True
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self._queue.unfinished_tasks == 0:
                return True
            time.sleep(0.02)
        log.warning("写库队列未在 %.1fs 内排空，剩余 %d 条", timeout, self._queue.unfinished_tasks)
        return self._queue.unfinished_tasks == 0

    def stop(self) -> None:
        self.drain()
        self._stopped.set()

    # --------------------------------------------------------------- internals
    def _ensure_thread(self) -> None:
        if self._thread is not None and self._thread.is_alive():
            return
        with self._lock:
            if self._thread is not None and self._thread.is_alive():
                return
            self._thread = threading.Thread(target=self._run, name="nq-db-writer", daemon=True)
            self._thread.start()

    def _record(self, ok: bool, elapsed_ms: float, error: str) -> None:
        with self._stats_lock:
            self._total += 1
            if not ok:
                self._failed += 1
                self._last_error = error
            if elapsed_ms > SLOW_WRITE_MS:
                self._slow += 1
            self._max_ms = max(self._max_ms, elapsed_ms)
            self._last_ms = elapsed_ms

    def _maybe_warn(self, elapsed_ms: float) -> None:
        """积压或慢写时写一条 warn 事件 (30 秒节流), 走告警通道."""
        now = time.time()
        if now - self._last_warn < 30:
            return
        backlog = self._queue.qsize()
        if backlog <= BACKLOG_WARN and elapsed_ms <= SLOW_WRITE_MS:
            return
        self._last_warn = now
        try:
            from . import events

            events.record(
                "warn",
                "db",
                f"写库队列积压 {backlog} 条, 单次写入 {elapsed_ms:.0f}ms, 请检查磁盘/慢查询",
            )
        except Exception:  # noqa: BLE001
            pass

    def _run(self) -> None:
        while not self._stopped.is_set() or not self._queue.empty():
            try:
                fn, args, kwargs = self._queue.get(timeout=0.2)
            except queue.Empty:
                continue
            started = time.perf_counter()
            ok = False
            error = ""
            try:
                # 锁竞争类错误做短退避重试; 其它错误直接进死信, 不浪费重试
                for attempt in range(1, MAX_ATTEMPTS + 1):
                    ok, error = _safe_call(fn, args, kwargs)
                    if ok or not _is_retryable(error):
                        break
                    time.sleep(0.05 * attempt)
            finally:
                self._queue.task_done()
            elapsed = (time.perf_counter() - started) * 1000
            self._record(ok, elapsed, error)
            if not ok:
                _dead_letter(fn, args, kwargs, error, attempt)
            self._maybe_warn(elapsed)


db_writer = DbWriter()
