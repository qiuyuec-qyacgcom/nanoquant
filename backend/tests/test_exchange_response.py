"""交易所响应解析与体检路由的降级行为.

日志里有过成串的 ``未处理异常 GET /api/v1/engine/trade-mode``：
交易所/代理回了 HTTP 200 + 非 JSON 内容时，``r.json()`` 抛 JSONDecodeError
穿透到路由变成 500。这里把两个环节各钉一条测试。
"""
from __future__ import annotations

import json

import pytest
from fastapi.testclient import TestClient

from nanoquant.exchange.client import BinanceError, _parse_response_json  # noqa: SLF001

API = "/api/v1"


class _Resp:
    def __init__(self, text: str, payload: object = None, status_code: int = 200) -> None:
        self.text = text
        self.status_code = status_code
        self._payload = payload

    def json(self) -> object:
        if self._payload is None:
            raise json.JSONDecodeError("Expecting value", self.text, 0)
        return self._payload


def test_empty_body_is_an_empty_result():
    assert _parse_response_json(_Resp("")) == {}
    assert _parse_response_json(_Resp("   \n ")) == {}


def test_normal_json_passes_through():
    assert _parse_response_json(_Resp('{"a": 1}', {"a": 1})) == {"a": 1}
    assert _parse_response_json(_Resp("[1,2]", [1, 2])) == [1, 2]


def test_non_json_body_becomes_binance_error():
    html = "<html><body>429 Too Many Requests</body></html>"
    with pytest.raises(BinanceError) as exc:
        _parse_response_json(_Resp(html, status_code=200))
    assert exc.value.code == -1
    assert "非 JSON" in exc.value.message
    assert "Too Many Requests" in exc.value.message
    assert exc.value.status == 200


@pytest.fixture(scope="module")
def client():
    from nanoquant.main import bootstrap, create_app

    bootstrap()
    c = TestClient(create_app())
    try:
        yield c
    finally:
        c.close()


def _token(client: TestClient) -> dict[str, str]:
    res = client.post(f"{API}/auth/login", json={"username": "admin", "password": "admin123"})
    assert res.status_code == 200, res.text
    return {"Authorization": f"Bearer {res.json()['token']}"}


def test_trade_mode_returns_503_instead_of_500(client, monkeypatch):
    from nanoquant.api.routes import trade as trade_routes

    class BrokenBroker:
        async def trade_mode_status(self, _symbols):
            raise BinanceError(-1, "交易所返回非 JSON (HTTP 200): '<html>...'")

    monkeypatch.setattr(trade_routes, "get_broker", lambda _mode: BrokenBroker())

    res = client.get(f"{API}/engine/trade-mode", headers=_token(client))
    assert res.status_code == 503
    assert "体检暂不可用" in res.json()["detail"]
