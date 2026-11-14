"""行情推送链路：K 线 hub 的订阅/解析/背压，以及 WebSocket 端点的准入.

这条链路的典型故障是"界面不动了但没有任何报错"：队列满了被静默丢弃、
token 过期后连接半死不活、订阅表泄漏。下面按这几种情况逐个钉住。
"""
from __future__ import annotations

import asyncio
import json

import pytest
from fastapi.testclient import TestClient
from starlette.websockets import WebSocketDisconnect

from nanoquant.exchange.kline_stream import ALLOWED_INTERVALS, KlineStreamHub, stream_name

API = "/api/v1"


def _kline_msg(symbol: str = "BTCUSDT", interval: str = "1m", close: str = "100.5") -> str:
    return json.dumps(
        {
            "e": "kline",
            "E": 1_700_000_000_000,
            "k": {
                "s": symbol,
                "i": interval,
                "t": 1_699_999_940_000,
                "T": 1_700_000_039_999,
                "o": "100.0",
                "h": "101.0",
                "l": "99.5",
                "c": close,
                "v": "12.5",
                "x": True,
            },
        }
    )


# ------------------------------------------------------------------ 纯函数
def test_stream_name_is_lowercase_binance_format():
    assert stream_name("BTCUSDT", "1m") == "btcusdt@kline_1m"


# ------------------------------------------------------------------ hub 行为
def test_subscribe_and_unsubscribe_track_state():
    async def scenario():
        hub = KlineStreamHub()
        q = await hub.subscribe("btcusdt", "1m")
        status = hub.status()
        assert status["subscriptions"] == ["BTCUSDT@1m"], "symbol 统一成大写"
        # clients 统计的是订阅队列数（每个 WebSocket 连接一个），订阅即 +1
        assert status["clients"] == 1

        hub._dispatch(_kline_msg())
        data = q.get_nowait()
        assert data["symbol"] == "BTCUSDT" and data["c"] == 100.5
        assert data["closed"] is True
        assert data["next_close_at"] == 1_700_000_040_000

        await hub.unsubscribe("BTCUSDT", "1m", q)
        assert hub.status()["subscriptions"] == []
        assert hub.status()["clients"] == 0

    asyncio.run(scenario())


def test_dispatch_ignores_garbage_without_raising():
    async def scenario():
        hub = KlineStreamHub()
        q = await hub.subscribe("BTCUSDT", "1m")
        for bad in ("", "not json", "{}", json.dumps({"e": "aggTrade"}),
                    json.dumps({"e": "kline", "k": {"s": "BTCUSDT"}})):
            hub._dispatch(bad)  # 不抛异常即可
        assert q.empty()

    asyncio.run(scenario())


def test_dispatch_handles_combined_stream_wrapper():
    """/stream?streams= 形式的事件包在 data 里, 顶层解析会漏."""
    async def scenario():
        hub = KlineStreamHub()
        q = await hub.subscribe("BTCUSDT", "1m")
        hub._dispatch(json.dumps({"stream": "btcusdt@kline_1m", "data": json.loads(_kline_msg())}))
        assert q.get_nowait()["symbol"] == "BTCUSDT"

    asyncio.run(scenario())


def test_full_queue_drops_instead_of_blocking():
    """慢客户端不能把整条行情链路拖死: 队列满时丢弃这一条, 解析继续。"""
    async def scenario():
        hub = KlineStreamHub()
        q = await hub.subscribe("BTCUSDT", "1m")
        for _ in range(q.maxsize):
            q.put_nowait({"filler": True})
        assert q.full()

        hub._dispatch(_kline_msg())  # 不得抛 QueueFull
        assert q.qsize() == q.maxsize
        assert hub._last_msg_ts > 0, "即使丢了这一条, 也该更新最后收到消息的时间"

    asyncio.run(scenario())


