"""死信队列的行为：概览、幂等重放、自动重试上限、人工忽略.

这块代码只在"写库已经失败"时才跑，平时根本没有流量走到，
所以必须靠测试保证它不是一堆想当然的分支。
"""
from __future__ import annotations

import time

import pytest

from nanoquant.db import session_scope
from nanoquant.models import OrderRecord, WriteFailure
from nanoquant.services import dead_letter


def _add(kind: str = "_persist_entry", args=(), kwargs=None, *, resolved: bool = False,
         auto_attempts: int = 0) -> int:
    with session_scope() as s:
        row = WriteFailure(
            kind=kind,
            payload={"args": list(args), "kwargs": kwargs or {}},
            error="disk I/O error",
            attempts=3,
            auto_attempts=auto_attempts,
            resolved=resolved,
        )
        s.add(row)
        s.flush()
        return int(row.id)


def _add_order(cid: str) -> None:
    with session_scope() as s:
        s.add(
            OrderRecord(
                symbol="BTCUSDT",
                side="long",
                order_type="MARKET",
                client_order_id=cid,
            )
        )


def test_summary_counts_pending_and_stuck():
    before = dead_letter.summary()
    _add(kind="_persist_entry")
    _add(kind="_persist_close", auto_attempts=dead_letter.AUTO_MAX_ATTEMPTS)

    after = dead_letter.summary()
    assert after["pending"] >= before["pending"] + 2
    assert after["stuck"] >= before["stuck"] + 1, "达到自动重试上限的死信必须单独计数"
    kinds = {item["kind"]: item["count"] for item in after["by_kind"]}
    assert kinds.get("_persist_entry", 0) >= 1
    assert after["last_ts"] > 0, "有死信时应该能报出最近一条的时间"


def test_replay_missing_and_unsupported():
    assert dead_letter.replay_one(10**9)["status"] == "missing"
    fid = _add(kind="not_a_handler")
    assert dead_letter.replay_one(fid)["status"] == "unsupported"


def test_replay_skips_when_client_order_id_already_stored():
    """重放前先查订单表：同一 client_order_id 已在库里就不能重复记账。"""
    cid = f"cid-{time.time_ns()}"
    _add_order(cid)
    fid = _add(args=({"client_order_id": cid},))

    assert dead_letter.already_applied("_persist_entry", {"args": [{"client_order_id": cid}]}) is True
    res = dead_letter.replay_one(fid)
    assert res["ok"] is True
    assert res["status"] == "skipped"
    with session_scope() as s:
        assert s.get(WriteFailure, fid).resolved is True


def test_already_applied_is_false_without_client_order_id():
    assert dead_letter.already_applied("_persist_entry", {"args": [{}]}) is False


def test_replay_failure_increments_auto_attempts_only_for_auto(monkeypatch):
    def boom(*_args, **_kwargs):
        raise RuntimeError("still broken")

    monkeypatch.setattr(dead_letter, "_registry", lambda: {"_persist_entry": boom})
    fid = _add(args=({},))

    manual = dead_letter.replay_one(fid)
    assert manual["status"] == "failed"
    with session_scope() as s:
        assert (s.get(WriteFailure, fid).auto_attempts or 0) == 0, "手动重放不该消耗自动重试次数"

    auto = dead_letter.replay_one(fid, auto=True)
    assert auto["status"] == "failed"
    with session_scope() as s:
        assert s.get(WriteFailure, fid).auto_attempts == 1


def test_ignore_then_replay_reports_resolved():
    fid = _add()
    assert dead_letter.ignore_one(fid)["status"] == "ignored"
    assert dead_letter.ignore_one(10**9)["status"] == "missing"
    assert dead_letter.replay_one(fid)["status"] == "resolved"


def test_replay_all_aggregates_statuses():
    _add(kind="unknown_for_batch")
    stats = dead_letter.replay_all(limit=200)
    assert stats["total"] >= 1
    assert stats["unsupported"] >= 1
    assert set(stats) == {"total", "replayed", "skipped", "failed", "unsupported"}


@pytest.mark.parametrize("kind", ["_persist_entry", "_persist_close"])
def test_payload_cid_reads_first_arg_dict(kind):
    assert dead_letter._payload_cid(kind, {"args": [{"client_order_id": "abc"}]}) == "abc"
    assert dead_letter._payload_cid(kind, {"args": ["not-a-dict"]}) == ""
    assert dead_letter._payload_cid("other", {"args": [{"client_order_id": "abc"}]}) == ""
