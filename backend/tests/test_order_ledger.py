"""P3-5：订单台账投递（Python → sidecar）.

钉住四条护栏：
1. `off` 模式完全不投递（零开销，保持纯 Python 记账）；
2. 首次投递**直接按实际状态建档**，不制造 `PLANNED → FILLED` 跨级跳；
3. sidecar 报错/不可用只计数，**绝不抛异常**（不能影响真实下单）；
4. sidecar 掉线后进入静默期，期间直接 skip（不每笔单都等一次超时）。
"""
import asyncio

from nanoquant.exchange.exec_client import ExecSidecarError
from nanoquant.services.order_ledger import OrderLedger


class FakeClient:
    def __init__(self, fail: bool = False, connected: bool = True):
        self.fail = fail
        self.connected = connected
        self.target = "unix:/tmp/fake.sock"
        # 模拟 sidecar 台账：cid -> state
        self.book: dict[str, str] = {}
        self.registered: list[tuple] = []
        self.transitions: list[tuple] = []

    async def order_register(self, cid, symbol, state="PLANNED"):
        if self.fail:
            raise ExecSidecarError("boom")
        self.registered.append((cid, symbol, state))
        if cid in self.book:
            return {"ok": True, "duplicate": True}
        self.book[cid] = state
        return {"ok": True, "duplicate": False}

    async def order_transition(self, cid, state, filled_qty=None, avg_price=None):
        if self.fail:
            raise ExecSidecarError("boom")
        if cid not in self.book:
            raise ExecSidecarError("未知订单")
        self.transitions.append((cid, state, filled_qty, avg_price))
        self.book[cid] = state
        return {"ok": True}


def _run(coro):
    return asyncio.run(coro)


def _drain(seconds: float = 0.1):
    async def _inner():
        await asyncio.sleep(seconds)

    _run(_inner())


def test_off_mode_never_delivers():
    client = FakeClient()
    ledger = OrderLedger(client=client, mode_provider=lambda: "off")

    async def scenario():
        ledger.observe("nq1", "BTCUSDT", "FILLED", 0.01, 100.0)
        await asyncio.sleep(0.05)

    _run(scenario())
    assert client.registered == [] and client.transitions == []
    assert ledger.status()["enabled"] is False


def test_first_delivery_uses_actual_state_not_planned():
    """关键回归：首次建档必须直接用 FILLED，而不是 PLANNED 再跨级迁移。"""
    client = FakeClient()
    ledger = OrderLedger(client=client, mode_provider=lambda: "shadow")

    async def scenario():
        ledger.observe("nq1", "BTCUSDT", "FILLED", 0.01, 73800.5)
        await asyncio.sleep(0.1)

    _run(scenario())
    assert client.registered == [("nq1", "BTCUSDT", "FILLED")]
    assert client.book["nq1"] == "FILLED"
    assert ledger.status()["counts"]["registered"] == 1


def test_second_observation_transitions_legally():
    client = FakeClient()
    ledger = OrderLedger(client=client, mode_provider=lambda: "shadow")

    async def scenario():
        ledger.observe("nq1", "BTCUSDT", "ACKED")       # 挂单先 ACKED
        await asyncio.sleep(0.1)
        ledger.observe("nq1", "BTCUSDT", "FILLED", 0.01, 100.0)  # 成交后迁移
        await asyncio.sleep(0.1)

    _run(scenario())
    assert client.registered == [("nq1", "BTCUSDT", "ACKED"), ("nq1", "BTCUSDT", "FILLED")]
    assert ("nq1", "FILLED", 0.01, 100.0) in client.transitions
    assert client.book["nq1"] == "FILLED"


def test_same_state_is_deduplicated():
    client = FakeClient()
    ledger = OrderLedger(client=client, mode_provider=lambda: "shadow")

    async def scenario():
        ledger.observe("nq1", "BTCUSDT", "FILLED", 0.01, 100.0)
        ledger.observe("nq1", "BTCUSDT", "FILLED", 0.01, 100.0)
        await asyncio.sleep(0.1)

    _run(scenario())
    assert len(client.registered) == 1, "同一 (cid, state) 只投递一次"
    assert ledger.status()["counts"]["duplicates"] >= 1


def test_sidecar_failure_never_raises():
    """台账失败必须被吞掉：下单链路不能因为观测组件挂掉而失败。"""
    client = FakeClient(fail=True, connected=False)
    ledger = OrderLedger(client=client, mode_provider=lambda: "enforce")

    async def scenario():
        ledger.observe("nq1", "BTCUSDT", "FILLED", 0.01, 100.0)  # 不应抛
        await asyncio.sleep(0.1)

    _run(scenario())
    st = ledger.status()
    assert st["counts"]["errors"] >= 1
    assert st["last_error"]


def test_down_cooldown_skips_without_network():
    client = FakeClient(fail=True, connected=False)
    ledger = OrderLedger(client=client, mode_provider=lambda: "shadow")

    async def scenario():
        ledger.observe("nq1", "BTCUSDT")
        await asyncio.sleep(0.1)
        before = ledger.status()["counts"]["skipped"]
        ledger.observe("nq2", "ETHUSDT")
        await asyncio.sleep(0.05)
        return before, ledger.status()["counts"]["skipped"]

    before, after = _run(scenario())
    assert after > before, "静默期内必须跳过而非继续打点"
    assert ledger.status()["cooldown_ms"] >= 0


def test_empty_identifiers_are_ignored():
    client = FakeClient()
    ledger = OrderLedger(client=client, mode_provider=lambda: "shadow")

    async def scenario():
        ledger.observe("", "BTCUSDT")
        ledger.observe("nq1", "")
        await asyncio.sleep(0.05)

    _run(scenario())
    assert client.registered == []
