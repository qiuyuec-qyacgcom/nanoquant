"""P3-3：保护单并行构造对账（sidecar 构造 vs Python 本地构造）.

钉住三条：
1. 两边 query 逐字节一致 → `protect_counts.matched++`；
2. 不一致只计数并留样本，**不抛异常**（真实保护单不受影响）；
3. sidecar 报错只计数，绝不冒泡到下单链路。
"""
import asyncio
from urllib.parse import urlencode

from nanoquant.exchange.exec_client import ExecSidecarError
from nanoquant.services.order_ledger import OrderLedger


def _local_query(symbol, close_side, kind, px, cid, ps=""):
    """复刻 broker._place_protection 的 algo_params 插入顺序。"""
    params = {
        "algoType": "CONDITIONAL",
        "symbol": symbol,
        "side": close_side,
        "type": kind,
        "triggerPrice": px,
        "closePosition": "true",
        "workingType": "MARK_PRICE",
        "priceProtect": "TRUE",
        "clientAlgoId": cid,
    }
    if ps:
        params["positionSide"] = ps
    return urlencode(params, doseq=True)


class FakeProtectionClient:
    def __init__(self, force_mismatch: bool = False, fail: bool = False):
        self.force_mismatch = force_mismatch
        self.fail = fail
        self.connected = True
        self.target = "unix:/tmp/fake.sock"
        self.calls: list[dict] = []

    async def protection_plan(self, **kwargs):
        if self.fail:
            raise ExecSidecarError("boom")
        self.calls.append(kwargs)
        symbol = kwargs["symbol"]
        cs = kwargs["close_side"]
        ps = kwargs.get("position_side", "")
        stop_q = _local_query(symbol, cs, "STOP_MARKET", kwargs["stop_price"],
                              kwargs["stop_client_id"], ps)
        take_q = _local_query(symbol, cs, "TAKE_PROFIT_MARKET", kwargs["take_price"],
                              kwargs["take_client_id"], ps)
        if self.force_mismatch:
            stop_q = stop_q + "&tampered=1"
        return [
            {"kind": "STOP_MARKET", "query": stop_q, "signature": "s1"},
            {"kind": "TAKE_PROFIT_MARKET", "query": take_q, "signature": "s2"},
        ]


def _run(coro):
    return asyncio.run(coro)


def test_matching_queries_count_as_matched():
    client = FakeProtectionClient()
    led = OrderLedger(client=client, mode_provider=lambda: "shadow")
    legs = [
        ("STOP_MARKET", "73000", _local_query("BTCUSDT", "SELL", "STOP_MARKET", "73000", "np1")),
        ("TAKE_PROFIT_MARKET", "75000",
         _local_query("BTCUSDT", "SELL", "TAKE_PROFIT_MARKET", "75000", "np2")),
    ]

    async def scenario():
        led.observe_protection(
            "BTCUSDT", "SELL", legs, stop_client_id="np1", take_client_id="np2"
        )
        await asyncio.sleep(0.15)

    _run(scenario())
    assert led.status()["protect_counts"]["matched"] == 1
    assert led.status()["protect_counts"]["mismatched"] == 0


def test_hedge_position_side_is_forwarded_and_matches():
    client = FakeProtectionClient()
    led = OrderLedger(client=client, mode_provider=lambda: "shadow")
    legs = [
        ("STOP_MARKET", "2500.5",
         _local_query("ETHUSDT", "BUY", "STOP_MARKET", "2500.5", "np1", "SHORT")),
        ("TAKE_PROFIT_MARKET", "2400",
         _local_query("ETHUSDT", "BUY", "TAKE_PROFIT_MARKET", "2400", "np2", "SHORT")),
    ]

    async def scenario():
        led.observe_protection(
            "ETHUSDT", "BUY", legs, position_side="SHORT",
            stop_client_id="np1", take_client_id="np2",
        )
        await asyncio.sleep(0.15)

    _run(scenario())
    assert client.calls and client.calls[0]["position_side"] == "SHORT"
    assert led.status()["protect_counts"]["matched"] == 1


def test_mismatch_is_recorded_not_raised():
    client = FakeProtectionClient(force_mismatch=True)
    led = OrderLedger(client=client, mode_provider=lambda: "shadow")
    legs = [
        ("STOP_MARKET", "73000", _local_query("BTCUSDT", "SELL", "STOP_MARKET", "73000", "np1")),
    ]

    async def scenario():
        led.observe_protection("BTCUSDT", "SELL", legs, stop_client_id="np1")
        await asyncio.sleep(0.15)

    _run(scenario())  # 不得抛异常
    st = led.status()
    assert st["protect_counts"]["mismatched"] == 1
    assert st["last_protect_mismatch"]["kind"] == "STOP_MARKET"


def test_sidecar_error_only_counts():
    client = FakeProtectionClient(fail=True)
    client.connected = False
    led = OrderLedger(client=client, mode_provider=lambda: "enforce")
    legs = [("STOP_MARKET", "73000", "q")]

    async def scenario():
        led.observe_protection("BTCUSDT", "SELL", legs)
        await asyncio.sleep(0.15)

    _run(scenario())
    assert led.status()["protect_counts"]["errors"] >= 1


def test_off_mode_skips_protection():
    client = FakeProtectionClient()
    led = OrderLedger(client=client, mode_provider=lambda: "off")
    legs = [("STOP_MARKET", "73000", "q")]

    async def scenario():
        led.observe_protection("BTCUSDT", "SELL", legs)
        await asyncio.sleep(0.05)

    _run(scenario())
    assert client.calls == []
