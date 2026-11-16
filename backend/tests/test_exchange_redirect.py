"""交易所返回 HTTP 302（nginx 重定向 HTML）时的可操作诊断.

用户实测：VPS 上「开仓前置条件」的交易通道/资金与风控都报
`交易所返回非 JSON (HTTP 302): '<html>...302 Found...nginx'`，
但错误没告诉人是 IP 被拦还是 base_url 配错。现在 3xx 单独归因。
"""
from nanoquant.exchange.client import BinanceError, _parse_response_json


class FakeResponse:
    def __init__(self, status, text, headers=None):
        self.status_code = status
        self.text = text
        self.headers = headers or {}

    def json(self):
        import json

        return json.loads(self.text)


def test_302_reports_location_and_actionable_hint():
    r = FakeResponse(
        302,
        "<html><head><title>302 Found</title></head><body><center><h1>302 Found</h1></center></body></html>",
        {"location": "https://www.binance.com/en/register?ref=x"},
    )
    try:
        _parse_response_json(r)
    except BinanceError as exc:
        assert exc.status == 302
        assert "HTTP 302" in str(exc)
        assert "重定向目标" in str(exc)
        assert "出口 IP" in str(exc) or "base_url" in str(exc)
    else:
        raise AssertionError("302 必须抛 BinanceError")


def test_302_without_location_still_actionable():
    r = FakeResponse(302, "<html>302</html>")
    try:
        _parse_response_json(r)
    except BinanceError as exc:
        assert "HTTP 302" in str(exc)
        assert "代理配置错误" in str(exc)
    else:
        raise AssertionError("302 必须抛 BinanceError")


def test_200_html_keeps_non_json_diagnosis():
    r = FakeResponse(200, "<html>cloudflare challenge</html>")
    try:
        _parse_response_json(r)
    except BinanceError as exc:
        assert "非 JSON (HTTP 200)" in str(exc)
        assert "HTML 验证页" in str(exc)
    else:
        raise AssertionError("HTML 必须抛 BinanceError")


def test_empty_body_is_still_empty_dict():
    assert _parse_response_json(FakeResponse(200, "")) == {}


def test_normal_json_unchanged():
    assert _parse_response_json(FakeResponse(200, '{"ok":true}')) == {"ok": True}
