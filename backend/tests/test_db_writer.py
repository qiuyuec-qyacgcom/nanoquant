"""异步写库队列：提交/排空/统计/满队列退化同步/死信落库.

数据一致性命脉：订单/持仓靠这个线程落库，满了宁可慢也不能丢，失败必须进死信。
"""
from __future__ import annotations

import pytest

from nanoquant.services import db_writer as m


def test_submit_runs_task_and_records_stats():
    w = m.DbWriter(maxsize=10)
    calls: list[int] = []
    w.submit(calls.append, 42)
    assert w.drain(2.0) is True
    assert calls == [42]
    st = w.stats()
    assert st["total"] == 1 and st["failed"] == 0 and st["pending"] == 0
    w.stop()


def test_record_tracks_fail_slow_and_max():
    w = m.DbWriter()
    w._record(True, 10.0, "")
    w._record(False, 900.0, "boom")
    w._record(False, 100.0, "later")
    st = w.stats()
    assert st["total"] == 3
    assert st["failed"] == 2
    assert st["slow"] == 1, "只有超过 SLOW_WRITE_MS 那次计入 slow"
    assert st["max_ms"] == 900.0
    assert st["last_error"] == "later"
    assert st["last_ms"] == 100.0


def test_queue_full_falls_back_to_sync(monkeypatch):
    w = m.DbWriter(maxsize=1)
    w._ensure_thread = lambda: None  # 不启动线程，队列不会被消费
    w._queue.put_nowait(("occupied", (), {}))  # 塞满唯一一个槽位
    calls: list[str] = []
    w.submit(calls.append, "sync-path")  # 满 → 退化为同步执行
    assert calls == ["sync-path"]
    assert w.stats()["total"] == 1


def test_failed_task_lands_in_dead_letter():
    def boom() -> None:
        raise RuntimeError("database is locked")

    # 重试特征是 "database is locked"，最终失败后进死信
    w = m.DbWriter(maxsize=10)
    w.submit(boom)
    assert w.drain(5.0) is True
    w.stop()

    from nanoquant.db import session_scope
    from nanoquant.models import WriteFailure

    with session_scope() as s:
        row = s.query(WriteFailure).filter(WriteFailure.kind == "boom").first()
    assert row is not None, "写失败必须落死信，否则本地账本与交易所静默不一致"
    assert "locked" in row.error
    assert row.attempts >= 1


def test_dead_letter_direct_persist():
    def dead_letter_target() -> None:
        raise RuntimeError("disk i/o error")

    m._dead_letter(dead_letter_target, ("arg1",), {"k": "v"}, "disk i/o error", attempts=2)

    from nanoquant.db import session_scope
    from nanoquant.models import WriteFailure

    with session_scope() as s:
        row = s.query(WriteFailure).filter(WriteFailure.kind == "dead_letter_target").first()
    assert row is not None
    assert row.attempts == 2
    assert row.payload == {"args": ["arg1"], "kwargs": {"k": "v"}}


def test_retryable_classification():
    assert m._is_retryable("sqlite3.OperationalError: database is locked")
    assert m._is_retryable("disk i/o error")
    assert not m._is_retryable("ZeroDivisionError: boom")
    assert not m._is_retryable("")
