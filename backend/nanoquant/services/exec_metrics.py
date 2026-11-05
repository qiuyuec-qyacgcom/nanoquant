"""下单关键路径的分段耗时 (内存环形缓冲).

为什么落内存而不是落库:
- p99 是滚动指标，历史值参考意义有限；
- 把 `UPDATE orders SET stages=...` 放到下单关键路径上，等于为了观测而增加一次写库，
  反而把 p99 抬高。这里只保留最近 N 笔，接口实时读，零热路径 I/O。

分段口径（从 broker 视角，单位毫秒）:
- plan_ms    组装参数、精度对齐、点差校验等本地计算
- wait_ms    权重桶 / 订单数桶排队
- sign_ms    时间戳 + urlencode + HMAC-SHA256 签名
- http_ms    实际 HTTP 往返（含 TLS 复用）
- persist_ms 写 orders/positions/trades 的 SQLite 提交
- protect_ms 挂止损/止盈保护单
- total_ms   整个 open/close 调用
"""
from __future__ import annotations

import threading
import time
from collections import deque
from typing import Any

CAPACITY = 2000
STAGES = ("plan_ms", "wait_ms", "sign_ms", "http_ms", "persist_ms", "protect_ms", "total_ms")


class ExecMetrics:
    def __init__(self, capacity: int = CAPACITY) -> None:
        self._buf: dict[str, deque[float]] = {k: deque(maxlen=capacity) for k in STAGES}
        self._lock = threading.Lock()
        self._last_ts = 0.0

    def record(self, stages: dict[str, float]) -> None:
        with self._lock:
            for key, value in stages.items():
                buf = self._buf.get(key)
                if buf is None:
                    continue
                try:
                    number = float(value)
                except (TypeError, ValueError):
                    continue
                if number != number or number < 0:  # NaN / 负值防御
                    continue
                buf.append(number)
            self._last_ts = time.time()

    @staticmethod
    def _percentile(sorted_vals: list[float], q: float) -> float:
        if not sorted_vals:
            return 0.0
        pos = (len(sorted_vals) - 1) * q
        low = int(pos)
        high = min(low + 1, len(sorted_vals) - 1)
        weight = pos - low
        return sorted_vals[low] * (1 - weight) + sorted_vals[high] * weight

    def summary(self) -> dict[str, Any]:
        with self._lock:
            snap = {key: sorted(buf) for key, buf in self._buf.items()}
            last_ts = self._last_ts
        count = len(snap.get("total_ms", []))
        result: dict[str, Any] = {"count": count, "updated_at": int(last_ts)}
        for key, values in snap.items():
            result[key] = {
                "p50": round(self._percentile(values, 0.50), 2),
                "p95": round(self._percentile(values, 0.95), 2),
                "p99": round(self._percentile(values, 0.99), 2),
                "max": round(values[-1], 2) if values else 0.0,
            }
        return result

    def reset(self) -> None:
        with self._lock:
            for buf in self._buf.values():
                buf.clear()
            self._last_ts = 0.0


exec_metrics = ExecMetrics()
