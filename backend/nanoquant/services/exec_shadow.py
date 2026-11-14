"""执行内核影子模式（P2）：把本地签名送一份给 sidecar，逐字节比对.

三种模式（`system.exec_mode`）：

| 模式 | 行为 | 适用 |
|---|---|---|
| `off` | 只用 Python 本地签名，不连 sidecar | 默认 |
| `shadow` | 每次签名**后台**送一份给 sidecar 比对，只统计，不影响下单 | 灰度观察 |
| `enforce` | 下单前**同步**比对，不一致立刻拒绝下单（fail closed） | 切流前最后一道闸 |

设计取舍：
- `shadow` 必须零延迟影响，所以比对是 fire-and-forget（并发上限内），代价只是多一个
  本地任务；p99 已经被网络 RTT 主导，不能为了观测再往关键路径塞一次跨进程往返。
- `enforce` 会多一次本地往返（UDS 通常 < 1ms），换来"不一致就绝不下单"。
  这里**只对不一致 fail-closed**；sidecar 本身不可用时按协议降级回 Python 签名，
  避免 sidecar 挂掉连带停止交易。
- sidecar 只拿到 `query` 与签名结果，不接触 Key/Secret 以外的任何账户信息。
"""
from __future__ import annotations

import asyncio
import random
import time
from collections import deque
from dataclasses import dataclass
from typing import Any, Callable

from ..exchange.exec_client import ExecSidecar, ExecSidecarError
from ..logging_setup import get_logger

log = get_logger("exec_shadow")

#: 影子模式下同时最多有多少个比对任务（防止下单高峰把任务表撑爆）
MAX_INFLIGHT = 32
#: 保留的最近不一致样本数
MISMATCH_SAMPLES = 5
#: 配置读取缓存（避免每次签名都做一次 Pydantic 校验）
CFG_TTL = 2.0
#: 不一致事件告警节流
ALERT_INTERVAL = 10.0
#: sidecar 掉线后的静默期: 期间不再投递比对, 避免每个签名都等一次超时
DOWN_COOLDOWN = 5.0


class ExecShadowError(RuntimeError):
    """enforce 模式下签名不一致：必须拒绝下单."""


@dataclass(frozen=True)
class ShadowConfig:
    mode: str = "off"
    target: str = ""
    sample_pct: int = 100
    timeout_ms: int = 500


def _default_config() -> ShadowConfig:
    """读配置（延迟 import，避免模块加载就连数据库）."""
    from .settings import get_system

    cfg = get_system()
    return ShadowConfig(
        mode=str(getattr(cfg, "exec_mode", "off")),
        target=str(getattr(cfg, "exec_socket", "") or ""),
        sample_pct=int(getattr(cfg, "exec_sample_pct", 100)),
        timeout_ms=int(getattr(cfg, "exec_timeout_ms", 500)),
    )


