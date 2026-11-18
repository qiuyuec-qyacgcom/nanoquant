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
        #: P3-3：保护单构造对账（sidecar 并行构造 vs Python 本地构造）
        self.protect_counts = {
            "matched": 0,
            "mismatched": 0,
            "errors": 0,
            "skipped": 0,
            # P3-3 收尾：sidecar 真代发的统计
            "placed": 0,
            "place_failed": 0,
            "place_unknown": 0,
        }
        self.last_protect_mismatch: dict[str, Any] | None = None

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

    def place_enabled(self) -> bool:
        """P3-3 收尾：是否由 sidecar 代发保护单。

        双重闸门：Python 侧 `system.exec_place_protection=True`
        **且** sidecar 侧 `EXEC_PLACE_MODE=enabled`（由 sidecar 拒绝兜底）。
        另需 sidecar 处于 shadow/enforce（即已接入）。
        """
        if not self.enabled():
            return False
        try:
            from .settings import get_system

            return bool(getattr(get_system(), "exec_place_protection", False))
        except Exception:  # noqa: BLE001  读不到配置一律按关闭处理
            return False

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

    async def place_protection(
        self,
        *,
        symbol: str,
        close_side: str,
        stop_price: str,
        take_price: str,
        api_key: str,
        position_side: str = "",
        stop_client_id: str = "",
        take_client_id: str = "",
    ) -> dict[str, Any]:
        """P3-3 收尾：让 sidecar 代发保护单。

        **同步调用**（不是 fire-and-forget）：调用方需要明确的成功/未知结果来决定
        是否回退 Python 发单。任何异常都转成 `{"ok": False, "unknown": False}`，
        让调用方安全回退；只有 `unknown=True` 时禁止回退。
        """
        if not self.enabled():
            return {"ok": False, "unknown": False, "reason": "ledger_disabled"}
        if time.monotonic() < self._down_until:
            return {"ok": False, "unknown": False, "reason": "cooldown"}
        try:
            res = await self._client.protection_place(
                symbol=symbol,
                close_side=close_side,
                stop_price=stop_price,
                take_price=take_price,
                api_key=api_key,
                position_side=position_side,
                stop_client_id=stop_client_id,
                take_client_id=take_client_id,
            )
            if res.get("unknown"):
                self.protect_counts["place_unknown"] += 1
                self.last_error = "sidecar 代发结果未知"
            elif res.get("ok"):
                self.protect_counts["placed"] += 1
                self.last_error = ""
            else:
                self.protect_counts["place_failed"] += 1
            return res
        except ExecSidecarError as exc:
            self.protect_counts["place_failed"] += 1
            self.last_error = str(exc)[:200]
            if not getattr(self._client, "connected", False):
                self._down_until = time.monotonic() + DOWN_COOLDOWN
            # sidecar 明确不可用（连接失败/超时）→ 允许回退 Python 发单
            return {"ok": False, "unknown": False, "reason": self.last_error}
        except Exception as exc:  # noqa: BLE001
            self.protect_counts["place_failed"] += 1
            self.last_error = f"{type(exc).__name__}: {exc}"[:200]
            return {"ok": False, "unknown": False, "reason": self.last_error}

    def authoritative_enabled(self) -> bool:
        """P3-5 收尾：是否把 sidecar 台账当读取权威源（默认关）。"""
        if not self.enabled():
            return False
        try:
            from .settings import get_system

            return bool(getattr(get_system(), "exec_ledger_authoritative", False))
        except Exception:  # noqa: BLE001
            return False

    async def authoritative_orders(self, limit: int = 500) -> dict[str, dict[str, Any]] | None:
        """取 sidecar 台账作为权威视图；不可用时返回 None（调用方回退本地表）。

        只返回状态类字段（state / filled_qty / avg_price），其余元数据仍以本地为准。
        """
        if not self.authoritative_enabled():
            return None
        try:
            snap = await self._client.order_snapshot()
        except ExecSidecarError:
            return None
        except Exception:  # noqa: BLE001
            return None
        orders = (snap.get("order") or {}).get("orders") or []
        out: dict[str, dict[str, Any]] = {}
        for item in orders[:limit]:
            if not isinstance(item, dict):
                continue
            cid = str(item.get("client_order_id") or "")
            if not cid:
                continue
            out[cid] = {
                "state": str(item.get("state") or "").upper(),
                "filled_qty": float(item.get("filled_qty") or 0),
                "avg_price": float(item.get("avg_price") or 0),
            }
        return out

    async def reconcile(self, *, lookback_sec: int = 24 * 3600, limit: int = 500) -> dict[str, Any]:
        """P3-5 收尾的**前置**：sidecar 台账 ↔ Python `orders` 表双向对账（只读）。

        为什么要先做对账而不是直接移交写权：
        只有先量化「两个台账到底差多少」，才能判断能否把权威源交给 sidecar。
        本方法**不改任何数据**，只比对并分类差异：

        | 差异类型 | 含义 |
        |---|---|
        | `missing_in_sidecar` | 本地有、sidecar 没有（投递丢失/未开启时下的单） |
        | `missing_in_local` | sidecar 有、本地没有（本地写库失败/死信） |
        | `state_mismatch` | 两侧都有但状态不同（迁移丢失或异步未达） |
        | `qty_mismatch` | 成交量不同 |

        返回统计 + 最多 50 条样本，供面板/人工判断。
        """
        import time as _time

        from ..db import session_scope
        from ..models import OrderRecord as LocalOrder

        since = int(_time.time()) - lookback_sec
        with session_scope() as s:
            rows = (
                s.query(LocalOrder)
                .filter(LocalOrder.ts >= since)
                .order_by(LocalOrder.ts.desc())
                .limit(limit)
                .all()
            )
            local: dict[str, dict[str, Any]] = {
                str(r.client_order_id or ""): {
                    "symbol": r.symbol,
                    "status": str(r.status or "").strip().upper(),
                    "qty": float(r.qty or 0),
                }
                for r in rows
                if str(r.client_order_id or "")
            }

        try:
            snap = await self._client.order_snapshot()
        except ExecSidecarError as exc:
            return {"ok": False, "reason": str(exc)[:200], "checked": 0}
        except Exception as exc:  # noqa: BLE001
            return {"ok": False, "reason": f"{type(exc).__name__}: {exc}"[:200], "checked": 0}

        orders = (snap.get("order") or {}).get("orders") or []
        remote: dict[str, dict[str, Any]] = {}
        for item in orders:
            if not isinstance(item, dict):
                continue
            cid = str(item.get("client_order_id") or "")
            if cid:
                remote[cid] = item

        missing_in_sidecar = [c for c in local if c not in remote]
        missing_in_local = [c for c in remote if c not in local]
        state_mismatch: list[dict[str, Any]] = []
        qty_mismatch: list[dict[str, Any]] = []
        for cid in local.keys() & remote.keys():
            ls = local[cid]["status"]
            rs = str(remote[cid].get("state") or "").upper()
            # 终态集合不同表述（CANCELLED/CANCELED）不算差异
            norm = lambda x: "CANCELED" if x in {"CANCELED", "CANCELLED"} else x  # noqa: E731
            if norm(ls) != norm(rs):
                state_mismatch.append({"client_order_id": cid, "local": ls, "sidecar": rs})
            lq = local[cid]["qty"]
            rq = float(remote[cid].get("filled_qty") or 0)
            if abs(lq - rq) > 1e-9:
                qty_mismatch.append({"client_order_id": cid, "local": lq, "sidecar": rq})

        diffs = (
            len(missing_in_sidecar)
            + len(missing_in_local)
            + len(state_mismatch)
            + len(qty_mismatch)
        )
        return {
            "ok": True,
            "checked": len(local),
            "sidecar_total": len(remote),
            "diff_count": diffs,
            "aligned": diffs == 0 and len(local) > 0,
            "missing_in_sidecar": missing_in_sidecar[:50],
            "missing_in_local": missing_in_local[:50],
            "state_mismatch": state_mismatch[:50],
            "qty_mismatch": qty_mismatch[:50],
        }

    def observe_protection(
        self,
        symbol: str,
        close_side: str,
        legs: list[tuple[str, str, str]],
        *,
        position_side: str = "",
        stop_client_id: str = "",
        take_client_id: str = "",
    ) -> None:
        """P3-3：让 sidecar 并行构造保护单，与 Python 本地构造逐字节对账。

        `legs` 是 Python 侧 `[(kind, trigger_price, query), ...]`；**只比对不代发**。
        对账失败只计数，绝不影响真实保护单下发。
        """
        if not symbol or not close_side or not legs:
            return
        if not self.enabled():
            return
        if time.monotonic() < self._down_until:
            self.protect_counts["skipped"] += 1
            return
        self._spawn(
            self._do_protection(
                symbol, close_side, legs, position_side, stop_client_id, take_client_id
            )
        )

    async def _do_protection(
        self,
        symbol: str,
        close_side: str,
        legs: list[tuple[str, str, str]],
        position_side: str,
        stop_client_id: str,
        take_client_id: str,
    ) -> None:
        try:
            by_kind = {kind: (px, q) for kind, px, q in legs}
            stop = by_kind.get("STOP_MARKET")
            take = by_kind.get("TAKE_PROFIT_MARKET")
            if not stop and not take:
                self.protect_counts["skipped"] += 1
                return
            remote = await self._client.protection_plan(
                symbol=symbol,
                close_side=close_side,
                stop_price=(stop or ("", ""))[0],
                take_price=(take or ("", ""))[0],
                position_side=position_side,
                stop_client_id=stop_client_id,
                take_client_id=take_client_id,
            )
            rmap = {str(x.get("kind")): x for x in remote if isinstance(x, dict)}
            ok = True
            for kind, (_px, local_q) in by_kind.items():
                r = rmap.get(kind)
                if not r or r.get("query") != local_q:
                    ok = False
                    self.last_protect_mismatch = {
                        "ts": int(time.time()),
                        "symbol": symbol,
                        "kind": kind,
                        "local": local_q[:200],
                        "remote": str((r or {}).get("query"))[:200],
                    }
                    break
            if ok:
                self.protect_counts["matched"] += 1
                self.last_error = ""
            else:
                self.protect_counts["mismatched"] += 1
        except ExecSidecarError as exc:
            self.protect_counts["errors"] += 1
            if not getattr(self._client, "connected", False):
                self._down_until = time.monotonic() + DOWN_COOLDOWN
            self.last_error = str(exc)[:200]
        except Exception as exc:  # noqa: BLE001
            self.protect_counts["errors"] += 1
            self.last_error = f"{type(exc).__name__}: {exc}"[:200]

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
            "protect_counts": dict(self.protect_counts),
            # P3-5 收尾：是否把 sidecar 台账当读取权威源
            "authoritative": self.authoritative_enabled(),
            "place_enabled": self.place_enabled(),
            "last_protect_mismatch": self.last_protect_mismatch,
            "last_error": self.last_error,
            "cooldown_ms": max(0, int((self._down_until - time.monotonic()) * 1000)),
        }


order_ledger = OrderLedger()
