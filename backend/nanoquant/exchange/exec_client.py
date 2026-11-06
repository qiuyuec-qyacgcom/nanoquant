"""执行内核 sidecar 的 Python 客户端（JSON Lines over UDS / TCP）.

用途：
- `shadow` 模式：把同一批 intent 的 query 送给 Rust 签名，与本地签名逐字节比对；
- 后续真实下单时替换 `client._request` 的签名与发送。

传输：
- Linux/生产：`unix:/run/nanoquant/exec.sock`
- 本地开发（Windows 无 asyncio unix socket）：`tcp:127.0.0.1:9101`

协议见 docs/EXEC_PROTOCOL.md。
"""
from __future__ import annotations

import asyncio
import json
import os
import uuid
from typing import Any

DEFAULT_TARGET = "/run/nanoquant/exec.sock"


class ExecSidecarError(RuntimeError):
    pass


class ExecSidecar:
    def __init__(self, target: str | None = None, timeout: float = 3.0) -> None:
        self.target = target or os.getenv("EXEC_SOCKET") or DEFAULT_TARGET
        self.timeout = timeout

    # ---------------------------------------------------------------- transport
    async def _connect(self):
        target = self.target
        if target.startswith("tcp:"):
            host, _, port = target[4:].partition(":")
            return await asyncio.open_connection(host or "127.0.0.1", int(port or 9101))
        path = target[5:] if target.startswith("unix:") else target
        return await asyncio.open_unix_connection(path)

    # ------------------------------------------------------------------ request
    async def request(self, op: str, query: str = "") -> dict[str, Any]:
        req_id = uuid.uuid4().hex[:12]
        payload = json.dumps({"id": req_id, "op": op, "query": query}) + "\n"
        try:
            reader, writer = await asyncio.wait_for(self._connect(), timeout=self.timeout)
        except (OSError, asyncio.TimeoutError) as exc:
            raise ExecSidecarError(f"连接 sidecar 失败({self.target}): {exc}") from exc
        try:
            writer.write(payload.encode())
            await writer.drain()
            line = await asyncio.wait_for(reader.readline(), timeout=self.timeout)
        except asyncio.TimeoutError as exc:
            raise ExecSidecarError("sidecar 响应超时") from exc
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:  # noqa: BLE001
                pass
        if not line:
            raise ExecSidecarError("sidecar 连接被关闭")
        try:
            data = json.loads(line.decode())
        except json.JSONDecodeError as exc:
            raise ExecSidecarError(f"sidecar 返回非 JSON: {line[:80]!r}") from exc
        if not data.get("ok"):
            raise ExecSidecarError(str(data.get("error") or "sidecar 返回失败"))
        return data

    async def health(self) -> dict[str, Any]:
        return await self.request("health")

    async def sign(self, query: str) -> dict[str, Any]:
        """影子模式：只签名，不发单."""
        return await self.request("plan", query)


sidecar = ExecSidecar()
