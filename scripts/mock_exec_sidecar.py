"""用 Python 实现一份执行 sidecar 协议（TCP），用于在没有 Rust 工具链的机器上验证接入.

    python scripts/mock_exec_sidecar.py --port 9101 --secret "<base64 私钥或 HMAC>"
    EXEC_SOCKET=tcp:127.0.0.1:9101 python scripts/verify_sidecar.py

行为与 Rust 版一致：health / plan / sign，都只签名不发单。
"""
from __future__ import annotations

import argparse
import asyncio
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "backend"))

from nanoquant.exchange.client import _private_key, sign_query  # noqa: E402


def algo_of(secret: str) -> str:
    key = _private_key(secret)
    if key is None:
        return "hmac-sha256"
    return "ed25519" if type(key).__name__.startswith("Ed25519") else "rsa"


async def handle(reader: asyncio.StreamReader, writer: asyncio.StreamWriter, secret: str) -> None:
    algo = algo_of(secret)
    try:
        while True:
            line = await reader.readline()
            if not line:
                break
            try:
                req = json.loads(line.decode())
            except json.JSONDecodeError:
                writer.write((json.dumps({"id": "unknown", "ok": False, "error": "bad json"}) + "\n").encode())
                await writer.drain()
                continue

            req_id = str(req.get("id", ""))
            op = str(req.get("op", ""))
            query = str(req.get("query", ""))
            if op == "health":
                resp = {"id": req_id, "ok": True, "algo": algo}
            elif op in {"plan", "sign"} and query:
                resp = {
                    "id": req_id,
                    "ok": True,
                    "signature": sign_query(query, secret),
                    "algo": algo,
                    "query": query,
                }
            elif op in {"plan", "sign"}:
                resp = {"id": req_id, "ok": False, "error": "query 不能为空"}
            else:
                resp = {"id": req_id, "ok": False, "error": f"unknown op: {op}"}
            writer.write((json.dumps(resp) + "\n").encode())
            await writer.drain()
    finally:
        writer.close()


async def main_async(port: int, secret: str) -> None:
    server = await asyncio.start_server(lambda r, w: handle(r, w, secret), "127.0.0.1", port)
    print(f"mock sidecar on tcp:127.0.0.1:{port} algo={algo_of(secret)}")
    async with server:
        await server.serve_forever()


def main() -> int:
    parser = argparse.ArgumentParser(description="mock exec sidecar (TCP)")
    parser.add_argument("--port", type=int, default=9101)
    parser.add_argument("--secret", required=True)
    args = parser.parse_args()
    try:
        asyncio.run(main_async(args.port, args.secret))
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
