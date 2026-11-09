"""本轮开仓的保证金账本（进程内，按 cycle 重置）.

为什么需要：`_scan` 会并发 spawn 多个 `_open_entry`（Semaphore=2），每个任务都拿
cycle 开始时的同一份 `account.available` 计算仓位。DOGE 和 ARB 同一秒各算出 2720U
保证金，DOGE 成交后 ARB 就被交易所拒绝（-2019）。账本把「计算 + 预留」串行化，
让后一个任务只能用前一个任务剩下的额度。

约定：
- `begin_cycle(available)` 每轮用最新账户快照重置预算与占用；
- 预留在下单失败时 `release`，成交后**不释放**（交易所已实际占用，
  等下一轮 `begin_cycle` 用新快照覆盖）；
- 预算本身不打折：`estimate_qty` 已按 `MARGIN_SAFETY=0.9` 留过安全垫。
"""
from __future__ import annotations

import asyncio


class MarginLedger:
    def __init__(self) -> None:
        self._budget = 0.0
        self._used = 0.0
        self._gen = 0
        self._lock = asyncio.Lock()

    def begin_cycle(self, available: float | None) -> None:
        self._budget = max(0.0, float(available or 0.0))
        self._used = 0.0
        self._gen += 1

    @property
    def budget(self) -> float:
        return self._budget

    @property
    def used(self) -> float:
        return self._used

    @property
    def remaining(self) -> float:
        return max(0.0, self._budget - self._used)

    async def reserve(self, amount: float) -> int:
        """串行占额度；返回本次预留所属的代数，0 表示本轮剩余额度不够."""
        if amount <= 0:
            return 0
        async with self._lock:
            if self._used + amount > self._budget + 1e-9:
                return 0
            self._used += amount
            return self._gen

    async def release(self, amount: float, gen: int = 0) -> None:
        """归还额度；跨轮的任务（gen 不匹配）不能动新一轮的预算."""
        if gen and gen != self._gen:
            return
        async with self._lock:
            self._used = max(0.0, self._used - max(0.0, float(amount or 0.0)))
