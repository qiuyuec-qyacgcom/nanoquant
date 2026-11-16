"""P3-5：把 Python 侧订单生命周期投递给 sidecar 台账（shadow 对账，绝不拦单）.

背景：
- P2 的 `exec_shadow` 只比对**签名**，不知道订单走到了哪一步；
- P3-2 在 sidecar 建了幂等订单状态机（Planned → Sent → Acked → Filled），
  但还没有任何人告诉它「这笔单发生了什么」。

本模块负责这条投递链路，设计上刻意与交易关键路径**解耦**：

| 约束 | 做法 |
|---|---|
| 绝不阻塞下单 | 投递一律 `create_task` 后台执行（fire-and-forget），失败只计数 |
| 绝不拦单 | 与 `exec_shadow.enforce` 不同，这里**没有任何拒绝路径**；台账投递失败不影响交易 |
| sidecar 掉线不刷屏 | 连续失败进入 `DOWN_COOLDOWN` 静默期，期间直接 skip |
| 幂等 | 以 `clientOrderId` 为准，重复 register 由 sidecar 去重，不会产生重复台账 |
| 不重复投递终态 | 已投递过的 `(cid, state)` 不再重复发送，避免每轮轮询都打一次 |

开关复用 `system.exec_mode`：`off` 时不投递（保持纯 Python 记账）。
"""

from __future__ import annotations

import asyncio
import time
from collections import deque
from typing import Any, Callable

from ..exchange.exec_client import ExecSidecar, ExecSidecarError
from ..logging_setup import get_logger

log = get_logger("order_ledger")

#: 同时在途的投递任务上限（下单高峰时防止任务表膨胀）
MAX_INFLIGHT = 32
#: sidecar 掉线后的静默期（秒）：期间直接跳过，避免每笔单都等一次超时
DOWN_COOLDOWN = 5.0
#: 已投递终态的去重集合上限（防止内存无界增长）
SEEN_LIMIT = 2000
#: 配置读取缓存（秒）
CFG_TTL = 2.0


