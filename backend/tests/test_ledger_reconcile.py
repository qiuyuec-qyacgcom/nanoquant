"""P3-5 收尾前置：sidecar 台账 ↔ Python orders 双向对账（只读）。

钉住四类差异分类与「不改任何数据」：
- missing_in_sidecar / missing_in_local / state_mismatch / qty_mismatch
- CANCELLED 与 CANCELED 视为同一状态（不同表述不算差异）
- sidecar 不可用时返回 ok=False 而不是抛异常
"""
import asyncio

from nanoquant.exchange.exec_client import ExecSidecarError
from nanoquant.services.order_ledger import OrderLedger


class FakeSnapshotClient:
    def __init__(self, orders=None, fail=False):
        self._orders = orders or []
        self.fail = fail
        self.connected = not fail
        self.target = "unix:/tmp/fake.sock"

    async def order_snapshot(self):
        if self.fail:
            raise ExecSidecarError("boom")
        return {"order": {"orders": self._orders, "total": len(self._orders)}}


def _ledger(client):
    return OrderLedger(client=client, mode_provider=lambda: "enforce")


def _run(coro):
    return asyncio.run(coro)


def test_sidecar_failure_returns_not_ok():
    res = _run(_ledger(FakeSnapshotClient(fail=True)).reconcile())
    assert res["ok"] is False
    assert "boom" in res["reason"]


def test_aligned_when_both_sides_identical(monkeypatch):
    """两侧完全相同 → aligned=True（用假本地表喂数据）。"""
    import nanoquant.services.order_ledger as mod

    class Row:
        def __init__(self, cid, status, qty, symbol="BTCUSDT"):
            self.client_order_id = cid
            self.status = status
            self.qty = qty
            self.symbol = symbol
            self.ts = 0

    fake_rows = [Row("c1", "filled", 0.01), Row("c2", "CANCELED", 0)]

    class FakeSession:
        def query(self, *_a, **_k):
            return self

        def filter(self, *_a, **_k):
            return self

        def order_by(self, *_a, **_k):
            return self

        def limit(self, *_a, **_k):
            return self

        def all(self):
            return fake_rows

        def __enter__(self):
            return self

        def __exit__(self, *_a):
            return False

    monkeypatch.setattr(mod, "session_scope", lambda: FakeSession(), raising=False)
    # 让函数内 import 拿到我们的假 session
    import nanoquant.db as dbmod

    monkeypatch.setattr(dbmod, "session_scope", lambda: FakeSession())

    client = FakeSnapshotClient(
        [
            {"client_order_id": "c1", "state": "FILLED", "filled_qty": 0.01},
            {"client_order_id": "c2", "state": "CANCELED", "filled_qty": 0},
        ]
    )
    res = _run(_ledger(client).reconcile())
    assert res["ok"] is True
    assert res["aligned"] is True, res
    assert res["diff_count"] == 0


def test_detects_all_four_diff_kinds(monkeypatch):
    import nanoquant.db as dbmod
    import nanoquant.services.order_ledger as mod

    class Row:
        def __init__(self, cid, status, qty):
            self.client_order_id = cid
            self.status = status
            self.qty = qty
            self.symbol = "BTCUSDT"
            self.ts = 0

    fake_rows = [
        Row("both_ok", "FILLED", 0.01),
        Row("state_diff", "ACKED", 0.01),     # sidecar 说 FILLED
        Row("qty_diff", "FILLED", 0.01),      # sidecar 说 0.05
        Row("only_local", "FILLED", 0.01),    # sidecar 没有
    ]

    class FakeSession:
        def query(self, *_a, **_k):
            return self

        def filter(self, *_a, **_k):
            return self

        def order_by(self, *_a, **_k):
            return self

        def limit(self, *_a, **_k):
            return self

        def all(self):
            return fake_rows

        def __enter__(self):
            return self

        def __exit__(self, *_a):
            return False

    monkeypatch.setattr(mod, "session_scope", lambda: FakeSession(), raising=False)
    monkeypatch.setattr(dbmod, "session_scope", lambda: FakeSession())

    client = FakeSnapshotClient(
        [
            {"client_order_id": "both_ok", "state": "FILLED", "filled_qty": 0.01},
            {"client_order_id": "state_diff", "state": "FILLED", "filled_qty": 0.01},
            {"client_order_id": "qty_diff", "state": "FILLED", "filled_qty": 0.05},
            {"client_order_id": "only_sidecar", "state": "FILLED", "filled_qty": 0.01},
        ]
    )
    res = _run(_ledger(client).reconcile())
    assert "only_local" in res["missing_in_sidecar"]
    assert "only_sidecar" in res["missing_in_local"]
    assert any(x["client_order_id"] == "state_diff" for x in res["state_mismatch"])
    assert any(x["client_order_id"] == "qty_diff" for x in res["qty_mismatch"])
    assert res["aligned"] is False
    assert res["diff_count"] >= 4
