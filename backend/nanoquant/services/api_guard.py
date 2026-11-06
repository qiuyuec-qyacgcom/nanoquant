"""API 请求限频（进程内滑动窗口，按客户端 IP）.

不落库、不进事件总线：限频本身必须在最外层、尽可能便宜。
单进程部署足够用；多实例部署时应在反向代理侧再做一层。
"""
from __future__ import annotations

import threading
import time
from collections import defaultdict, deque

WINDOW_SEC = 60.0
_MAX_KEYS = 4096


class SlidingWindowLimiter:
    def __init__(self) -> None:
        self._hits: dict[str, deque[float]] = defaultdict(deque)
        self._lock = threading.Lock()
        self._checks = 0

    def check(self, key: str, limit: int) -> tuple[bool, int]:
        """返回 (是否放行, 建议重试秒数)。limit<=0 时永远放行。"""
        if limit <= 0:
            return True, 0
        now = time.monotonic()
        with self._lock:
            q = self._hits[key]
            while q and now - q[0] >= WINDOW_SEC:
                q.popleft()
            if len(q) >= limit:
                retry = max(1, int(WINDOW_SEC - (now - q[0])) + 1)
                return False, retry
            q.append(now)

            # 偶发清理，避免扫描器把 key 撑爆
            self._checks += 1
            if self._checks % 512 == 0 and len(self._hits) > _MAX_KEYS:
                stale = [k for k, v in self._hits.items() if not v or now - v[-1] >= WINDOW_SEC]
                for k in stale:
                    self._hits.pop(k, None)
            return True, 0

    def clear(self) -> None:
        with self._lock:
            self._hits.clear()


limiter = SlidingWindowLimiter()