def _percentile(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    pos = (len(values) - 1) * q
    low = int(pos)
    high = min(low + 1, len(values) - 1)
    return values[low] * (1 - (pos - low)) + values[high] * (pos - low)


def _default_notifier(message: str, payload: dict[str, Any] | None) -> None:
    """签名不一致属于安全问题: 走事件总线 (error 级 → 值班群告警)."""
    from . import events

    events.record("error", "exec", message, payload)


class ExecShadow:
    def __init__(
        self,
        client: ExecSidecar | None = None,
        config_provider: Callable[[], ShadowConfig] | None = None,
        notifier: Callable[[str, dict[str, Any] | None], None] | None = None,
    ) -> None:
        self._client = client or ExecSidecar()
        self._provider = config_provider or _default_config
        self._notifier = notifier or _default_notifier
        self._cfg: ShadowConfig | None = None
        self._cfg_ts = 0.0
        self._lat: deque[float] = deque(maxlen=1000)
        self._tasks: set[asyncio.Task] = set()
        self._last_alert = 0.0
        self._down_until = 0.0
        self.counts = {"matched": 0, "mismatched": 0, "errors": 0, "skipped": 0, "inflight": 0}
        self.last_error = ""
        self.last_mismatch: dict[str, Any] | None = None

    # ----------------------------------------------------------------- 配置
    def config(self) -> ShadowConfig:
        now = time.monotonic()
        if self._cfg is not None and now - self._cfg_ts < CFG_TTL:
            return self._cfg
        try:
            cfg = self._provider()
        except Exception:  # noqa: BLE001  配置读不到时按关闭处理，绝不阻塞交易
            cfg = ShadowConfig()
        timeout = max(0.05, cfg.timeout_ms / 1000.0)
        if self._client.timeout != timeout:
            self._client.timeout = timeout
        # 配置留空时回落到 EXEC_SOCKET 环境变量 (reconfigure 内部按空串处理)
        self._client.reconfigure(cfg.target)
        self._cfg, self._cfg_ts = cfg, now
        return cfg

    def reset(self) -> None:
        self.counts = {"matched": 0, "mismatched": 0, "errors": 0, "skipped": 0, "inflight": 0}
        self._lat.clear()
        self._down_until = 0.0
        self.last_error = ""
        self.last_mismatch = None

    # ------------------------------------------------------------- 关键路径
    async def observe(self, query: str, signature: str) -> None:
        """在 `client._request` 签名之后调用.

        - shadow：只投递后台任务，立即返回；
        - enforce：同步比对，不一致抛 `ExecShadowError`（调用方必须拒绝下单）。
        """
        cfg = self.config()
        if cfg.mode not in {"shadow", "enforce"}:
            return
        if cfg.sample_pct < 100 and random.randrange(100) >= cfg.sample_pct:
            self.counts["skipped"] += 1
            return
        if time.monotonic() < self._down_until:
            # 掉线静默期: 继续比对只会让每个签名都付一次超时, 直接按降级处理
            self.counts["skipped"] += 1
            return
        if cfg.mode == "enforce":
            await self._compare(query, signature, strict=True)
            return
        if self.counts["inflight"] >= MAX_INFLIGHT:
            self.counts["skipped"] += 1
            return
        try:
            task = asyncio.get_running_loop().create_task(self._compare(query, signature))
        except RuntimeError:  # 没有事件循环（写库线程等）→ 放弃比对
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
        except ExecShadowError:
            pass
        except Exception:  # noqa: BLE001
            # 比对任务异常必须留痕: 影子模式的结论就是靠这些统计得出的
            log.warning("影子比对任务异常退出", exc_info=True)

    async def _compare(self, query: str, signature: str, *, strict: bool = False) -> bool:
        started = time.perf_counter()
        try:
            resp = await self._client.sign(query)
        except ExecSidecarError as exc:
            self.counts["errors"] += 1
            self.last_error = str(exc)
            if not getattr(self._client, "connected", False):
                self._down_until = time.monotonic() + DOWN_COOLDOWN
            # sidecar 不可用 → 降级回 Python 签名（协议第 7 节），不拦单
            return False
        finally:
            # 优先用客户端在锁内测得的往返 (不含排队), 否则退回墙钟
            elapsed = getattr(self._client, "last_ms", 0.0)
            self._lat.append(float(elapsed) if elapsed else (time.perf_counter() - started) * 1000)

        remote = str(resp.get("signature") or "")
        echoed = resp.get("query")
        if remote == signature and (echoed is None or echoed == query):
            self.counts["matched"] += 1
            self.last_error = ""
            return True

        self.counts["mismatched"] += 1
        self.last_mismatch = {
            "ts": int(time.time()),
            "algo": resp.get("algo") or self._client.algo,
            "query": query[:300],
            "local": signature[:120],
            "remote": remote[:120],
            "echo_ok": echoed is None or echoed == query,
        }
        action = "已拒绝下单" if strict else "shadow 模式仅记录，不拦单"
        self._notify(f"sidecar 签名不一致（algo={self.last_mismatch['algo']}），{action}")
        if strict:
            raise ExecShadowError("sidecar 签名与本地不一致")
        return False

    def _notify(self, message: str) -> None:
        now = time.monotonic()
        if now - self._last_alert < ALERT_INTERVAL:
            return
        self._last_alert = now
        try:
            self._notifier(message, self.last_mismatch)
        except Exception:  # noqa: BLE001
            # 通知器坏了不该影响下单, 但"签名不一致却没人收到通知"必须记录下来
            log.warning("影子比对通知推送失败", exc_info=True)

    # ----------------------------------------------------------------- 观测
    def status(self) -> dict[str, Any]:
        cfg = self.config()
        lat = sorted(self._lat)
        client = self._client
        out: dict[str, Any] = {
            "mode": cfg.mode,
            "target": client.target,
            "connected": bool(client.connected),
            "algo": client.algo,
            "sample_pct": cfg.sample_pct,
            "timeout_ms": cfg.timeout_ms,
            "counts": dict(self.counts),
            "latency_ms": {
                "p50": round(_percentile(lat, 0.50), 3),
                "p95": round(_percentile(lat, 0.95), 3),
                "p99": round(_percentile(lat, 0.99), 3),
                "max": round(lat[-1], 3) if lat else 0.0,
            },
            "sidecar": {"requests": client.counters["requests"], "errors": client.counters["errors"]},
            "last_error": self.last_error or client.last_error,
            "last_mismatch": self.last_mismatch,
            "cooldown_ms": max(0, int((self._down_until - time.monotonic()) * 1000)),
        }
        out["verdict"] = self._verdict(out)
        return out

    @staticmethod
    def _verdict(data: dict[str, Any]) -> str:
        mode = data["mode"]
        counts = data["counts"]
        if mode == "off":
            return "未启用：使用 Python 本地签名"
        if counts["mismatched"]:
            return "签名不一致：禁止切流，先对比两侧密钥与归一化逻辑"
        if not data["connected"] and counts["errors"]:
            # 历史一致过不代表现在通: 切流看的是"当前能否比对成功"
            return (
                f"sidecar 不可用（{(data['last_error'] or '连接失败')[:60]}），已降级为 Python 签名；"
                f"历史一致 {counts['matched']} 次"
            )
        if counts["matched"]:
            extra = "；旁路比对不拦单" if mode == "shadow" else "；不一致将拒绝下单"
            return f"签名逐字节一致（{counts['matched']} 次）{extra}"
        return "已启用但尚未产生比对样本"

    async def self_test(self, secret: str) -> dict[str, Any]:
        """用一段固定 query 自检：health + 与本地签名比对.

        不经过 `client._request`，因此不消耗权重、不下单。
        """
        from ..exchange.client import sign_query

        query = "symbol=BTCUSDT&side=BUY&type=MARKET&timestamp=1700000000000"
        result: dict[str, Any] = {"query": query, "target": self._client.target}
        try:
            health = await self._client.health()
            result["health"] = {"ok": True, "algo": health.get("algo")}
        except ExecSidecarError as exc:
            result.update({"ok": False, "detail": f"sidecar 不可用: {exc}"})
            return result
        if not secret:
            result.update({"ok": False, "detail": "当前交易环境没有配置 API Secret，无法比对签名"})
            return result
        local = sign_query(query, secret)
        started = time.perf_counter()
        try:
            resp = await self._client.sign(query)
        except ExecSidecarError as exc:
            result.update({"ok": False, "detail": f"签名失败: {exc}"})
            return result
        elapsed = (time.perf_counter() - started) * 1000
        remote = str(resp.get("signature") or "")
        ok = remote == local
        if ok:
            self.counts["matched"] += 1
        else:
            self.counts["mismatched"] += 1
            self.last_mismatch = {
                "ts": int(time.time()),
                "algo": resp.get("algo") or self._client.algo,
                "query": query,
                "local": local[:120],
                "remote": remote[:120],
                "echo_ok": resp.get("query") == query,
            }
            self._notify("sidecar 签名不一致（自检）")
        result.update(
            {
                "ok": ok,
                "algo": resp.get("algo"),
                "compare_ms": round(elapsed, 2),
                "local_len": len(local),
                "remote_len": len(remote),
                "detail": "签名逐字节一致" if ok else "签名不一致：两侧密钥可能不是同一把",
            }
        )
        return result


exec_shadow = ExecShadow()