def test_status_reports_last_message_age():
    hub = KlineStreamHub()
    assert hub.status()["last_message_age"] is None
    hub._last_msg_ts = 1.0
    assert hub.status()["last_message_age"] is not None


# ------------------------------------------------------------------ WebSocket 端点
@pytest.fixture(scope="module")
def client():
    from nanoquant.main import bootstrap, create_app

    bootstrap()
    c = TestClient(create_app())
    try:
        yield c
    finally:
        c.close()


def _token(client: TestClient) -> str:
    res = client.post(f"{API}/auth/login", json={"username": "admin", "password": "admin123"})
    assert res.status_code == 200, res.text
    return res.json()["token"]


def _auth_frame(token: str = "bad.token") -> str:
    return json.dumps({"type": "auth", "token": token})


def test_event_ws_rejects_invalid_token(client):
    with pytest.raises(WebSocketDisconnect) as exc:
        with client.websocket_connect(f"{API}/ws") as ws:
            ws.send_text(_auth_frame())
            ws.receive_text()
    assert exc.value.code == 4401


def test_event_ws_rejects_non_auth_first_frame(client):
    # 首帧不是 auth：立即判失败关闭，而不是干等 5s 超时
    with pytest.raises(WebSocketDisconnect) as exc:
        with client.websocket_connect(f"{API}/ws") as ws:
            ws.send_text(json.dumps({"type": "ping"}))
            ws.receive_text()
    assert exc.value.code == 4401


def test_event_ws_sends_hello_with_valid_token(client):
    with client.websocket_connect(f"{API}/ws") as ws:
        ws.send_text(_auth_frame(_token(client)))
        hello = json.loads(ws.receive_text())
    assert hello["type"] == "hello" and hello["data"]["ok"] is True


def test_kline_ws_rejects_bad_interval(client):
    with pytest.raises(WebSocketDisconnect) as exc:
        with client.websocket_connect(f"{API}/ws/klines?symbol=BTCUSDT&interval=7m") as ws:
            ws.receive_text()
    assert exc.value.code == 4400
    assert "7m" not in ALLOWED_INTERVALS


def test_kline_ws_rejects_symbol_outside_pool(client):
    """不在标的池里的合约不提供推送, 否则任何人都能借服务端打交易所接口。"""
    with pytest.raises(WebSocketDisconnect) as exc:
        with client.websocket_connect(f"{API}/ws/klines?symbol=NOTACOIN&interval=1m") as ws:
            ws.receive_text()
    assert exc.value.code == 4404


def test_kline_ws_requires_login_or_public_site(client):
    # 未登录且前台未开启 → 拒绝（首帧不带 auth，按匿名处理）
    from nanoquant.services.settings import get_public, update_group

    pub = get_public()
    update_group("public", {"site_enabled": False})
    try:
        with pytest.raises(WebSocketDisconnect) as exc:
            with client.websocket_connect(f"{API}/ws/klines?symbol=BTCUSDT&interval=1m") as ws:
                ws.receive_text()
        assert exc.value.code == 4403
    finally:
        update_group("public", {"site_enabled": pub.site_enabled})


def test_kline_ws_admin_boots_over_closed_site(client):
    """站点关闭时匿名 4403，但管理员首帧带合法 token 应能连上."""
    from nanoquant.db import session_scope
    from nanoquant.models import TradingSymbol
    from nanoquant.services.settings import get_public, update_group

    with session_scope() as s:
        if not s.get(TradingSymbol, "BTCUSDT"):
            s.add(TradingSymbol(symbol="BTCUSDT"))

    pub = get_public()
    update_group("public", {"site_enabled": False})
    try:
        with client.websocket_connect(f"{API}/ws/klines?symbol=BTCUSDT&interval=1m") as ws:
            ws.send_text(_auth_frame(_token(client)))
            hello = json.loads(ws.receive_text())
        assert hello["type"] == "hello"
    finally:
        update_group("public", {"site_enabled": pub.site_enabled})
