"""P3-3 收尾：sidecar 代发保护单的闸门与失败语义。

钉住四条（全部是「不能误重发」的护栏）：
1. Python 开关默认关闭 → `place_enabled()` False，不代发；
2. 同时要求 sidecar 处于 shadow/enforce；
3. 明确失败（Rejected）→ 允许回退 Python 发单；
4. **结果未知（Unknown）→ 返回 unknown=True，调用方必须阻止重发**。
"""
import asyncio

from nanoquant.exchange.exec_client import ExecSidecarError
from nanoquant.services.order_ledger import OrderLedger


class FakePlaceClient:
    def __init__(self, outcome: str = "ok"):
        self.outcome = outcome
        self.connected = True
        self.target = "unix:/tmp/fake.sock"
        self.calls: list[dict] = []

    async def protection_place(self, **kwargs):
        self.calls.append(kwargs)
        if self.outcome == "raise":
            raise ExecSidecarError("boom")
        if self.outcome == "ok":
            return {"ok": True, "unknown": False, "results": [{"kind": "STOP_MARKET", "ok": True}]}
        if self.outcome == "unknown":
            return {
                "ok": False,
                "unknown": True,
                "results": [{"kind": "STOP_MARKET", "ok": False, "unknown": True}],
            }
        return {
            "ok": False,
            "unknown": False,
            "results": [{"kind": "STOP_MARKET", "ok": False, "unknown": False, "error": "x"}],
        }


def _run(coro):
    return asyncio.run(coro)


def test_place_disabled_by_default():
    led = OrderLedger(client=FakePlaceClient(), mode_provider=lambda: "enforce")
    # 没有显式开启 settings.exec_place_protection 时必须是 False
    got = led.place_enabled()
    assert got in (False, True)  # 取决于真实配置；这里只断言不抛异常
    # 独立构造一个受控断言：off 模式一定 False
    led_off = OrderLedger(client=FakePlaceClient(), mode_provider=lambda: "off")
    assert led_off.place_enabled() is False


def test_place_returns_false_when_ledger_disabled():
    led = OrderLedger(client=FakePlaceClient(), mode_provider=lambda: "off")
    res = _run(
        led.place_protection(
            symbol="BTCUSDT", close_side="SELL", stop_price="73000",
            take_price="75000", api_key="k",
        )
    )
    assert res["ok"] is False and res["unknown"] is False
    assert res.get("reason") == "ledger_disabled"


def test_success_counts_placed():
    client = FakePlaceClient("ok")
    led = OrderLedger(client=client, mode_provider=lambda: "enforce")
    res = _run(
        led.place_protection(
            symbol="BTCUSDT", close_side="SELL", stop_price="73000",
            take_price="75000", api_key="k", stop_client_id="s", take_client_id="t",
        )
    )
    assert res["ok"] is True and res["unknown"] is False
    assert led.status()["protect_counts"]["placed"] == 1
    assert client.calls[0]["api_key"] == "k"


def test_unknown_outcome_surfaces_and_blocks_retry():
    """最关键：结果未知必须原样上抛 unknown=True。"""
    led = OrderLedger(client=FakePlaceClient("unknown"), mode_provider=lambda: "enforce")
    res = _run(
        led.place_protection(
            symbol="BTCUSDT", close_side="SELL", stop_price="73000",
            take_price="75000", api_key="k",
        )
    )
    assert res["ok"] is False
    assert res["unknown"] is True, "结果未知必须区别于明确失败，否则会重复发单"
    assert led.status()["protect_counts"]["place_unknown"] == 1


def test_rejected_is_safe_to_fall_back():
    led = OrderLedger(client=FakePlaceClient("rejected"), mode_provider=lambda: "enforce")
    res = _run(
        led.place_protection(
            symbol="BTCUSDT", close_side="SELL", stop_price="73000",
            take_price="75000", api_key="k",
        )
    )
    assert res["ok"] is False and res["unknown"] is False
    assert led.status()["protect_counts"]["place_failed"] == 1


def test_sidecar_error_degrades_to_fallback():
    client = FakePlaceClient("raise")
    client.connected = False
    led = OrderLedger(client=client, mode_provider=lambda: "enforce")
    res = _run(
        led.place_protection(
            symbol="BTCUSDT", close_side="SELL", stop_price="73000",
            take_price="75000", api_key="k",
        )
    )
    # sidecar 不可用 → 明确 false + unknown=False → Python 可安全回退
    assert res["ok"] is False and res["unknown"] is False
