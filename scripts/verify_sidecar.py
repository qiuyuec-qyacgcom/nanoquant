"""验证 sidecar 协议与签名一致性（无需 Rust 工具链）.

流程：起一个 Python 版 mock sidecar → 用 ExecSidecar 走协议 → 与本地 sign_query 逐字节比对。

    python scripts/verify_sidecar.py
    # 指定真实 Rust sidecar（需同时给出它使用的 Secret 才能比对签名，
    # 否则只校验协议）：EXEC_SECRET=... python scripts/verify_sidecar.py --target unix:/run/nanoquant/exec.sock
"""
from __future__ import annotations

import argparse
import asyncio
import base64
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "backend"))
sys.path.insert(0, str(ROOT / "scripts"))

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey  # noqa: E402
from cryptography.hazmat.primitives.serialization import Encoding, NoEncryption, PrivateFormat  # noqa: E402

from mock_exec_sidecar import handle  # noqa: E402
from nanoquant.exchange.client import sign_query  # noqa: E402
from nanoquant.exchange.exec_client import ExecSidecar, ExecSidecarError  # noqa: E402

QUERIES = [
    "symbol=BTCUSDT&side=BUY&type=MARKET&quantity=0.001&timestamp=1700000000000",
    "symbol=ETHUSDT&side=SELL&type=STOP_MARKET&triggerPrice=2000&closePosition=true&timestamp=1700000001000",
]


async def run(target: str | None, secret: str | None = None) -> int:
    server = None
    if target is None:
        secret = secret or base64.b64encode(
            Ed25519PrivateKey.generate().private_bytes(Encoding.DER, PrivateFormat.PKCS8, NoEncryption())
        ).decode()
        server = await asyncio.start_server(lambda r, w: handle(r, w, secret), "127.0.0.1", 0)
        port = server.sockets[0].getsockname()[1]
        target = f"tcp:127.0.0.1:{port}"
    if not secret:
        print("[WARN] 未提供 Secret：只校验协议与回显，不比对签名（真实 sidecar 请传 --secret 或 EXEC_SECRET）")

    client = ExecSidecar(target)
    ok = True
    try:
        health = await client.health()
        print("[PASS] health →", health.get("algo"))
    except ExecSidecarError as exc:
        print("[FAIL] health:", exc)
        return 1

    for query in QUERIES:
        try:
            resp = await client.sign(query)
        except ExecSidecarError as exc:
            print("[FAIL] sign:", exc)
            ok = False
            continue
        expect = sign_query(query, secret) if secret else None
        if expect is not None and resp["signature"] != expect:
            print("[FAIL] 签名不一致")
            ok = False
        elif resp.get("query") != query:
            print("[FAIL] query 回显不一致")
            ok = False
        else:
            print(f"[PASS] sign {resp['algo']} · {len(resp['signature'])} chars")

    try:
        await client.sign("")
        print("[FAIL] 空 query 竟然成功")
        ok = False
    except ExecSidecarError:
        print("[PASS] 空 query 被拒绝")

    # 客户端是长连接：必须先断开，否则 mock server 的处理协程还挂着，
    # wait_closed() 会一直等下去（脚本自身的老坑，把诊断带偏过一次）
    await client.close()
    if server is not None:
        server.close()
        try:
            await asyncio.wait_for(server.wait_closed(), 3)
        except asyncio.TimeoutError:
            print("[WARN] mock server 未在 3s 内关闭（仍有连接未释放）")

    print("\n=== 结论:", "通过" if ok else "存在失败项", "===")
    return 0 if ok else 2


def main() -> int:
    parser = argparse.ArgumentParser(description="sidecar 协议验证")
    parser.add_argument("--target", default=None, help="默认自起 mock；也可指定 unix:/path 或 tcp:host:port")
    parser.add_argument(
        "--secret",
        default=os.getenv("EXEC_SECRET") or None,
        help="外部 target 的签名 Secret（也可用 EXEC_SECRET）；不给则只校验协议",
    )
    args = parser.parse_args()
    return asyncio.run(run(args.target, args.secret))


if __name__ == "__main__":
    raise SystemExit(main())
