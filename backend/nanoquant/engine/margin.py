"""开仓保证金账本（进程内，轮询开仓与信号跟单共用）.

为什么需要：`_scan` 会并发 spawn 多个 `_open_entry`（Semaphore=2），每个任务都拿
cycle 开始时的同一份 `account.available` 计算仓位。DOGE 和 ARB 同一秒各算出 2720U
保证金，DOGE 成交后 ARB 就被交易所拒绝（-2019）。账本把「计算 + 预留」串行化。

与轮询开仓不冲突的关键：信号跟单不走 15s 的 cycle，它可能在引擎两轮之间下单。
`begin_cycle` 只清「已成交、交易所快照里已经扣掉」的占用，**在途预留必须跨轮保留**，
否则下一轮重置额度后两条链路会同时按满额发单。`reserve_tracked` / `settle` 就是为此:

    rid = await ledger.reserve_tracked(margin)   # 下单前占额度（跨轮保留）
    ... await broker.open_position(...)
    await ledger.settle(rid, opened=res.ok)      # 成交→转为已占用; 失败→归还

约定：
- `begin_cycle(available)` 每轮用最新账户快照重置预算与已占用；
- `refresh(available)` 用更新的快照改预算（快照已反映已成交单，故清掉已占用）；
- 在途预留（_pending）任何情况下都保留，直到 `settle`；
- 预算本身不打折：`estimate_qty` 已按 `MARGIN_SAFETY=0.9` 留过安全垫。

`reserve/release` 保留旧的「按代数」接口（契约测试与兼容调用方仍可用）。
"""
from __future__ import annotations

import asyncio


class MarginLedger:
    def __init__(self) -> None:
        self._budget = 0.0
        self._used = 0.0
        self._pending: dict[int, float] = {}
        self._gen = 0
        self._token = 0
        self._lock = asyncio.Lock()

    def begin_cycle(self, available: float | None) -> None:
        self._budget = max(0.0, float(available or 0.0))
        self._used = 0.0
        self._gen += 1
        # _pending 不动：跨 cycle 的在途订单仍然占额度

    def refresh(self, available: float | None) -> None:
        """用最新可用余额更新预算（交易所快照已扣除已成交单）.

        信号跟单可能在引擎两轮之间到达，不能等下一轮 begin_cycle；在途预留
        (_pending) 继续保留，避免把还没成交的订单额度重复放出去。
        """
        self._budget = max(0.0, float(available or 0.0))
        self._used = 0.0

    @property
    def _pending_total(self) -> float:
        return sum(self._pending.values())

    @property
    def budget(self) -> float:
        return self._budget

    @property
    def used(self) -> float:
        return self._used + self._pending_total

    @property
    def inflight(self) -> float:
        """尚未确认成交的在途预留金额（信号跟单期间会 > 0）."""
        return self._pending_total

    @property
    def remaining(self) -> float:
        return max(0.0, self._budget - self.used)

    async def reserve(self, amount: float) -> int:
        """旧接口：串行占额度；返回本次预留所属的代数，0 表示本轮剩余额度不够."""
        value = float(amount or 0.0)
        if value <= 0:
            return 0
        async with self._lock:
            if self.used + value > self._budget + 1e-9:
                return 0
            self._used += value
            return self._gen

    async def release(self, amount: float, gen: int = 0) -> None:
        """旧接口：归还额度；跨轮的任务（gen 不匹配）不能动新一轮的预算."""
        if gen and gen != self._gen:
            return
        async with self._lock:
            self._used = max(0.0, self._used - max(0.0, float(amount or 0.0)))

    async def reserve_tracked(self, amount: float) -> int:
        """占额度并返回预留凭据；0=额度不足。凭据在 settle 前跨 begin_cycle 有效."""
        value = float(amount or 0.0)
        if value <= 0:
            return 0
        async with self._lock:
            if self.used + value > self._budget + 1e-9:
                return 0
            self._token += 1
            self._pending[self._token] = value
            return self._token

    async def settle(self, rid: int, *, opened: bool) -> None:
        """下单结束：成交→转为已占用(下一轮快照覆盖)，失败/演练→直接归还."""
        async with self._lock:
            value = self._pending.pop(int(rid or 0), 0.0)
            if value <= 0:
                return
            if opened:
                self._used += value