class OrderLedger:
    def __init__(
        self,
        client: ExecSidecar | None = None,
        mode_provider: Callable[[], str] | None = None,
    ) -> None:
        self._client = client or ExecSidecar()
        self._mode_provider = mode_provider or self._default_mode
        self._mode = "off"
        self._mode_ts = 0.0
        self._tasks: set[asyncio.Task] = set()
        self._seen: deque[str] = deque(maxlen=SEEN_LIMIT)
        self._seen_set: set[str] = set()
        self._down_until = 0.0
        self.counts = {
            "registered": 0,
            "transitioned": 0,
            "duplicates": 0,
            "skipped": 0,
            "errors": 0,
            "inflight": 0,
        }
        self.last_error = ""

    @staticmethod
    def _default_mode() -> str:
        from .settings import get_system

        return str(getattr(get_system(), "exec_mode", "off") or "off")

    def _mode_of(self) -> str:
        """`off` 时不投递；`shadow`/`enforce` 都投递（台账只读，不拦单）。"""
        now = time.monotonic()
        if now - self._mode_ts >= CFG_TTL:
            try:
                self._mode = self._mode_provider()
            except Exception:  # noqa: BLE001  配置读不到按关闭处理，绝不阻塞交易
                self._mode = "off"
            self._mode_ts = now
        return self._mode

    def enabled(self) -> bool:
        return self._mode_of() in {"shadow", "enforce"}

    def reset(self) -> None:
        self.counts = {
            "registered": 0,
            "transitioned": 0,
            "duplicates": 0,
            "skipped": 0,
            "errors": 0,
            "inflight": 0,
        }
        self._seen.clear()
        self._seen_set.clear()
        self._down_until = 0.0
        self.last_error = ""

    # ------------------------------------------------------------- 关键路径
    def _remember(self, key: str) -> bool:
        """记录已投递的 (cid,state)；返回 True 表示「之前没见过」。"""
        if key in self._seen_set:
            return False
        if len(self._seen) == self._seen.maxlen:
            oldest = self._seen[0]
            self._seen_set.discard(oldest)
        self._seen.append(key)
        self._seen_set.add(key)
        return True

    def _spawn(self, coro) -> None:
        if self.counts["inflight"] >= MAX_INFLIGHT:
            coro.close()  # 丢弃未执行的协程，避免 "never awaited" 警告
            self.counts["skipped"] += 1
            return
        try:
            task = asyncio.get_running_loop().create_task(coro)
        except RuntimeError:  # 没有事件循环（写库线程/同步调用）→ 放弃投递
            coro.close()
            self.counts["skipped"] += 1
            return
        self.counts["inflight"] += 1
        self._tasks.add(task)
        task.add_done_callback(self._done)

    def _done(self, task: asyncio.Task) -> None:
        self._tasks.discard(task)
        self.counts["inflight"] = max(0, self.counts["inflight"] - 1)
        try:
            task.result()
        except asyncio.CancelledError:
            pass
        except Exception:  # noqa: BLE001
            log.debug("订单台账投递任务异常退出", exc_info=True)

    def observe(
        self,
        client_order_id: str,
        symbol: str,
        state: str = "",
        filled_qty: float | None = None,
        avg_price: float | None = None,
    ) -> None:
        """投递一次订单观测（register + 必要时的 transition）。

        关键：首次登记**直接用实际状态**建档，而不是固定 `PLANNED` 再跨级迁移——
        `PLANNED → FILLED` 是状态机明确拒绝的异常路径（无成交过程）。
        已登记过的单（`duplicate=true`）才走 `transition`，此时路径合法
        （如挂单先 `ACKED`、成交后 `ACKED → FILLED`）。
        """
        cid = str(client_order_id or "").strip()
        if not cid or not symbol:
            return
        if not self.enabled():
            return
        if time.monotonic() < self._down_until:
            self.counts["skipped"] += 1
            return
        st = str(state or "").strip().upper() or "PLANNED"
        # 已投递过同一个 (cid, 状态) 就不再重复打点
        if not self._remember(f"{cid}:{st}"):
            self.counts["duplicates"] += 1
            return
        self._spawn(self._do_observe(cid, symbol, st, filled_qty, avg_price))

    # ------------------------------------------------------------------ 投递
    async def _do_observe(
        self,
        cid: str,
        symbol: str,
        state: str,
        filled_qty: float | None,
        avg_price: float | None,
    ) -> None:
        """先按实际状态登记；已存在则做一次状态迁移。"""
        try:
            resp = await self._client.order_register(cid, symbol, state)
            if resp.get("duplicate"):
                # 已登记过：走合法迁移（如 ACKED → FILLED）
                await self._client.order_transition(cid, state, filled_qty, avg_price)
                self.counts["transitioned"] += 1
            else:
                self.counts["registered"] += 1
                # 首次建档时顺带补成交量/均价（状态已是终态也不能再迁移）
                if (filled_qty or 0) > 0 or (avg_price or 0) > 0:
                    try:
                        await self._client.order_transition(cid, state, filled_qty, avg_price)
                        self.counts["transitioned"] += 1
                    except ExecSidecarError:
                        # 同状态重复迁移被拒属正常（状态机幂等），忽略
                        pass
            self.last_error = ""
        except ExecSidecarError as exc:
            self._fail(str(exc))
        except Exception as exc:  # noqa: BLE001
            self._fail(f"{type(exc).__name__}: {exc}")

    def _fail(self, message: str) -> None:
        self.counts["errors"] += 1
        self.last_error = message[:200]
        if not getattr(self._client, "connected", False):
            self._down_until = time.monotonic() + DOWN_COOLDOWN

    # ------------------------------------------------------------------ 观测
    def status(self) -> dict[str, Any]:
        client = self._client
        return {
            "mode": self._mode_of(),
            "enabled": self.enabled(),
            "target": client.target,
            "connected": bool(client.connected),
            "counts": dict(self.counts),
            "last_error": self.last_error,
            "cooldown_ms": max(0, int((self._down_until - time.monotonic()) * 1000)),
        }


order_ledger = OrderLedger()
