"""币安权重 / 订单数双闸限频.

- 权重桶: 被动按 1 分钟窗口补充, 主动参考响应头 X-MBX-USED-WEIGHT-1M
- 订单桶: 10 秒 / 1 分钟双窗口, 对应交易所的 order rate limit
- 封禁熔断: 命中 -1003 后解析 "banned until <ms>", 到期前直接拒发
"""
from __future__ import annotations

import asyncio
import re
import time

from ..logging_setup import get_logger

log = get_logger("ratelimit")

_BAN_RE = re.compile(r"banned until (\d{13})")

#: 交易所返回的封禁时间可能长达数小时 (共享出口 IP 被邻居拖累时尤其长)。
#: 直接按字面值熔断会让整个面板"假死", 所以本地最多只认 3 分钟,
#: 到点先试探一次: 仍被封就再续 3 分钟, 已解封就立即恢复。
MAX_BAN_HONOR_SEC = 180.0
#: 令牌不足时的最大排队时间。超过就快速失败，避免请求在网关里堆到超时（雪崩）。
MAX_WAIT_SEC = 5.0


class RateLimited(RuntimeError):
    """本地限频熔断: 调用方应立即失败, 而不是挂起等封禁结束."""


class WeightLimiter:
    def __init__(self, capacity: int = 2400, window: float = 60.0, max_used_ratio: float = 0.85) -> None:
        self.capacity = capacity
        self.window = window
        self.max_used_ratio = max_used_ratio
        self._tokens = float(capacity)
        self._last = time.monotonic()
        self._used_header = 0
        self._ban_until = 0.0
        #: 交易所声明的真实解封时间（可能长达数小时），仅用于展示与提示，不按它熔断
        self._ban_declared_until = 0.0
        self._ban_logged_until = 0.0
        self._lock = asyncio.Lock()

    # ------------------------------------------------------------ internal
    def _refill(self) -> None:
        now = time.monotonic()
        elapsed = now - self._last
        if elapsed <= 0:
            return
        self._tokens = min(self.capacity, self._tokens + self.capacity * elapsed / self.window)
        self._last = now

    @property
    def banned(self) -> bool:
        return time.time() < self._ban_until

    def ban_remaining(self) -> float:
        return max(0.0, self._ban_until - time.time())

    @property
    def ban_declared_until(self) -> float:
        """交易所声明的解封时间戳；0 表示没有长封禁信息."""
        return self._ban_declared_until

    def ban_declared_remaining(self) -> float:
        return max(0.0, self._ban_declared_until - time.time())

    def observe_ban(self, message: str) -> bool:
        m = _BAN_RE.search(message or "")
        if not m:
            return False
        declared = int(m.group(1)) / 1000.0
        self._ban_declared_until = max(self._ban_declared_until, declared)
        self._ban_until = max(self._ban_until, min(declared, time.time() + MAX_BAN_HONOR_SEC))
        # 同一轮封禁每 3 分钟会试探一次，声明时间不变时不要刷 ERROR
        if declared > self._ban_logged_until + 1:
            self._ban_logged_until = declared
            log.warning(
                "币安返回封禁 (声明至 %s), 本地每 %.0f 分钟试探一次, 下次 %s",
                time.strftime("%H:%M:%S", time.localtime(declared)),
                MAX_BAN_HONOR_SEC / 60,
                time.strftime("%H:%M:%S", time.localtime(self._ban_until)),
            )
        return True

    def observe_weight(self, used: int) -> None:
        if used <= 0:
            return
        self._used_header = used
        # 交易所口径已用权重直接压缩本地令牌, 避免双算
        remaining_ratio = max(0.0, 1.0 - used / self.capacity)
        self._tokens = min(self._tokens, self.capacity * remaining_ratio)

    @property
    def used_weight(self) -> int:
        return self._used_header

    async def acquire(self, weight: int = 1) -> None:
        started = time.monotonic()
        while True:
            async with self._lock:
                self._refill()
                if self.banned:
                    # 熔断期内直接失败: 挂起会让 HTTP 请求超时, 面板看起来像卡死
                    declared_left = self.ban_declared_remaining()
                    if declared_left > MAX_BAN_HONOR_SEC:
                        when = time.strftime("%H:%M", time.localtime(self._ban_declared_until))
                        raise RateLimited(
                            f"币安封禁至 {when}（还需约 {int(declared_left // 60)} 分钟）；"
                            f"本地 {int(self.ban_remaining())}s 后试探，并非解封时间"
                        )
                    raise RateLimited(f"币安限频冷却中, 剩余 {int(self.ban_remaining())}s")
                if self._tokens >= weight:
                    self._tokens -= weight
                    return
                wait = (weight - self._tokens) * self.window / self.capacity
            if time.monotonic() - started >= MAX_WAIT_SEC:
                raise RateLimited(
                    f"本地权重预算不足 (近1分钟已用 {self._used_header}/{self.capacity}), 请稍后重试"
                )
            await asyncio.sleep(min(2.0, max(0.03, wait)))


class OrderRateLimiter:
    def __init__(self, per_10s: int = 250, per_minute: int = 1000) -> None:
        self.per_10s = per_10s
        self.per_minute = per_minute
        self._events: list[float] = []
        self._lock = asyncio.Lock()

    async def acquire(self) -> None:
        async with self._lock:
            now = time.monotonic()
            self._events = [t for t in self._events if now - t < 60]
            in_10s = sum(1 for t in self._events if now - t < 10)
            if in_10s >= self.per_10s:
                wait = 10 - (now - self._events[-self.per_10s])
                await asyncio.sleep(max(0.05, wait))
            elif len(self._events) >= self.per_minute:
                wait = 60 - (now - self._events[-self.per_minute])
                await asyncio.sleep(max(0.05, wait))
            self._events.append(time.monotonic())
