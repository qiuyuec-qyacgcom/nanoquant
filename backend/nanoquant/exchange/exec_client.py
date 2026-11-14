"""执行内核 sidecar 的 Python 客户端（JSON Lines over UDS / TCP）.

用途：
- `shadow` 模式：把同一批 intent 的 query 送给 Rust 签名，与本地签名逐字节比对；
- 后续真实下单时替换 `client._request` 的签名与发送。

传输：
- Linux/生产：`unix:/run/nanoquant/exec.sock`
- 本地开发（Windows 无 asyncio unix socket）：`tcp:127.0.0.1:9101`

实现要点：
- **长连接复用**：协议是行分隔的请求/响应，一条连接可以连发多个请求；
  每单重建连接会把握手成本（本地回环 ~0.1ms，跨进程仍不可忽略）加到签名路径上。
- **串行化**：同一连接上的请求/响应必须一一对应，用 `asyncio.Lock` 保证不交错。
- **失败即丢弃连接**：超时/断流后不复用，下一次请求重新建连，避免读到上一条的迟到响应。

协议见 docs/EXEC_PROTOCOL.md。
"""
from __future__ import annotations

import asyncio
import json
import os
import time
import uuid
from typing import Any

from ..logging_setup import get_logger

log = get_logger("exec_client")

DEFAULT_TARGET = "/run/nanoquant/exec.sock"


class ExecSidecarError(RuntimeError):
    pass


class ExecSidecar:
    def __init__(self, target: str | None = None, timeout: float = 3.0) -> None:
        self._target = target
        self.timeout = timeout
        self._reader: asyncio.StreamReader | None = None
        self._writer: asyncio.StreamWriter | None = None
        self._lock: asyncio.Lock | None = None
        self.connected = False
        self.algo = ""
        self.last_error = ""
        self.last_ms = 0.0
        self.counters: dict[str, int] = {"requests": 0, "errors": 0, "connects": 0, "timeouts": 0}

    # ------------------------------------------------------------------ config
    @property
    def target(self) -> str:
        """配置可热更新：每次取，改了 socket 路径不需要重启进程."""
        return self._target or os.getenv("EXEC_SOCKET") or DEFAULT_TARGET

    def reconfigure(self, target: str | None, timeout: float | None = None) -> None:
        """热更新地址; 传空串表示"回落到 EXEC_SOCKET 环境变量"."""
        if target is not None and target != (self._target or ""):
            self._target = target or None
            self._drop()
        if timeout:
            self.timeout = timeout

    # --------------------------------------------------------------- transport
    async def _connect(self):
        target = self.target
        if target.startswith("tcp:"):
            host, _, port = target[4:].partition(":")
            return await asyncio.open_connection(host or "127.0.0.1", int(port or 9101))
        path = target[5:] if target.startswith("unix:") else target
        if not hasattr(asyncio, "open_unix_connection"):  # pragma: no cover - Windows
            # Windows 上这个属性根本不存在, 直接 AttributeError 会让人以为是配置问题之外的事
            raise ExecSidecarError(
                "当前平台不支持 unix socket, 请把执行内核地址配成 tcp:127.0.0.1:9101"
            )
        return await asyncio.open_unix_connection(path)

    async def _ensure(self) -> None:
        if self._writer is not None and not self._writer.is_closing():
            return
        reader, writer = await self._connect()
        self._reader = reader
        self._writer = writer
        self.connected = True
        self.counters["connects"] += 1

    def _drop(self) -> None:
        writer = self._writer
        self._reader = None
        self._writer = None
        self.connected = False
        if writer is not None:
            try:
                writer.close()
            except Exception:  # noqa: BLE001
                # 断线清理路径: 连接可能已经半关闭, 关不掉是常态, 只留 debug 线索
                log.debug("关闭执行通道时出错(已忽略)", exc_info=True)

    def _guard(self) -> asyncio.Lock:
        if self._lock is None:
            self._lock = asyncio.Lock()
        return self._lock

    # ------------------------------------------------------------------ request
    async def request(self, op: str, query: str = "") -> dict[str, Any]:
        payload = json.dumps({"id": uuid.uuid4().hex[:12], "op": op, "query": query}) + "\n"
        async with self._guard():
            self.counters["requests"] += 1
            # 计时从拿到锁开始: 排队时长不是 sidecar 的往返, 算进去会让宕机时的 p99 虚高
            started = time.perf_counter()
            try:
                await asyncio.wait_for(self._ensure(), timeout=self.timeout)
                assert self._writer is not None and self._reader is not None
                self._writer.write(payload.encode())
                await self._writer.drain()
                line = await asyncio.wait_for(self._reader.readline(), timeout=self.timeout)
            except asyncio.TimeoutError as exc:
                self._drop()
                self.counters["timeouts"] += 1
                self.counters["errors"] += 1
                self.last_error = f"sidecar 响应超时({self.timeout * 1000:.0f}ms)"
                raise ExecSidecarError(self.last_error) from exc
            except (OSError, ValueError, AttributeError) as exc:
                self._drop()
                self.counters["errors"] += 1
                self.last_error = f"连接 sidecar 失败({self.target}): {exc}"
                raise ExecSidecarError(self.last_error) from exc
            finally:
                self.last_ms = round((time.perf_counter() - started) * 1000, 3)

        if not line:
            self._drop()
            self.counters["errors"] += 1
            self.last_error = "sidecar 连接被关闭"
            raise ExecSidecarError(self.last_error)
        try:
            data = json.loads(line.decode())
        except json.JSONDecodeError as exc:
            self.counters["errors"] += 1
            self.last_error = f"sidecar 返回非 JSON: {line[:80]!r}"
            raise ExecSidecarError(self.last_error) from exc
        if not data.get("ok"):
            self.counters["errors"] += 1
            self.last_error = str(data.get("error") or "sidecar 返回失败")
            raise ExecSidecarError(self.last_error)
        if data.get("algo"):
            self.algo = str(data["algo"])
        # 最近一次已成功: 清掉历史错误, 否则面板会一直挂着早已恢复的超时
        self.last_error = ""
        # 在锁内计时, 不含排队等待: 影子比对要的是 sidecar 真实往返, 不是排队时长
        data["elapsed_ms"] = self.last_ms
        return data

    async def health(self) -> dict[str, Any]:
        return await self.request("health")

    async def sign(self, query: str) -> dict[str, Any]:
        """影子模式：只签名，不发单."""
        return await self.request("plan", query)

    async def close(self) -> None:
        writer = self._writer
        self._drop()
        if writer is not None:
            try:
                await writer.wait_closed()
            except Exception:  # noqa: BLE001
                log.debug("等待执行通道关闭超时(已忽略)", exc_info=True)


sidecar = ExecSidecar()
