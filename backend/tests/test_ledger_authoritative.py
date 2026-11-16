"""P3-5 收尾：sidecar 台账作为读取权威源（只读视图，可一键回退）。

钉住四条：
1. 开关默认关闭 → 不查 sidecar，`ledger_source=local`；
2. 开启且 sidecar 可用 → 状态类字段以 sidecar 为准，`ledger_source=sidecar`；
3. sidecar 不可用/异常 → **自动回退本地**，绝不 5xx；
4. 本地有、sidecar 没有的单保持本地值（不因权威源而消失）。
"""
import asyncio

from nanoquant.exchange.exec_client import ExecSidecarError
from nanoquant.services.order_ledger import OrderLedger


class FakeSnap:
    def __init__(self, orders=None, fail=False):
        self._orders = orders or []
        self.fail = fail
        self.connected = not fail
        self.target = "unix:/tmp/fake.sock"

    async def order_snapshot(self):
        if self.fail:
            raise ExecSidecarError("boom")
        return {"order": {"orders": self._orders}}


def _ledger(client, authoritative: bool):
    led = OrderLedger(client=client, mode_provider=lambda: "enforce")
    led.authoritative_enabled = lambda: bool(authoritative)  # noqa: ARG005
    return led


def _run(coro):
    return asyncio.run(coro)


def test_disabled_returns_none_so_caller_uses_local():
    led = _ledger(FakeSnap([{"client_order_id": "c1", "state": "FILLED"}]), authoritative=False)
    assert _run(led.authoritative_orders()) is None


def test_enabled_returns_state_only_view():
    led = _ledger(
        FakeSnap([
            {"client_order_id": "c1", "state": "filled", "filled_qty": 0.02, "avg_price": 74000.5},
        ]),
        authoritative=True,
    )
    out = _run(led.authoritative_orders())
    assert out is not None
    assert out["c1"]["state"] == "FILLED"
    assert out["c1"]["filled_qty"] == 0.02
    assert out["c1"]["avg_price"] == 74000.5


def test_sidecar_failure_returns_none_for_fallback():
    led = _ledger(FakeSnap(fail=True), authoritative=True)
    assert _run(led.authoritative_orders()) is None, "sidecar 挂了必须回退本地，不能抛"


def test_entries_without_client_order_id_are_skipped():
    led = _ledger(
        FakeSnap([{"client_order_id": "", "state": "FILLED"}, {"state": "FILLED"}]),
        authoritative=True,
    )
    out = _run(led.authoritative_orders())
    assert out == {}


def test_zero_filled_qty_and_price_are_preserved_as_zero():
    """sidecar 未见成交时应返回 0，由调用方决定是否覆盖（接口层只在 >0 时覆盖）。"""
    led = _ledger(FakeSnap([{"client_order_id": "c9", "state": "ACKED"}]), authoritative=True)
    out = _run(led.authoritative_orders())
    assert out["c9"]["filled_qty"] == 0.0
    assert out["c9"]["avg_price"] == 0.0
