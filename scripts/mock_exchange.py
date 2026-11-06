"""最小币安合约 mock 交易所: 让执行层在没有凭证的情况下端到端跑通.

只实现 broker 关键路径用到的接口:
    GET    /fapi/v1/time              校时
    POST   /fapi/v1/marginType        保证金模式
    POST   /fapi/v1/leverage          杠杆
    POST   /fapi/v1/order             下单 (开仓 / 平仓 / 止损 / 止盈)
    DELETE /fapi/v1/allOpenOrders     撤单
    GET    /fapi/v3/account           账户 (可选)

可选 `--delay-ms` 模拟公网 RTT, 用来对比"应用侧耗时"与"网络耗时"。
启动: python scripts/mock_exchange.py --port 8898 --delay-ms 0
"""
from __future__ import annotations

import argparse
import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

_seq = {"order": 1000}
STATE = {"delay_ms": 0.0, "orders": 0}


def _json_response(handler: BaseHTTPRequestHandler, payload: object, status: int = 200) -> None:
    body = json.dumps(payload).encode()
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # 静默
        pass

    def _delay(self) -> None:
        if STATE["delay_ms"]:
            time.sleep(STATE["delay_ms"] / 1000.0)

    def _query(self) -> dict[str, str]:
        parsed = urlparse(self.path)
        return {k: v[0] for k, v in parse_qs(parsed.query).items()}

    def do_GET(self) -> None:  # noqa: N802
        path = urlparse(self.path).path
        self._delay()
        if path == "/fapi/v1/time":
            _json_response(self, {"serverTime": int(time.time() * 1000)})
        elif path == "/fapi/v3/account":
            _json_response(
                self,
                {
                    "totalWalletBalance": "1000",
                    "totalUnrealizedProfit": "0",
                    "totalInitialMargin": "0",
                    "availableBalance": "1000",
                    "totalMarginBalance": "1000",
                },
            )
        elif path == "/fapi/v3/positionRisk":
            _json_response(self, [])
        elif path == "/fapi/v1/ping":
            _json_response(self, {})
        else:
            _json_response(self, {"code": -1121, "msg": f"mock: unsupported GET {path}"}, 400)

    def do_DELETE(self) -> None:  # noqa: N802
        path = urlparse(self.path).path
        self._delay()
        if path == "/fapi/v1/allOpenOrders":
            _json_response(self, {"code": 200, "msg": "success"})
        else:
            _json_response(self, {"code": -1121, "msg": f"mock: unsupported DELETE {path}"}, 400)

    def do_POST(self) -> None:  # noqa: N802
        path = urlparse(self.path).path
        params = self._query()
        self._delay()
        if path == "/fapi/v1/marginType":
            _json_response(self, {"code": 200, "msg": "success"})
            return
        if path == "/fapi/v1/leverage":
            _json_response(self, {"leverage": int(params.get("leverage", 20)), "symbol": params.get("symbol", "")})
            return
        if path == "/fapi/v1/order":
            _seq["order"] += 1
            STATE["orders"] += 1
            price = params.get("stopPrice") or params.get("price") or "100"
            qty = params.get("quantity", "0")
            _json_response(
                self,
                {
                    "orderId": _seq["order"],
                    "clientOrderId": params.get("newClientOrderId", ""),
                    "symbol": params.get("symbol", ""),
                    "status": "FILLED",
                    "avgPrice": price,
                    "executedQty": qty,
                    "cumQuote": "",
                    "updateTime": int(time.time() * 1000),
                },
            )
            return
        _json_response(self, {"code": -1121, "msg": f"mock: unsupported POST {path}"}, 400)


def serve(port: int = 8898, delay_ms: float = 0.0) -> ThreadingHTTPServer:
    STATE["delay_ms"] = delay_ms
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    return server


def main() -> int:
    parser = argparse.ArgumentParser(description="最小币安合约 mock")
    parser.add_argument("--port", type=int, default=8898)
    parser.add_argument("--delay-ms", type=float, default=0.0, help="模拟网络 RTT")
    args = parser.parse_args()
    server = serve(args.port, args.delay_ms)
    print(f"mock exchange on http://127.0.0.1:{args.port} delay={args.delay_ms}ms")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.shutdown()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
