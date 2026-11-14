"""交易引擎主循环.

节奏拆分:
- 每轮必做: 同步账户 -> 管理在场仓位 (止损/止盈/移动/超时) -> 权益采样
- 每轮择优: 扫描标的 -> 量化信号 -> 风控闸门 -> 交给独立的 AI 评估任务
AI 评估在后台任务中执行 (带并发闸), 绝不阻塞仓位管理与止损监控.

单实例: 文件锁 + 心跳文件; 面板读心跳判断"到底有没有在跑".
"""
from __future__ import annotations

import asyncio
import hashlib
import os
import time
from typing import Any

from sqlalchemy.exc import IntegrityError

from ..ai.decision import decide
from ..db import session_scope
from ..exchange.broker import (
    Broker,
    get_broker,
    qty_from_margin_pct,
    record_external_position,
)
from ..exchange.client import BinanceError, NoCredentials
from ..exchange.client import client as binance_client
from ..logging_setup import get_logger
from ..models import EquityPoint, OrderRecord, PositionRecord, Signal, SignalFollow, TradeRecord
from ..news import service as news_service
from ..services import events, position_view, state
from ..services import order_sync
from ..services import signal_follow as signal_feed
from ..services import symbols as symbol_service
from ..services.settings import (
    EngineSettings,
    RiskSettings,
    StrategySettings,
    get_ai,
    get_data,
    get_engine,
    get_evolution,
    get_news,
    get_risk,
    get_signal_follow,
    get_strategy,
    normalize_mode,
    update_group,
)
from ..evolution import mantra as mantra_store
from ..evolution import review as evolution_review
from . import risk as risk_mod
from .margin import MarginLedger
from .strategy import (
    build_features,
    detect_exit,
    entry_regime,
    partial_close_qty,
    partial_tp_r,
    score_signal,
    strategy_fingerprint,
)
from . import indicators as ta

log = get_logger("engine")

MAX_TASKS = 2
#: 同一标的、同一方向/拒绝原因的 flat 信号在这个窗口内合并成一行 (repeats 计数)
SIGNAL_DEDUP_SEC = 60
#: AI 议会一致度下限：低于该值视为分歧，`ai.require_agreement` 打开时直接观望
AGREEMENT_MIN = 0.6
#: flat 信号（扫描到了但没有方向）只保留审计需要的字段。
#: 完整特征快照约 1.3KB，而 flat 占全部信号的 93%：一天两千多条 = 每天 3MB 纯冗余，
#: 两周就是 34MB，还连带每次去重命中都把整块 JSON 重写一遍（WAL 放大）。
FLAT_FEATURE_KEYS = (
    "score", "price", "source", "atr_pct", "vol_ratio", "rsi",
    "ema_gap_pct", "regime", "change_24h", "quote_volume",
)


def _flat_features(features: dict[str, Any]) -> dict[str, Any]:
    """flat 信号的精简特征：保留"为什么没开"需要的量，丢掉逐指标快照."""
    return {k: features[k] for k in FLAT_FEATURE_KEYS if k in features}


def ai_agreement_reject(require_agreement: bool, agreement: float) -> str:
    """AI 议会的分歧闸门：返回拒绝原因，空字符串表示放行.

    单席位模式下 agreement 恒为 1.0，不受影响；多席位时低于 ``AGREEMENT_MIN`` 视为吵成一团。
    """
    if not require_agreement:
        return ""
    if agreement >= AGREEMENT_MIN:
        return ""
    return f"AI 议会分歧: 一致度 {agreement:.2f} < {AGREEMENT_MIN:.2f}，按观望处理"


def _scan_bars() -> int:
    """引擎扫描取的 K 线条数，跟随「参数配置 → 数据源 → K 线长度」.

    下限 60：EMA21/RSI14/ATR14/布林标准分至少要这么多根才不至于开头全是空值。
    上限 500：再多只是白吃权重，策略用不到。
    """
    try:
        limit = int(get_data().kline_limit)
    except Exception:  # noqa: BLE001  配置读失败不该阻断扫描
        return 60
    return max(60, min(500, limit))


def _channel_of(parsed: dict[str, Any]) -> str:
    """从解析结果里提取信号来源（频道名），用于按信号源评估质量。"""
    raw = parsed.get("raw")
    if isinstance(raw, dict):
        for key in ("channel", "source", "group", "频道", "来源"):
            value = raw.get(key)
            if value:
                return str(value).strip()[:64]
    return ""


class TradingEngine:
    """引擎主体：一轮 cycle = 对账 → 管仓 → 体检 → 扫描 → 决策 → 开仓。

    这个类现在有 2400 行左右，按关注点分组，找代码时按下表跳（顺序即文件顺序）：

    生命周期    start / stop / _loop / _idle_tick / _heartbeat
    每轮主循环  cycle
    对账        _reconcile_live / _sync_orders_if_due / _flatten_on_halt
    持仓管理    _manage_positions / _close / protect_position / _algo_sl_tp
    复盘调度    _review_trade / _review_backfill_* / _maybe_global_review
    体检        _check_trade_mode / _check_liquidation_risk / _health_check
    扫描        _symbols / _scan（并发取数 + 串行落库）/ _record_signal
    开仓决策    _do_open（点差闸门 → AI 裁决 → 定仓 → 下单）/ _open_entry_guarded
    外部信号    _follow_signal_locked 起（约 700 行：去重/池检查/闸门/定仓/下单/
                加仓/减仓/移止损/平仓），与扫描-开仓主循环是两条独立入口
    面板数据    status / signal_follow_status

    注意：这些方法大量共享模块级绑定（`binance_client` / `symbol_service` /
    `risk_mod` 等），测试也是直接 patch `worker.<名字>`。拆成 mixin 会导致
    绑定出现两份、patch 只对其中一份生效（试过：31 个用例因此失败），
    所以刻意保持单文件；需要导览时看这张表。
    """

    def __init__(self) -> None:
        self._task: asyncio.Task | None = None
        self._stop = asyncio.Event()
        self._entry_tasks: dict[str, asyncio.Task] = {}
        self._signal_dedup: dict[tuple, int] = {}
        #: 上一轮各标的持仓量快照。OI 接口只给单点值，不自己存上一轮就算不出 ΔOI，
        #: 而 ΔOI 因子没有值时会以 0 分隐身（旧实现就是这样白占 12% 权重）
        self._oi_prev: dict[str, float] = {}
        #: 强平价告警去重（symbol+side -> 上次告警时间戳）
        self._last_liq_warn: dict[str, float] = {}
        #: 健康检查节流与 WS 告警去重
        self._last_health_check = 0.0
        self._last_halt_flatten = 0.0
        self._last_order_sync = 0.0
        self._last_ws_warn = 0.0
        self._margin = MarginLedger()
        self._sem = asyncio.Semaphore(MAX_TASKS)
        self._last_equity = 0.0
        self._last_account: dict[str, Any] = {}
        self._running = False
        self._lock_fd: int | None = None
        self._trades_since_review = 0
        self._last_news = 0.0
        self._last_cleanup = 0.0
        self._last_dlq = 0.0
        self._last_digest = 0.0
        self._last_review_backfill = 0.0
        self._reviewing: set[int] = set()
        self._ai_warn_ts = 0.0
        self._started_at = 0
        # ---- 外部信号跟单 ----
        self._follow_task: asyncio.Task | None = None
        self._follow_lock = asyncio.Lock()
        #: 同标的开仓在途标记: symbol -> 时间戳，引擎扫描与信号跟单互斥
        self._symbol_busy: dict[str, float] = {}
        self._follow_path = ""
        self._follow_cursor_key = ""
        self._follow_last_poll = 0.0
        self._follow_last_error = ""
        #: 频道链接(http/https)模式：首次接入只建去重索引、不回放历史
        self._follow_url_primed = False
        self._follow_url_primed_for = ""
        self._follow_url_seen: set[str] = set()
        #: 交易模式体检（双向持仓 + 逐仓）节流：5 分钟查一次，提示 1 小时一次
        self._trade_mode_ts = 0.0
        self._trade_mode_warn_ts = 0.0

    # ------------------------------------------------------------- lifecycle
    def _acquire_lock(self) -> bool:
        try:
            if self._lock_fd is not None:
                return True
            import msvcrt  # Windows

            fd = os.open(str(state_file()), os.O_CREAT | os.O_RDWR)
            try:
                msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
            except OSError:
                os.close(fd)
                return False
            self._lock_fd = fd
            return True
        except ImportError:  # POSIX
            import fcntl

            fd = os.open(str(state_file()), os.O_CREAT | os.O_RDWR)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError:
                os.close(fd)
                return False
            self._lock_fd = fd
            return True

    def _release_lock(self) -> None:
        if self._lock_fd is None:
            return
        try:
            os.close(self._lock_fd)
        except OSError:
            pass
        self._lock_fd = None

    @property
    def running(self) -> bool:
        return bool(self._task and not self._task.done())

    async def start(self) -> bool:
        if self.running:
            return True
        if not self._acquire_lock():
            events.record("warn", "engine", "引擎已在其它进程中运行, 本进程不再重复启动")
            return False
        self._stop.clear()
        self._started_at = int(time.time())
        self._recover_stale_follows()
        self._task = asyncio.create_task(self._loop(), name="nanoquant-engine")
        self._follow_task = asyncio.create_task(
            self._signal_follow_loop(), name="nanoquant-signal-follow"
        )
        events.record("info", "engine", "交易引擎已启动")
        return True

    async def stop(self) -> None:
        self._stop.set()
        if self._task:
            try:
                await asyncio.wait_for(self._task, timeout=8)
            except (asyncio.TimeoutError, asyncio.CancelledError):
                self._task.cancel()
        self._task = None
        for t in list(self._entry_tasks.values()):
            t.cancel()
        self._entry_tasks.clear()
        if self._follow_task:
            self._follow_task.cancel()
            try:
                await self._follow_task
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
            self._follow_task = None
        self._release_lock()
        events.record("info", "engine", "交易引擎已停止")

    async def _loop(self) -> None:
        self._running = True
        while not self._stop.is_set():
            cfg = get_engine()
            interval = max(1, cfg.cycle_interval_sec)
            started = time.time()
            try:
                if cfg.enabled:
                    await self.cycle()
                else:
                    await self._idle_tick()
                state.set("last_cycle_ms", str(int((time.time() - started) * 1000)))
                state.set("last_cycle_ts", str(int(time.time())))
                state.set("last_error", "")
            except asyncio.CancelledError:
                raise
            except Exception as exc:  # noqa: BLE001
                log.exception("引擎循环异常")
                state.set("last_error", f"{type(exc).__name__}: {exc}"[:400])
                events.record("error", "engine", f"引擎循环异常: {exc}"[:300])
            await self._heartbeat()
            await asyncio.sleep(interval)
        self._running = False

    async def _idle_tick(self) -> None:
        await self._heartbeat()
        if get_news().enabled and time.time() - self._last_news > get_news().refresh_minutes * 60:
            self._last_news = time.time()
            try:
                await news_service.refresh()
            except Exception as exc:  # noqa: BLE001
                log.warning("资讯刷新失败: %s", exc)

    async def _heartbeat(self) -> None:
        try:
            from ..config import paths

            paths.HEARTBEAT.write_text(
                f"{int(time.time())}\n{'running' if get_engine().enabled else 'idle'}\n", encoding="utf-8"
            )
        except OSError:
            pass

    # ----------------------------------------------------------------- cycle
    async def cycle(self) -> dict[str, Any]:
        _t_cfg = time.perf_counter()
        engine_cfg = get_engine()
        risk_cfg = get_risk()
        strat_cfg = get_strategy()
        broker = get_broker(engine_cfg.mode)
        timings: dict[str, int] = {"config": int((time.perf_counter() - _t_cfg) * 1000)}
        if timings["config"] > 2500:
            log.warning("慢阶段 config 耗时 %dms", timings["config"])

        async def timed(name: str, coro):
            t0 = time.perf_counter()
            try:
                return await coro
            finally:
                cost = int((time.perf_counter() - t0) * 1000)
                timings[name] = cost
                if cost > 2500:
                    log.warning("慢阶段 %s 耗时 %dms", name, cost)

        try:
            account = await timed("account", broker.account())
        except (BinanceError, NoCredentials) as exc:
            state.set("last_error", f"账户同步失败: {str(exc)[:160]}")
            return {"skipped": f"账户不可用: {str(exc)[:120]}"}
        self._last_account = account.as_dict()
        self._last_equity = account.equity

        # 模拟盘与实盘都是交易所账户, 都需要与交易所对账
        await timed("reconcile", self._reconcile_live(broker))
        # 委托状态收敛: 挂单回执只会返回 NEW, 成交/撤销要回查才知道（60s 节流，无待查单时零请求）
        await timed("orders_sync", self._sync_orders_if_due())

        await timed("positions", self._manage_positions(broker, strat_cfg, engine_cfg))
        await timed("liquidation_risk", self._check_liquidation_risk(broker, risk_cfg))
        await timed("health_check", self._health_check())
        await timed("equity_sample", self._sample_equity(account))

        _t_gate = time.perf_counter()
        ok, why = risk_mod.trading_window_open(engine_cfg)
        gate = risk_mod.global_gates(account, engine_cfg, risk_cfg)
        _gate_ms = int((time.perf_counter() - _t_gate) * 1000)
        timings["gates"] = _gate_ms
        if _gate_ms > 2500:
            log.warning("慢阶段 gates 耗时 %dms", _gate_ms)
        if not ok:
            state.set("cycle_timings", __import__("json").dumps(timings))
            return {"skipped": why}
        if not gate.ok:
            # 只有需要人工/日切解除的闸门（日亏、回撤）才写 halted；
            # 止损熔断、连亏冷静期等定时自愈状态绝不能持久化，否则到期后仍被判熔断。
            if gate.halt:
                state.set("halted", state.normalize_halted(gate.halt))
                if getattr(engine_cfg, "close_on_halt", False):
                    await self._flatten_on_halt(gate.halt)
            state.set("cycle_timings", __import__("json").dumps(timings))
            return {"skipped": gate.reason}
        state.set("halted", "")

        # 交易模式体检：双向持仓 + 逐仓。只提示不阻断（下单参数会按模式自适应）
        await self._check_trade_mode(broker)

        signals = await timed("scan", self._scan(broker, account, engine_cfg, risk_cfg, strat_cfg))
        state.set("cycle_timings", __import__("json").dumps(timings))
        return {"signals": len(signals), "equity": round(account.equity, 6)}

    async def _reconcile_live(self, broker: Broker) -> None:
        mode = normalize_mode(getattr(broker, "mode", get_engine().mode))
        try:
            venue = await broker.positions()
        except Exception as exc:  # noqa: BLE001
            events.record("warn", "engine", f"仓位同步失败: {exc}"[:200])
            return
        venue_map = {p.symbol: p for p in venue}
        with session_scope() as s:
            # 只对账当前交易环境的仓位: 另一环境的仓位不应被判成"交易所已无持仓"
            local = (
                s.query(PositionRecord)
                .filter(PositionRecord.status == "open", PositionRecord.mode == mode)
                .all()
            )
            local_map = {p.symbol: p for p in local}
        now = int(time.time())
        for symbol, vp in venue_map.items():
            if symbol not in local_map:
                sl, tp = await self._algo_sl_tp(symbol)
                record_external_position(
                    symbol, vp.side, vp.qty, vp.entry_price, vp.mark_price, vp.leverage, mode,
                    unrealized=vp.unrealized, sl_price=sl, tp_price=tp,
                )
                events.record("warn", "engine", f"发现交易所外部仓位 {symbol} {vp.side} {vp.qty}", {"symbol": symbol})
                if sl <= 0 and tp <= 0:
                    # 裸仓：手动开仓常常没挂保护单，系统只"看到 0"不会替你挂。
                    # 默认只告警（auto_protect_external=False），需要时一键补挂。
                    events.record(
                        "warn", "engine",
                        f"{symbol} 外部仓位没有止损/止盈保护，可用「补挂保护单」按 ATR 生成",
                        {"symbol": symbol, "position_id": None},
                    )
                    if get_risk().auto_protect_external:
                        with session_scope() as s:
                            row = (
                                s.query(PositionRecord)
                                .filter(
                                    PositionRecord.symbol == symbol,
                                    PositionRecord.status == "open",
                                    PositionRecord.mode == mode,
                                )
                                .first()
                            )
                            pid = row.id if row else 0
                        if pid:
                            outcome = await self.protect_position(pid, reason="自动补挂")
                            if not outcome.get("ok"):
                                events.record(
                                    "error", "engine",
                                    f"{symbol} 自动补挂保护单失败: {outcome.get('reason')}",
                                    {"symbol": symbol},
                                )
            else:
                with session_scope() as s:
                    row = s.get(PositionRecord, local_map[symbol].id)
                    if row:
                        row.qty = vp.qty
                        # markPrice 而非 24h lastPrice：合约盈亏与保护单都按 markPrice 结算
                        row.mark_price = vp.mark_price or row.mark_price
                        row.entry_price = row.entry_price or vp.entry_price
                        row.venue_unrealized = float(vp.unrealized or 0.0)
                        row.venue_ts = now
        for symbol, lp in local_map.items():
            if symbol not in venue_map:
                with session_scope() as s:
                    row = s.get(PositionRecord, lp.id)
                    if row:
                        row.status = "closed"
                        row.closed_at = int(time.time())
                        row.exit_price = lp.mark_price or lp.entry_price
                        row.exit_reason = "venue_sync"
                events.record("warn", "engine", f"{symbol} 本地有仓但交易所已无持仓, 已按同步关闭", {"symbol": symbol})

    async def _sync_orders_if_due(self) -> None:
        """把非终态委托与交易所对齐（60s 一次）.

        只有存在待确认委托时才会产生 REST 请求：市价单回执即 FILLED，
        正常情况下这里每轮只做一次本地 DB 查询、零网络请求。
        """
        now = time.time()
        if now - self._last_order_sync < 60:
            return
        self._last_order_sync = now
        try:
            result = await order_sync.sync()
        except Exception as exc:  # noqa: BLE001  台账同步失败不能打断交易循环
            log.debug("委托状态同步跳过: %s", exc)
            return
        if result.get("updated"):
            log.info(
                "委托状态同步: 查 %d 条 / 更新 %d 条 / 失败 %d 条",
                result["checked"], result["updated"], result["errors"],
            )

    async def _flatten_on_halt(self, halt_reason: str) -> None:
        """熔断即离场（`engine.close_on_halt`）.

        以前这个开关只存在设置页：熔断只锁新开仓，已有仓位继续裸奔——
        20x 的仓位在"停止开仓"之后仍会把账户打穿，等于只关了一半的闸。
        同一原因 10 分钟内只执行一次，避免平仓失败时每轮反复重试。
        """
        if not halt_reason:
            return
        now = time.time()
        if now - getattr(self, "_last_halt_flatten", 0.0) < 600:
            return
        self._last_halt_flatten = now
        try:
            result = await self.flatten_all(reason=f"熔断离场: {halt_reason[:80]}", stop_engine=False)
        except Exception as exc:  # noqa: BLE001
            events.record("error", "risk", f"熔断离场失败: {str(exc)[:180]}")
            return
        closed = result.get("closed") or []
        failed = result.get("failed") or []
        if closed or failed:
            events.record(
                "error" if failed else "warn",
                "risk",
                f"熔断离场：平掉 {len(closed)} 笔、失败 {len(failed)} 笔",
                {"halt": halt_reason[:120], "closed": closed, "failed": failed},
            )

    async def flatten_all(self, *, reason: str = "紧急全平", stop_engine: bool = True) -> dict[str, Any]:
        """紧急全平：停引擎 → 撤挂单 → 市价平掉当前环境全部仓位.

        事故处置只应该有一个动作。顺序不能反：先停引擎（不再开新仓）、
        再撤挂单（保护单占保证金，也可能与市价平仓抢同一个方向），
        最后逐仓市价平；交易所侧若有系统还没记录的仓位，先补录再平。
        """
        mode = normalize_mode(get_engine().mode)
        stopped = False
        if stop_engine:
            await self.stop()
            update_group("engine", {"enabled": False})
            state.set("enabled", "false")
            stopped = True

        broker = get_broker(mode)
        errors: list[str] = []
        with session_scope() as s:
            rows = (
                s.query(PositionRecord)
                .filter(PositionRecord.status == "open", PositionRecord.mode == mode)
                .all()
            )
            targets = [
                {"id": r.id, "symbol": r.symbol, "side": r.side, "qty": float(r.qty)}
                for r in rows
            ]

        # 交易所可能有系统没来得及落库的仓位（手动开仓、外部操作）：以交易所为准补录
        try:
            venue = await broker.positions()
        except Exception as exc:  # noqa: BLE001
            venue = []
            errors.append(f"读取交易所持仓失败: {str(exc)[:100]}")
        for vp in venue:
            if any(t["symbol"] == vp.symbol and t["side"] == vp.side for t in targets):
                continue
            record_external_position(
                vp.symbol, vp.side, vp.qty, vp.entry_price, vp.mark_price, vp.leverage, mode,
                unrealized=vp.unrealized,
            )
            with session_scope() as s:
                row = (
                    s.query(PositionRecord)
                    .filter(
                        PositionRecord.symbol == vp.symbol,
                        PositionRecord.side == vp.side,
                        PositionRecord.status == "open",
                        PositionRecord.mode == mode,
                    )
                    .first()
                )
                if row is not None:
                    targets.append({"id": row.id, "symbol": row.symbol, "side": row.side, "qty": float(row.qty)})

        symbols = sorted({t["symbol"] for t in targets})
        for symbol in symbols:
            for cancel in (binance_client.cancel_all, binance_client.cancel_all_algo):
                try:
                    await cancel(symbol)
                except Exception as exc:  # noqa: BLE001
                    errors.append(f"{symbol} 撤单失败: {str(exc)[:60]}")

        closed: list[dict[str, Any]] = []
        failed: list[dict[str, Any]] = []
        for target in targets:
            with session_scope() as s:
                pos = s.get(PositionRecord, target["id"])
                if pos is None or pos.status != "open":
                    continue
                s.expunge(pos)
            try:
                res = await broker.close_position(position=pos, reason=reason)
            except Exception as exc:  # noqa: BLE001
                failed.append({"symbol": target["symbol"], "side": target["side"], "error": str(exc)[:120]})
                continue
            if res.ok:
                closed.append({"symbol": target["symbol"], "side": target["side"], "qty": target["qty"]})
            else:
                failed.append(
                    {"symbol": target["symbol"], "side": target["side"], "error": res.message or "平仓失败"}
                )

        events.record(
            "error" if failed else "warn",
            "risk",
            f"{reason}：平仓 {len(closed)} 笔、失败 {len(failed)} 笔、撤单标的 {len(symbols)} 个",
            {"closed": closed, "failed": failed, "engine_stopped": stopped},
        )
        return {
            "ok": not failed,
            "engine_stopped": stopped,
            "closed": closed,
            "failed": failed,
            "symbols": symbols,
            "errors": errors,
        }

    async def protect_position(self, position_id: int, *, reason: str = "手动补挂") -> dict[str, Any]:
        """给没有保护单的仓位按 ATR 补挂止损/止盈.

        手动开的仓不走系统下单链路，交易所侧可能压根没挂止损，对账只会显示 `sl=0`。
        这里用当前周期的 ATR 与策略倍数算保护价，复用 amend_stop（先撤旧单再重挂），
        并在成交后把 sl/tp 写回持仓记录。
        """
        from . import indicators as ta

        with session_scope() as s:
            pos = s.get(PositionRecord, position_id)
            if pos is None or pos.status != "open":
                return {"ok": False, "reason": "仓位不存在或已平仓"}
            info = {
                "symbol": pos.symbol,
                "side": pos.side,
                "qty": float(pos.qty),
                "entry": float(pos.entry_price or 0.0),
                "mode": pos.mode,
            }
        if info["qty"] <= 0 or info["entry"] <= 0:
            return {"ok": False, "reason": "仓位数量或成本价无效"}

        cfg = get_strategy()
        try:
            candles = await binance_client.klines(info["symbol"], cfg.timeframe, limit=_scan_bars())
        except (BinanceError, NoCredentials) as exc:
            return {"ok": False, "reason": f"取 K 线失败: {str(exc)[:80]}"}
        atr = ta.atr(candles, cfg.atr_period)
        if atr <= 0:
            return {"ok": False, "reason": "ATR 不可用，无法计算保护价"}

        direction = 1 if info["side"] == "long" else -1
        stop = info["entry"] - direction * atr * cfg.sl_atr_mult
        tp = info["entry"] + direction * atr * cfg.tp_atr_mult if cfg.tp_atr_mult > 0 else 0.0
        if stop <= 0:
            return {"ok": False, "reason": "止损价计算为负，请检查 ATR 与倍数"}

        broker = get_broker(normalize_mode(info["mode"]))
        res = await broker.amend_stop(info["symbol"], info["side"], stop, info["qty"], tp)
        if not res.ok:
            return {"ok": False, "reason": res.message or "交易所拒绝挂保护单"}

        with session_scope() as s:
            row = s.get(PositionRecord, position_id)
            if row is not None:
                row.sl_price = stop
                row.tp_price = tp
        events.record(
            "info", "engine",
            f"{info['symbol']} 已补挂保护单 SL={stop:.8g} TP={tp:.8g}（{reason}）",
            {"symbol": info["symbol"], "position_id": position_id},
        )
        return {"ok": True, "sl_price": round(stop, 8), "tp_price": round(tp, 8)}

    async def _algo_sl_tp(self, symbol: str) -> tuple[float, float]:
        """从交易所保护单提取该标的的止损/止盈触发价（手动开仓同步用）."""
        sl = tp = 0.0
        try:
            rows = await binance_client.open_algo_orders(symbol, fresh=True)
        except (BinanceError, NoCredentials):
            return sl, tp
        for row in rows:
            if not isinstance(row, dict):
                continue
            typ = str(row.get("orderType") or row.get("type") or "")
            trig = float(row.get("triggerPrice") or row.get("stopPrice") or 0)
            if trig <= 0:
                continue
            if typ == "STOP_MARKET":
                sl = trig
            elif typ == "TAKE_PROFIT_MARKET":
                tp = trig
        return sl, tp

    # ------------------------------------------------------------- positions
    async def _manage_positions(self, broker: Broker, cfg: StrategySettings, engine_cfg: EngineSettings) -> None:
        mode = normalize_mode(getattr(broker, "mode", engine_cfg.mode))
        with session_scope() as s:
            # 只管理当前交易环境下的仓位: 模拟盘/实盘仓位互不影响
            rows = (
                s.query(PositionRecord)
                .filter(PositionRecord.status == "open", PositionRecord.mode == mode)
                .order_by(PositionRecord.opened_at.asc())
                .all()
            )
            rows = [r for r in rows]
        if not rows:
            return
        symbols = list({r.symbol for r in rows})
        try:
            tickers = await binance_client.tickers(symbols)
        except (BinanceError, NoCredentials) as exc:
            log.warning("持仓管理取价失败, 本轮跳过: %s", str(exc)[:160])
            return
        now = int(time.time())
        for pos in rows:
            ticker = tickers.get(pos.symbol)
            if ticker is None:
                continue
            mark = ticker.last
            with session_scope() as s:
                row = s.get(PositionRecord, pos.id)
                if row is None or row.status != "open":
                    continue
                venue_ts = int(row.venue_ts or 0)
                if venue_ts and now - venue_ts < position_view.VENUE_FRESH_SEC and row.mark_price:
                    # 交易所刚同步过 markPrice: 出场判定也应对齐它，别用 24h 最新成交价覆盖
                    mark = float(row.mark_price)
                else:
                    row.mark_price = mark
                if row.side == "long":
                    row.peak_price = max(row.peak_price or row.entry_price, mark)
                else:
                    row.peak_price = min(row.peak_price or row.entry_price, mark) if row.peak_price else mark
                sl, tp, peak, opened, side, entry = row.sl_price, row.tp_price, row.peak_price, row.opened_at, row.side, row.entry_price
                score = row.signal_score
                initial_sl = row.initial_sl_price or 0.0
                partial_at = row.partial_taken_at or 0
                live_qty = row.qty
                entry_feats = dict(row.features or {})

            # ---- 部分止盈: 浮盈达到 N 倍初始风险时先平掉一部分 ----
            # 放在硬止损/止盈判断之前, 触发后本轮直接 continue, 避免同轮重复平仓。
            if cfg.partial_tp_at_r > 0 and not partial_at and live_qty > 0 and initial_sl > 0:
                r_mult = partial_tp_r(side, entry, initial_sl, mark)
                if r_mult >= cfg.partial_tp_at_r:
                    filters = symbol_service.filters_for(pos.symbol)
                    if filters is None:
                        part_qty = live_qty * cfg.partial_tp_ratio
                    else:
                        part_qty = partial_close_qty(
                            live_qty, cfg.partial_tp_ratio, filters.qty_step, filters.min_qty
                        )
                    if part_qty > 0:
                        result = await broker.close_position(position=pos, reason="partial_tp", qty=part_qty)
                        if result.ok:
                            with session_scope() as s:
                                row = s.get(PositionRecord, pos.id)
                                if row:
                                    row.partial_taken_at = now
                            events.record(
                                "info",
                                "trade",
                                f"{pos.symbol} 部分止盈 {cfg.partial_tp_ratio:.0%} @{mark:.8g} (R={r_mult:.2f})",
                                {"symbol": pos.symbol, "qty": part_qty, "r": round(r_mult, 3)},
                            )
                            continue

            exit_now, reason = detect_exit(
                side=side, entry_price=entry, mark_price=mark, peak_price=peak,
                sl_price=sl, tp_price=tp, opened_at=opened, now_ts=now, cfg=cfg, score=score,
                # 用**入场时**的体制判断震荡：震荡市里资金不该空耗在不会突破的仓位上
                entry_regime=entry_regime(entry_feats, cfg),
            )
            if exit_now:
                await self._close(broker, pos.id, reason, mark)
                continue
            # 保本上移
            direction = 1 if side == "long" else -1
            r_multiple = (mark - entry) * direction / max(1e-9, abs(entry - sl)) if sl else 0.0
            if sl and cfg.breakeven_at_r > 0 and r_multiple >= cfg.breakeven_at_r:
                new_sl = entry * (1 + 0.0004 * direction)
                if (direction == 1 and new_sl > sl) or (direction == -1 and new_sl < sl):
                    # 必须把止盈一起传下去：amend_stop 内部会 cancel_all，
                    # 只挂止损等于把止盈单撤掉不再补 —— 仓位变成"有止损、没止盈"
                    res = await broker.amend_stop(
                        pos.symbol, side, new_sl, pos.qty, tp_price=float(tp or 0)
                    )
                    if res.ok:
                        events.record("info", "engine", f"{pos.symbol} 止损上移至保本 {new_sl:.8g}")

    async def _close(self, broker: Broker, position_id: int, reason: str, mark: float) -> None:
        with session_scope() as s:
            pos = s.get(PositionRecord, position_id)
            if pos is None:
                return
            s.expunge(pos)
        res = await broker.close_position(position=pos, reason=reason)
        if not res.ok:
            events.record("error", "engine", f"{pos.symbol} 平仓失败: {res.message}"[:300], {"symbol": pos.symbol})
            return
        pnl = 0.0
        with session_scope() as s:
            row = s.get(PositionRecord, position_id)
            if row:
                pnl = row.realized_pnl
                trade_id = row.trade_id
            else:
                trade_id = 0
        risk_mod.register_close(pnl, pos.symbol, get_risk(), reason)
        events.record(
            "info",
            "trade",
            f"{pos.symbol} {pos.side} 平仓({reason}) 收益 {pnl:+.5f} USDT",
            {"symbol": pos.symbol, "reason": reason, "pnl": round(pnl, 8), "mark": mark},
        )
        self._trades_since_review += 1
        evo_cfg = get_evolution()
        if trade_id and evo_cfg.enabled and evo_cfg.review_on_close:
            asyncio.create_task(self._review_trade(trade_id))

    async def _review_trade(self, trade_id: int) -> None:
        # 平仓触发与补扫可能同时命中同一笔，用进程内集合去重，避免重复消耗 AI 额度
        if trade_id in self._reviewing:
            return
        self._reviewing.add(trade_id)
        try:
            # 平仓只累计样本；攒够 batch_review_size 笔才由 review_batch 真正调模型
            result = await asyncio.wait_for(evolution_review.review_trade(trade_id), timeout=120)
            if result.get("ok") and not result.get("skipped"):
                events.record(
                    "info", "evolution",
                    f"批量复盘完成: {result.get('sample_size', 0)} 笔样本, "
                    f"change_status={result.get('change_status')}, "
                    f"产出/更新心法 {len(result.get('applied', []))} 条",
                    {"trade_id": trade_id, "result": result},
                )
        except Exception as exc:  # noqa: BLE001
            log.warning("复盘失败: %s", exc)
            events.record("warn", "evolution", f"交易 #{trade_id} 复盘失败: {exc}"[:200])
        finally:
            self._reviewing.discard(trade_id)

    def _review_backfill_if_due(self, now: int) -> None:
        """每 5 分钟补扫一笔漏掉的复盘（进程重启/任务丢失时兜底）."""
        cfg = get_evolution()
        if not cfg.enabled or not cfg.review_on_close:
            return
        if now - self._last_review_backfill < 300:
            return
        self._last_review_backfill = now
        asyncio.create_task(self._review_backfill_async())

    async def _review_backfill_async(self) -> None:
        """补扫：攒够批量样本才真正复盘，避免每 5 分钟重复扫同一笔."""
        from ..models import TradeRecord

        try:
            cfg = get_evolution()
            with session_scope() as s:
                pending = s.query(TradeRecord).filter(TradeRecord.reviewed.is_(False)).count()
            if pending >= cfg.batch_review_size:
                result = await evolution_review.review_batch()
                if result.get("ok") and not result.get("skipped"):
                    events.record(
                        "info", "evolution",
                        f"补扫批量复盘: {result.get('sample_size', 0)} 笔, {result.get('change_status')}",
                        result,
                    )
        except Exception as exc:  # noqa: BLE001
            log.warning("复盘补扫失败: %s", exc)

    async def _maybe_global_review(self) -> None:
        cfg = get_evolution()
        if not cfg.enabled:
            return
        if self._trades_since_review < cfg.global_review_every_trades:
            return
        self._trades_since_review = 0
        try:
            result = await asyncio.wait_for(evolution_review.global_review(), timeout=180)
            events.record(
                "info", "evolution",
                f"全局复盘完成: 新心法 {len(result.get('applied', []))} 条, 淘汰 {len(result.get('retired', []))} 条",
                result,
            )
        except Exception as exc:  # noqa: BLE001
            log.warning("全局复盘失败: %s", exc)

    async def _check_trade_mode(self, broker: Broker) -> None:
        """5 分钟一次体检：双向持仓 + 逐仓.

        单向/全仓都不阻断开仓（下单参数自适应），但要在面板与事件里明确提示，
        避免用户以为系统跑在双向逐仓、实际仍是单向全仓。
        """
        now = time.time()
        if now - self._trade_mode_ts < 300:
            return
        self._trade_mode_ts = now
        try:
            status = await broker.trade_mode_status(sorted(symbol_service.known_symbols()))
        except Exception as exc:  # noqa: BLE001
            log.debug("交易模式体检跳过: %s", exc)
            return
        state.set("trade_mode", __import__("json").dumps(status, ensure_ascii=False))
        if not (status.get("need_hedge") or status.get("need_isolated")):
            return
        if now - self._trade_mode_warn_ts < 3600:
            return
        self._trade_mode_warn_ts = now
        if status.get("need_hedge"):
            events.record(
                "warn", "engine",
                "账户为单向持仓：系统按双向逐仓设计，请在「参数配置 → 引擎 → 交易模式」开启双向持仓"
                "（需先平掉全部仓位与挂单）",
            )
        if status.get("need_isolated"):
            events.record(
                "warn", "engine",
                f"{len(status['need_isolated'])} 个标的仍为全仓：无持仓时会自动切逐仓，有持仓需先平仓再切换",
            )

    # ------------------------------------------------------------------ scan
    async def _symbols(self, cfg: EngineSettings) -> list[str]:
        """只扫描标的池里启用的标的.

        注意: 池为空时**不回落到配置里的默认数组**, 否则用户清空标的池后引擎仍会交易默认品种.
        """
        with session_scope() as s:
            from ..models import TradingSymbol

            rows = s.query(TradingSymbol).filter(TradingSymbol.enabled.is_(True)).all()
            pool = [r.symbol for r in rows]
        return pool[: cfg.max_symbols_per_cycle]

    async def _scan(
        self,
        broker: Broker,
        account,
        engine_cfg: EngineSettings,
        risk_cfg: RiskSettings,
        strat_cfg: StrategySettings,
    ) -> list[dict[str, Any]]:
        # 每轮用最新账户快照重置预算：并发 spawn 的开仓任务共享同一份额度，
        # 避免 DOGE / ARB 同一秒各自按 available 算出满额、第二笔被 -2019 拒
        self._margin.begin_cycle(getattr(account, "available", 0.0))
        symbols = await self._symbols(engine_cfg)
        if not symbols:
            return []
        with session_scope() as s:
            from ..models import TradingSymbol

            min_scores = {
                r.symbol: float(r.min_score or 0.0)
                for r in s.query(TradingSymbol).filter(TradingSymbol.enabled.is_(True)).all()
            }
        # 标的池**全量扫描**：策略只在 AI 决策阶段注入，不做标的预筛
        try:
            tickers = await binance_client.tickers(symbols)
            funding = await binance_client.funding_rates(symbols)
            oi = await binance_client.open_interest(symbols[:6])
        except (BinanceError, NoCredentials) as exc:
            events.record("warn", "engine", f"行情取数失败, 本轮跳过扫描: {str(exc)[:160]}")
            return []
        # OI 变化自己算：接口只返回当前快照。第一轮没有上一轮基线 -> 该因子标记缺失，
        # 由打分逻辑把它的权重剔出分母，而不是当成"变化 0"参与加权。
        oi_change: dict[str, float] = {}
        for _sym, _val in oi.items():
            _prev = self._oi_prev.get(_sym)
            if _prev and _prev > 0 and _val > 0:
                oi_change[_sym] = round((_val / _prev - 1) * 100, 4)
            self._oi_prev[_sym] = _val
        strat_fp = strategy_fingerprint(strat_cfg)
        prices = {s: t.last for s, t in tickers.items()}
        results: list[dict[str, Any]] = []

        # 第一阶段（并发）：只做网络 + 指标计算，不碰数据库。
        # 串行时每个标的要等一个 RTT，12 个标的就多出 2 秒"信号延迟"；
        # 并发只改延迟、不改每轮请求数（权重不变），所以默认开到 3。
        sem = asyncio.Semaphore(max(1, min(8, int(getattr(engine_cfg, "scan_concurrency", 3) or 3))))

        async def _analyze(symbol: str) -> dict[str, Any] | None:
            ticker = tickers.get(symbol)
            if ticker is None:
                return None
            async with sem:
                try:
                    candles = await binance_client.klines(symbol, strat_cfg.timeframe, _scan_bars())
                except (BinanceError, NoCredentials) as exc:
                    log.warning("%s K线取数失败: %s", symbol, str(exc)[:120])
                    return None
                # 高周期趋势：只做与 HTF 同向的信号（默认关闭，开启时每标的每轮多一次 REST）
                htf_trend = ""
                if strat_cfg.htf_timeframe:
                    try:
                        htf_candles = await binance_client.klines(symbol, strat_cfg.htf_timeframe, 60)
                        htf_closes = [c.close for c in htf_candles]
                        htf_fast = ta.ema(htf_closes, strat_cfg.ema_fast)
                        htf_slow = ta.ema(htf_closes, strat_cfg.ema_slow)
                        htf_trend = "up" if htf_fast > htf_slow else ("down" if htf_fast < htf_slow else "flat")
                    except (BinanceError, NoCredentials):
                        htf_trend = ""
            if len(candles) < 30:
                return None
            features = build_features(
                candles, strat_cfg,
                # 取不到就是 None（缺失），不能塞 0.0 冒充"费率中性 / OI 没变"
                funding=funding.get(symbol),
                open_interest=oi.get(symbol, 0.0),
                oi_change_pct=oi_change.get(symbol),
                change_24h=ticker.change_pct,
                quote_volume=ticker.quote_volume,
                data_degraded=getattr(ticker, "source", "ws") == "rest",
                htf_trend=htf_trend,
                # 传 now 才会做"停更"判断；回测不传，历史数据天然不触发
                now=time.time(),
            )
            # 策略版本指纹：随信号落库并写进持仓 features，复盘时能回答
            # "这笔亏损是策略版本问题还是执行问题"
            features["strategy_fp"] = strat_fp
            features["_bars"] = [
                {"o": c.open, "h": c.high, "l": c.low, "c": c.close} for c in candles[-14:]
            ]
            return {"symbol": symbol, "ticker": ticker, "features": features}

        analyzed = await asyncio.gather(*(_analyze(s) for s in symbols))

        # 第二阶段（串行）：落库 + 闸门 + 起任务。写库留在单线程里，避免 SQLite 写竞争；
        # gather 保序，所以信号顺序与标的池顺序一致，排查时好对照。
        for item in analyzed:
            if item is None:
                continue
            symbol = item["symbol"]
            ticker = item["ticker"]
            features = item["features"]
            sig = score_signal(symbol, features, strat_cfg)
            per_symbol_min = min_scores.get(symbol, 0.0)
            if sig.direction != "flat" and per_symbol_min > 0 and abs(sig.score) < per_symbol_min:
                sig.blocked = f"信号分 {sig.score:.3f} 低于该标的最低要求 {per_symbol_min:.2f}"
                sig.direction = "flat"
            rec = self._record_signal(sig, ticker, "scan")
            results.append(rec)
            if sig.direction == "flat":
                continue
            # 只观察不交易：信号照常落库供人眼对照，但不下单。
            # 与 engine.enabled 的区别是那个连跟单一起停，这个是"只看不做"。
            if not strat_cfg.auto_entry:
                self._update_signal_reject(rec["signal_id"], "自动开仓已关闭(仅观察)")
                continue
            if symbol in self._entry_tasks and not self._entry_tasks[symbol].done():
                continue
            gate = risk_mod.symbol_gates(
                symbol, account, engine_cfg, risk_cfg, prices=prices, side=sig.direction
            )
            if not gate.ok:
                self._update_signal_reject(rec["signal_id"], gate.reason)
                continue
            self._entry_tasks[symbol] = asyncio.create_task(
                self._open_entry_guarded(
                    broker, sig, ticker, account, engine_cfg, risk_cfg, strat_cfg, rec["signal_id"]
                )
            )
            self._entry_tasks[symbol].add_done_callback(lambda t, s=symbol: self._entry_tasks.pop(s, None))
        return results

    def _record_signal(self, sig, ticker, source: str) -> dict[str, Any]:
        # 先取环境再开事务: 避免在写事务里再嵌套一次配置读取
        mode = normalize_mode(get_engine().mode)
        now = int(time.time())
        features = {**sig.features, "price": ticker.last, "source": source}
        if sig.direction == "flat":
            features = _flat_features(features)
        reason = sig.blocked or ""
        decision = "pending" if sig.direction != "flat" else "flat"
        key = (mode, sig.symbol, sig.direction, decision, reason)

        with session_scope() as s:
            # flat 信号每轮都写会把信号表撑到 2 万行/天；同状态的在 60s 内合并为一行,
            # 用 repeats 保留真实扫描次数，审计页的密度图按 repeats 加权。
            if sig.direction == "flat":
                cached_id = self._signal_dedup.get(key)
                if cached_id:
                    row = s.get(Signal, cached_id)
                    first_ts = int((row.features or {}).get("first_ts", row.ts)) if row else 0
                    if row is not None and now - first_ts < SIGNAL_DEDUP_SEC:
                        # 只更新计数与时间戳：特征快照几乎逐轮相同，重写它纯粹是放大 WAL
                        row.repeats = int(row.repeats or 1) + 1
                        row.ts = now
                        row.score = sig.score
                        s.flush()
                        return self._signal_view(row, sig, reason)

            row = Signal(
                ts=now,
                symbol=sig.symbol,
                direction=sig.direction,
                score=sig.score,
                features={**features, "first_ts": now},
                decision=decision,
                reject_reason=reason,
                mode=mode,
                repeats=1,
            )
            s.add(row)
            s.flush()
            if sig.direction == "flat":
                self._signal_dedup[key] = row.id
                if len(self._signal_dedup) > 256:
                    self._signal_dedup.clear()
            return self._signal_view(row, sig, reason)

    @staticmethod
    def _signal_view(row: Signal, sig, reason: str) -> dict[str, Any]:
        return {
            "signal_id": row.id,
            "symbol": sig.symbol,
            "direction": sig.direction,
            "score": sig.score,
            "blocked": reason,
        }

    def _update_signal_reject(self, signal_id: int, reason: str) -> None:
        with session_scope() as s:
            row = s.get(Signal, signal_id)
            if row:
                row.decision = "rejected"
                row.reject_reason = reason[:250]

    # ------------------------------------------------------- 外部信号跟单
    def _symbol_acquire(self, symbol: str, ttl: float = 90.0) -> bool:
        """同标的开仓互斥：引擎扫描 / 信号跟单谁先拿到锁谁开，另一个直接跳过."""
        now = time.time()
        if now - self._symbol_busy.get(symbol, 0.0) < ttl:
            return False
        self._symbol_busy[symbol] = now
        return True

    def _symbol_release(self, symbol: str) -> None:
        self._symbol_busy.pop(symbol, None)

    async def _open_entry_guarded(
        self, broker, sig, ticker, account, engine_cfg, risk_cfg, strat_cfg, signal_id
    ) -> None:
        if not self._symbol_acquire(sig.symbol):
            self._update_signal_reject(signal_id, "同标的开仓任务在途, 跳过本轮")
            return
        try:
            await self._open_entry(
                broker, sig, ticker, account, engine_cfg, risk_cfg, strat_cfg, signal_id
            )
        finally:
            self._symbol_release(sig.symbol)

    @staticmethod
    def _follow_offset_key(path: str) -> str:
        """每个频道文件独立游标：自动发现换频道时不重放另一个频道的历史."""
        digest = hashlib.sha1(os.path.abspath(path).encode("utf-8")).hexdigest()[:12]
        return f"signal_follow_offset:{digest}"

    def _recover_stale_follows(self) -> None:
        """进程在下单途中崩溃会留下 processing 行；按 fail-closed 结案，避免重启后重复开仓."""
        cutoff = int(time.time()) - 15 * 60
        try:
            with session_scope() as s:
                rows = (
                    s.query(SignalFollow)
                    .filter(SignalFollow.status == "processing", SignalFollow.created_at < cutoff)
                    .all()
                )
                for row in rows:
                    row.status = "skipped"
                    row.reason = "跟单中断(进程重启), 是否成交未确认; 已按 fail-closed 跳过"
                if rows:
                    events.record("warn", "signal_follow", f"重启后结案 {len(rows)} 条中断的跟单")
        except Exception as exc:  # noqa: BLE001
            log.warning("跟单中断记录恢复失败: %s", exc)

    async def _poll_signal_feed_once(self) -> int:
        """读一轮信号源，返回本次处理的行数.

        支持两类信号源：本地 ``signals.jsonl`` 文件（增量字节游标）与 http(s) 频道链接
        （去重索引）。两者首次接入都**不回放历史**：文件从尾部、链接只建索引。
        """
        cfg = get_signal_follow()
        if not cfg.enabled:
            self._follow_last_error = ""
            return 0
        from ..config import paths

        raw = (cfg.jsonl_path or "").strip()
        if signal_feed.is_url(raw):
            return await self._poll_url(raw)

        path = signal_feed.resolve_path(cfg.jsonl_path, str(paths.HOME))
        if not path:
            self._follow_last_error = "未找到 signals.jsonl（请配置 jsonl_path 或 AO_MONITOR_DATA）"
            return 0
        if path != self._follow_path:
            self._follow_path = path
            self._follow_cursor_key = self._follow_offset_key(path)
            self._follow_url_primed = False
            self._follow_url_seen = set()
            state.set("signal_follow_path", path)
        if not os.path.isfile(path):
            self._follow_last_error = f"等待信号文件: {path}"
            return 0
        raw_off = state.get(self._follow_cursor_key, "")
        if raw_off == "":
            offset = os.path.getsize(path)
            state.set(self._follow_cursor_key, str(offset))
            events.record(
                "info", "signal_follow",
                f"接入信号文件 {path}, 从尾部 #{offset} 开始（不回放历史）",
            )
        else:
            offset = state.get_int(self._follow_cursor_key, 0)
        rows, new_offset = signal_feed.read_new_lines(path, offset)
        if new_offset != offset:
            state.set(self._follow_cursor_key, str(new_offset))
        self._follow_last_poll = time.time()
        self._follow_last_error = ""
        for row in rows:
            await self._handle_signal_row(row)
        return len(rows)

    async def _poll_url(self, url: str) -> int:
        """拉取频道链接（http/https）信号；首次接入只建去重索引、不回放历史.

        链接没有「尾部」，故首次轮询把当前返回的所有信号标识写入
        ``_follow_url_seen`` 而不跟单；之后只跟「新出现」的信号，与文件尾部语义一致。
        """
        if url != self._follow_path:
            self._follow_path = url
            self._follow_cursor_key = ""
            self._follow_url_primed = False
            self._follow_url_seen = set()
            state.set("signal_follow_path", url)
            events.record("info", "signal_follow", f"接入频道链接 {url}")
        try:
            rows = await signal_feed.fetch_url_signals(url, timeout=10.0)
        except Exception as exc:  # noqa: BLE001
            self._follow_last_error = f"拉取频道链接失败: {exc}"[:200]
            return 0
        self._follow_last_poll = time.time()
        self._follow_last_error = ""
        if not self._follow_url_primed or self._follow_url_primed_for != url:
            self._follow_url_primed_for = url
            self._follow_url_primed = True
            for r in rows:
                parsed = signal_feed.parse_signal(r)
                if parsed:
                    self._follow_url_seen.add(signal_feed.signal_identity(parsed))
            return 0
        new_rows: list[dict[str, Any]] = []
        for r in rows:
            parsed = signal_feed.parse_signal(r)
            if not parsed:
                continue
            key = signal_feed.signal_identity(parsed)
            if key in self._follow_url_seen:
                continue
            self._follow_url_seen.add(key)
            new_rows.append(r)
        for row in new_rows:
            await self._handle_signal_row(row)
        return len(new_rows)

    def reset_follow_cursor(self) -> dict[str, Any]:
        """手动把游标移到当前源尾部：本地文件移字节游标；频道链接清空去重索引（不回放历史）."""
        path = self._follow_path
        if signal_feed.is_url(path):
            self._follow_url_primed = False
            self._follow_url_seen = set()
            return {"ok": True, "path": path, "offset": 0, "mode": "url"}
        if not path or not os.path.isfile(path):
            return {"ok": False, "reason": "信号文件尚不可用"}
        key = self._follow_offset_key(path)
        offset = os.path.getsize(path)
        state.set(key, str(offset))
        self._follow_cursor_key = key
        return {"ok": True, "path": path, "offset": offset}

    async def _signal_follow_loop(self) -> None:
        """每 poll_sec 增量读本地 jsonl，收到即跟（不走 15s 的 cycle）."""
        while not self._stop.is_set():
            cfg = get_signal_follow()
            try:
                if cfg.enabled:
                    await self._poll_signal_feed_once()
            except Exception as exc:  # noqa: BLE001
                self._follow_last_error = str(exc)[:200]
                log.warning("信号跟单轮询失败: %s", exc)
            await asyncio.sleep(max(1, cfg.poll_sec if cfg.enabled else 5))

    async def _handle_signal_row(self, row: dict[str, Any]) -> dict[str, Any]:
        parsed = signal_feed.parse_signal(row)
        if parsed is None:
            return {"ok": False, "status": "invalid", "reason": "无法识别的信号格式"}
        return await self._follow_signal(parsed)

    async def follow_signal_now(self, row: dict[str, Any]) -> dict[str, Any]:
        """HTTP 直推入口（与文件轮询共用去重/风控/幂等），信号到达即刻处理."""
        if not get_signal_follow().enabled:
            return {"ok": False, "status": "skipped", "reason": "信号跟单未开启"}
        return await self._handle_signal_row(row)

    def _claim_follow(self, key: str, parsed: dict[str, Any], mode: str) -> bool:
        """下单**之前**用唯一键占位：文件轮询与 HTTP 重推同时到达也只会有一条进下单链路."""
        try:
            with session_scope() as s:
                if s.query(SignalFollow).filter(SignalFollow.signal_id == key).first():
                    return False
                s.add(
                    SignalFollow(
                        signal_id=key, symbol=parsed["symbol"], side=parsed["side"],
                        channel=_channel_of(parsed),
                        status="processing", reason="处理中", mode=mode,
                        leverage=int(parsed.get("leverage") or 0),
                        payload={"raw": parsed.get("raw"), "signal_ts": parsed.get("ts")},
                    )
                )
            return True
        except IntegrityError:
            return False

    def _finish_follow(
        self, key: str, parsed: dict[str, Any], status: str, reason: str, mode: str, **extra: Any
    ) -> None:
        """把占位行更新成最终裁决（同一信号只落一行，审计链完整）."""
        payload = {
            "raw": parsed.get("raw"),
            "signal_ts": parsed.get("ts"),
            "entry": parsed.get("entry"),
            "stop": parsed.get("stop"),
            "tp": parsed.get("tp"),
            "event": parsed.get("event"),
            **{k: v for k, v in extra.items() if k not in ("qty", "price", "margin", "leverage")},
        }
        try:
            with session_scope() as s:
                row = s.query(SignalFollow).filter(SignalFollow.signal_id == key).first()
                if row is None:
                    row = SignalFollow(
                        signal_id=key, symbol=parsed["symbol"], side=parsed["side"], mode=mode
                    )
                    s.add(row)
                row.status = status
                row.reason = (reason or "")[:240]
                if not row.channel:
                    row.channel = _channel_of(parsed)
                row.leverage = int(extra.get("leverage") or row.leverage or 0)
                row.qty = float(extra.get("qty") or 0)
                row.price = float(extra.get("price") or 0)
                row.margin = float(extra.get("margin") or 0)
                row.payload = payload
        except Exception as exc:  # noqa: BLE001
            log.warning("信号跟单记录写入失败: %s", exc)

    def _follow_skip(
        self, key: str, parsed: dict[str, Any], status: str, reason: str, mode: str, **extra: Any
    ) -> dict[str, Any]:
        self._finish_follow(key, parsed, status, reason, mode, **extra)
        label = "info" if status in {"skipped", "dry_run", "duplicate"} else "warn"
        events.record(label, "signal_follow", f"{parsed['symbol']} {parsed['side']} {status}: {reason}"[:220])
        return {"ok": status == "dry_run", "status": status, "reason": reason, **extra}

    async def _follow_signal(self, parsed: dict[str, Any]) -> dict[str, Any]:
        """串行处理一条信号，避免文件轮询与 HTTP 直推并发时重复占用额度."""
        async with self._follow_lock:
            return await self._follow_signal_locked(parsed)

    async def _follow_signal_locked(self, parsed: dict[str, Any]) -> dict[str, Any]:
        """跟一条外部信号：风控/资金/杠杆全部走本系统，不复制信号方的仓位参数."""
        cfg = get_signal_follow()
        engine_cfg = get_engine()
        risk_cfg = get_risk()
        mode = normalize_mode(engine_cfg.mode)
        key = signal_feed.signal_identity(parsed)
        symbol, side = parsed["symbol"], parsed["side"]
        now = int(time.time())

        if not cfg.enabled:
            return {"ok": False, "status": "skipped", "reason": "信号跟单未开启"}
        if not self._claim_follow(key, parsed, mode):
            return {"ok": True, "status": "duplicate", "reason": "该信号已处理过"}

        # 平仓/止损/更新一律不跟; 未知事件按 RUNBOOK S6-06「先记录原文 + 报告需扩充规则」
        # 再放行（有明确方向、且不含任何平仓词），strict_events 开启时改为 fail-closed。
        if parsed.get("event") == "exit":
            return self._follow_skip(key, parsed, "skipped", "非开仓事件(平仓/止损/更新), 忽略", mode)
        if parsed.get("event") == "unknown":
            events.record(
                "warn", "signal_follow",
                f"未识别的事件字段, 需扩充规则: {str((parsed.get('raw') or {}).get('event') or '')[:60]}",
                {"raw": parsed.get("raw")},
            )
            if cfg.strict_events:
                return self._follow_skip(key, parsed, "skipped", "未识别事件类型(严格模式已拒绝)", mode)

        if not engine_cfg.enabled:
            return self._follow_skip(key, parsed, "skipped", "交易引擎未启动", mode)
        age = now - int(parsed["ts"])
        if age > cfg.max_age_sec or age < -60:
            return self._follow_skip(
                key, parsed, "skipped", f"信号时间异常或已过期 {age}s (上限 {cfg.max_age_sec}s)", mode
            )
        with session_scope() as s:
            opened_last_hour = (
                s.query(SignalFollow)
                .filter(SignalFollow.status == "opened", SignalFollow.created_at >= now - 3600)
                .count()
            )
        if opened_last_hour >= cfg.max_per_hour:
            return self._follow_skip(
                key, parsed, "skipped", f"每小时跟单已达上限 {cfg.max_per_hour} 笔", mode
            )

        # 加仓/减仓/平仓/移止损：语义不同，走独立分支，且**排在标的锁之前**。
        # 那个锁的语义是"别对同一标的重复开仓"，开仓成功后会刻意保留 90 秒；
        # 若把离场动作放在锁后面，开仓后一分钟内收到的平仓信号会被挡掉 ——
        # 该止损的时候止不掉，比重复开仓严重得多。
        business = parsed.get("event") or "open"
        if business in ("add", "reduce", "close", "move_stop"):
            return await self._follow_lifecycle(key, parsed, business, mode, cfg, risk_cfg)

        if not self._symbol_acquire(symbol):
            return self._follow_skip(key, parsed, "skipped", "同标的开仓任务在途", mode)
        rid = 0
        follow_opened = False
        try:
            # 自动补池：信号里的标的可能不在本系统池内（池内但停用则不擅自启用）
            pool = symbol_service.known_symbols()
            if symbol not in pool:
                if not cfg.auto_add_symbol:
                    return self._follow_skip(key, parsed, "skipped", "标的不在池内且未开启自动添加", mode)
                try:
                    await symbol_service.add_symbol(symbol)
                    events.record("info", "signal_follow", f"{symbol} 不在标的池, 已自动添加")
                except symbol_service.PoolError as exc:
                    return self._follow_skip(key, parsed, "skipped", f"自动加池失败: {exc.detail}", mode)
            elif symbol not in set(symbol_service.ordered_symbols(enabled_only=True)):
                return self._follow_skip(key, parsed, "skipped", "标的在池内但已停用, 不自动启用", mode)

            filters = symbol_service.filters_for(symbol)
            if filters is None:
                return self._follow_skip(key, parsed, "skipped", "缺少交易规则(标的未就绪)", mode)
            broker = get_broker(mode)
            try:
                account = await broker.account()
            except Exception as exc:  # noqa: BLE001
                return self._follow_skip(key, parsed, "skipped", f"账户读取失败: {str(exc)[:120]}", mode)

            # 与轮询开仓同一套全局闸门: 交易时段 / 日内亏损 / 回撤 / 止损熔断 / 停机
            window_ok, window_why = risk_mod.trading_window_open(engine_cfg)
            if not window_ok:
                return self._follow_skip(key, parsed, "skipped", window_why, mode)
            global_gate = risk_mod.global_gates(account, engine_cfg, risk_cfg)
            if not global_gate.ok:
                return self._follow_skip(key, parsed, "skipped", global_gate.reason, mode)

            try:
                tickers = await binance_client.tickers([symbol])
            except (BinanceError, NoCredentials) as exc:
                return self._follow_skip(key, parsed, "skipped", f"取价失败: {str(exc)[:120]}", mode)
            ticker = tickers.get(symbol)
            if ticker is None or ticker.last <= 0:
                return self._follow_skip(key, parsed, "skipped", "行情暂不可用", mode)
            filters.last_price = ticker.last

            gate = risk_mod.symbol_gates(
                symbol, account, engine_cfg, risk_cfg,
                prices={symbol: ticker.last}, side=side,
            )
            if not gate.ok:
                return self._follow_skip(key, parsed, "skipped", gate.reason, mode)

            # 杠杆 = 原信号杠杆 +10x（没给杠杆就回落风控杠杆下限），再钳到全局杠杆下限/上限
            base_lev = signal_feed.follow_leverage(
                parsed["leverage"], risk_cfg.default_leverage, cfg.leverage_bonus
            )
            leverage = risk_mod.choose_leverage(
                symbol, engine_cfg, risk_cfg, filters, ai_leverage=float(base_lev)
            )
            ref = parsed["entry"] if parsed["entry"] > 0 else ticker.last
            if parsed["stop"] > 0:
                sl_pct = abs(ref - parsed["stop"]) / ref * 100
            else:
                sl_pct = cfg.default_sl_pct
            sl_pct = max(0.05, min(sl_pct, 20.0))
            tp_pct = (
                abs(parsed["tp"] - ref) / ref * 100 if parsed["tp"] > 0 else max(sl_pct * 1.6, sl_pct + 0.1)
            )

            # 资金完全按本系统来: 与轮询开仓共用账本
            self._margin.refresh(account.available)
            if cfg.sizing_mode == "margin_pct":
                # 默认: 按后台「单仓保证金占权益%」定仓。跟单的亏损由信号自带止损承担,
                # 不该再用"单笔风险"二次压缩 —— 否则止损给得宽的信号仓位会被压到很小
                pct = cfg.margin_pct_override if cfg.margin_pct_override > 0 else risk_cfg.margin_pct
                qty, info = qty_from_margin_pct(
                    equity=account.equity,
                    price=ticker.last,
                    leverage=leverage,
                    margin_pct=pct,
                    filters=filters,
                    available=self._margin.remaining,
                )
            else:
                qty, info = risk_mod.plan_position(
                    price=ticker.last, filters=filters, account=account,
                    cfg_risk=risk_cfg, sl_pct=sl_pct, leverage=leverage,
                    available=self._margin.remaining,
                )
            if qty <= 0:
                return self._follow_skip(key, parsed, "skipped", f"仓位不足: {info['note']}", mode)

            reserved = float(info.get("margin") or 0)
            rid = await self._margin.reserve_tracked(reserved)
            if not rid:
                return self._follow_skip(key, parsed, "skipped", "本轮可用保证金已被其它开仓占用", mode)

            sl_price = parsed["stop"] if parsed["stop"] > 0 else ticker.last * (
                1 - sl_pct / 100 if side == "long" else 1 + sl_pct / 100
            )
            tp_price = parsed["tp"] if parsed["tp"] > 0 else ticker.last * (
                1 + tp_pct / 100 if side == "long" else 1 - tp_pct / 100
            )
            sl_price = filters.align_price(sl_price)
            tp_price = filters.align_price(tp_price)

            if cfg.dry_run:
                await self._margin.settle(rid, opened=False)
                rid = 0
                self._finish_follow(
                    key, parsed, "dry_run", "演练模式: 只记录不下单", mode,
                    qty=qty, price=ticker.last, margin=reserved, leverage=leverage,
                    sl_pct=round(sl_pct, 4), tp_pct=round(tp_pct, 4), plan=info,
                )
                events.record(
                    "info", "signal_follow",
                    f"{symbol} {side} 演练: qty={qty} lev={leverage}x 保证金≈{reserved:.2f}U",
                )
                return {"ok": True, "status": "dry_run", "qty": qty, "leverage": leverage}

            try:
                res = await broker.open_position(
                    symbol=symbol, side=side, qty=qty, leverage=leverage,
                    sl_price=sl_price, tp_price=tp_price, filters=filters,
                    ai_confidence=0.0, ai_reason=f"外部信号跟单 {key}"[:800],
                    signal_score=0.0,
                    features={
                        "source": "signal_follow", "signal_id": key,
                        "signal": {k: v for k, v in parsed.items() if k != "raw"},
                        "plan": info, "leverage_bonus": cfg.leverage_bonus,
                    },
                )
            except Exception as exc:  # noqa: BLE001
                log.exception("信号跟单下单异常")
                await self._margin.settle(rid, opened=False)
                rid = 0
                return self._follow_skip(
                    key, parsed, "error", f"下单异常: {str(exc)[:150]}", mode,
                    qty=qty, price=ticker.last, margin=reserved, leverage=leverage,
                )
            if not res.ok:
                await self._margin.settle(rid, opened=False)
                rid = 0
                return self._follow_skip(
                    key, parsed, "error", f"下单失败: {res.message}"[:200], mode,
                    qty=qty, price=ticker.last, margin=reserved, leverage=leverage,
                )
            await self._margin.settle(rid, opened=True)
            rid = 0
            follow_opened = True
            self._finish_follow(
                key, parsed, "opened", f"跟单成交 @{res.price:.8g}", mode,
                qty=res.qty, price=res.price, margin=reserved, leverage=leverage,
                sl_pct=round(sl_pct, 4), tp_pct=round(tp_pct, 4),
            )
            events.record(
                "info", "signal_follow",
                f"{symbol} {side} 跟单 {res.qty} @{res.price:.8g} lev={leverage}x "
                f"(信号 {parsed['leverage']:g}x +{cfg.leverage_bonus}) 保证金≈{reserved:.2f}U",
            )
            return {"ok": True, "status": "opened", "qty": res.qty, "price": res.price, "leverage": leverage}
        except Exception as exc:  # noqa: BLE001
            log.exception("信号跟单处理异常")
            if rid:
                await self._margin.settle(rid, opened=False)
            return self._follow_skip(key, parsed, "error", f"跟单异常: {str(exc)[:150]}", mode)
        finally:
            # 成交后刻意保留同标的忙碌标记直到 TTL 自然过期: 异步落库还没写完时,
            # 下一轮轮询扫描不能对同一标的再开一单（90s 后或持仓可见即安全）。
            if not follow_opened:
                self._symbol_release(symbol)

    # ------------------------------------------------------------ 生命周期事件

    def _open_position(self, symbol: str, mode: str) -> PositionRecord | None:
        """取该标的当前 open 仓位，返回脱离 session 的副本。

        加仓/减仓/平仓/移止损都要先知道"现在有什么仓"，方向、均价、
        数量一律以本地账本为准，不信信号里写的方向。
        """
        with session_scope() as s:
            pos = (
                s.query(PositionRecord)
                .filter(
                    PositionRecord.symbol == symbol,
                    PositionRecord.status == "open",
                    PositionRecord.mode == mode,
                )
                .order_by(PositionRecord.opened_at.desc())
                .first()
            )
            if pos is not None:
                s.expunge(pos)
            return pos

    async def _follow_lifecycle(
        self,
        key: str,
        parsed: dict[str, Any],
        event: str,
        mode: str,
        cfg: Any,
        risk_cfg: Any,
    ) -> dict[str, Any]:
        """处理加仓 / 减仓 / 平仓 / 移止损。

        共同前提：本系统必须有该标的的持仓。没有持仓时不是"出错了"，
        而是这条频道消息对我们没有意义（我们没跟过这一单），直接跳过。
        """
        symbol = parsed["symbol"]
        pos = self._open_position(symbol, mode)
        if pos is None:
            return self._follow_skip(
                key, parsed, "skipped", f"本地无 {symbol} 持仓，无法执行 {event}", mode
            )

        filters = symbol_service.filters_for(symbol)
        if filters is None:
            return self._follow_skip(key, parsed, "skipped", "缺少交易规则(标的未就绪)", mode)
        broker = get_broker(mode)

        try:
            tickers = await binance_client.tickers([symbol])
        except (BinanceError, NoCredentials) as exc:
            return self._follow_skip(key, parsed, "skipped", f"取价失败: {str(exc)[:120]}", mode)
        ticker = tickers.get(symbol)
        if ticker is None or ticker.last <= 0:
            return self._follow_skip(key, parsed, "skipped", "行情暂不可用", mode)
        filters.last_price = ticker.last
        last = ticker.last

        if event == "close":
            return await self._lifecycle_close(key, parsed, pos, broker, mode, cfg)

        if event == "reduce":
            return await self._lifecycle_reduce(key, parsed, pos, broker, mode, cfg, filters)

        if event == "move_stop":
            return await self._lifecycle_move_stop(
                key, parsed, pos, broker, mode, cfg, filters, last
            )

        return await self._lifecycle_add(
            key, parsed, pos, broker, mode, cfg, risk_cfg, filters, last
        )

    async def _lifecycle_close(
        self, key, parsed, pos: PositionRecord, broker: Broker, mode: str, cfg: Any
    ) -> dict[str, Any]:
        if cfg.dry_run:
            self._finish_follow(
                key, parsed, "dry_run", "演练模式: 只记录不平仓", mode,
                qty=pos.qty, price=pos.mark_price, leverage=pos.leverage,
            )
            events.record(
                "info", "signal_follow",
                f"{pos.symbol} 演练: 收到平仓信号, 将平掉 {pos.qty:g}",
            )
            return {"ok": True, "status": "dry_run"}
        res = await broker.close_position(position=pos, reason="signal_close")
        if not res.ok:
            return self._follow_skip(key, parsed, "error", f"平仓失败: {res.message}"[:200], mode)
        self._finish_follow(
            key, parsed, "closed", f"跟单平仓 @{res.price:.8g}", mode,
            qty=res.qty, price=res.price, leverage=pos.leverage,
        )
        events.record(
            "info", "signal_follow",
            f"{pos.symbol} 跟单平仓 {res.qty:g} @{res.price:.8g}",
        )
        return {"ok": True, "status": "closed", "qty": res.qty, "price": res.price}

    async def _lifecycle_reduce(
        self, key, parsed, pos: PositionRecord, broker: Broker, mode: str, cfg: Any, filters
    ) -> dict[str, Any]:
        pct = float(parsed.get("reduce_pct") or 0.0)
        if pct <= 0:
            # 频道只说"减仓"没给比例：按一半处理，比"全平"保守，比"不动"有意义
            pct = 50.0
            why_pct = "未给比例, 按 50%"
        else:
            why_pct = f"{pct:g}%"
        pct = max(1.0, min(100.0, pct))
        qty = pos.qty * pct / 100.0
        # 数量对齐交给 close_position（它内部按步进取整并校验最小量）
        if qty <= 0:
            return self._follow_skip(key, parsed, "skipped", "减仓数量为 0", mode)
        if cfg.dry_run:
            self._finish_follow(
                key, parsed, "dry_run", f"演练模式: 只记录不平仓(减 {why_pct})", mode,
                qty=qty, price=pos.mark_price, leverage=pos.leverage,
            )
            events.record(
                "info", "signal_follow",
                f"{pos.symbol} 演练: 减仓 {why_pct}, 将平掉 {qty:g}",
            )
            return {"ok": True, "status": "dry_run", "qty": qty}
        res = await broker.close_position(
            position=pos, reason=f"signal_reduce_{pct:g}pct", qty=qty
        )
        if not res.ok:
            return self._follow_skip(key, parsed, "error", f"减仓失败: {res.message}"[:200], mode)
        self._finish_follow(
            key, parsed, "reduced", f"跟单减仓 {why_pct} @{res.price:.8g}", mode,
            qty=res.qty, price=res.price, leverage=pos.leverage,
        )
        events.record(
            "info", "signal_follow",
            f"{pos.symbol} 跟单减仓 {why_pct} 平掉 {res.qty:g} @{res.price:.8g}",
        )
        return {"ok": True, "status": "reduced", "qty": res.qty, "price": res.price}

    async def _lifecycle_move_stop(
        self, key, parsed, pos: PositionRecord, broker: Broker, mode: str, cfg: Any, filters, last: float
    ) -> dict[str, Any]:
        if parsed.get("stop_to_breakeven"):
            target = float(pos.entry_price or 0)
            why = "移到入场价(保本)"
        else:
            target = float(parsed.get("stop") or 0)
            why = f"移到 {target:g}" if target > 0 else ""
        if target <= 0:
            return self._follow_skip(key, parsed, "skipped", "移止损信号没有可用目标价", mode)

        target = filters.align_price(target)
        # 止损必须落在现价的安全侧，否则挂上去立刻触发市价平仓
        if pos.side == "long" and target >= last:
            return self._follow_skip(
                key, parsed, "skipped",
                f"多单止损 {target:g} 不低于现价 {last:g}, 挂上会立即触发", mode,
            )
        if pos.side == "short" and target <= last:
            return self._follow_skip(
                key, parsed, "skipped",
                f"空单止损 {target:g} 不高于现价 {last:g}, 挂上会立即触发", mode,
            )

        if cfg.dry_run:
            self._finish_follow(
                key, parsed, "dry_run", f"演练模式: 只记录不移动止损({why})", mode,
                qty=pos.qty, price=target, leverage=pos.leverage,
            )
            events.record(
                "info", "signal_follow",
                f"{pos.symbol} 演练: 止损 {why}, 当前止损 {pos.sl_price or '-'}",
            )
            return {"ok": True, "status": "dry_run", "price": target}
        res = await broker.amend_stop(
            pos.symbol, pos.side, target, float(pos.qty), tp_price=float(pos.tp_price or 0)
        )
        if not res.ok:
            return self._follow_skip(key, parsed, "error", f"移止损失败: {res.message}"[:200], mode)
        self._finish_follow(
            key, parsed, "stop_moved", f"止损 {why}", mode,
            qty=pos.qty, price=target, leverage=pos.leverage,
        )
        events.record("info", "signal_follow", f"{pos.symbol} 跟单止损 {why}")
        return {"ok": True, "status": "stop_moved", "price": target}

    async def _lifecycle_add(
        self, key, parsed, pos: PositionRecord, broker: Broker, mode: str, cfg: Any,
        risk_cfg: Any, filters, last: float,
    ) -> dict[str, Any]:
        side = parsed.get("side") or pos.side
        if side != pos.side:
            return self._follow_skip(
                key, parsed, "skipped",
                f"加仓方向 {side} 与现有持仓 {pos.side} 相反, 已忽略", mode,
            )
        # 只挡"引擎真的正在给同标的开仓"这一种并发：那种情况下再加一笔，
        # 额度与均价都可能算重。这里刻意不用 _symbol_acquire —— 它的 90 秒
        # TTL 在开仓成功后不会释放，而频道的"开仓 / 现价加仓"经常前后脚发出，
        # 用那把锁会把正常的加仓信号全部吃掉。
        # 跟单本身由 _follow_lock 串行，不存在两条加仓互相覆盖的可能。
        task = self._entry_tasks.get(pos.symbol)
        if task is not None and not task.done():
            return self._follow_skip(
                key, parsed, "skipped", "引擎正在同标的开仓, 加仓稍后重试", mode
            )
        return await self._lifecycle_add_locked(
            key, parsed, pos, broker, mode, cfg, risk_cfg, filters, last
        )

    async def _lifecycle_add_locked(
        self, key, parsed, pos: PositionRecord, broker: Broker, mode: str, cfg: Any,
        risk_cfg: Any, filters, last: float,
    ) -> dict[str, Any]:
        try:
            account = await broker.account()
        except Exception as exc:  # noqa: BLE001
            return self._follow_skip(key, parsed, "skipped", f"账户读取失败: {str(exc)[:120]}", mode)

        self._margin.refresh(account.available)
        pct = cfg.margin_pct_override if cfg.margin_pct_override > 0 else risk_cfg.margin_pct
        if cfg.sizing_mode == "margin_pct":
            qty, info = qty_from_margin_pct(
                equity=account.equity,
                price=last,
                leverage=float(pos.leverage or 1),
                margin_pct=pct,
                filters=filters,
                available=self._margin.remaining,
            )
        else:
            sl_pct = cfg.default_sl_pct
            ref = parsed["entry"] if parsed["entry"] > 0 else last
            if parsed["stop"] > 0:
                sl_pct = max(0.05, min(abs(ref - parsed["stop"]) / ref * 100, 20.0))
            qty, info = risk_mod.plan_position(
                price=last, filters=filters, account=account, cfg_risk=risk_cfg,
                sl_pct=sl_pct, leverage=float(pos.leverage or 1),
                available=self._margin.remaining,
            )
        if qty <= 0:
            return self._follow_skip(key, parsed, "skipped", f"加仓数量不足: {info['note']}", mode)

        # 止损/止盈沿用原仓位的设定，信号给了新的就用新的
        sl_price = float(parsed["stop"]) if parsed["stop"] > 0 else float(pos.sl_price or 0)
        tp_price = float(parsed["tp"]) if parsed["tp"] > 0 else float(pos.tp_price or 0)
        if sl_price <= 0:
            # 绝不留裸仓：加完之后没有止损是最危险的状态
            sl_pct = cfg.default_sl_pct
            sl_price = last * (1 - sl_pct / 100 if pos.side == "long" else 1 + sl_pct / 100)
        sl_price = filters.align_price(sl_price)
        if tp_price > 0:
            tp_price = filters.align_price(tp_price)

        reserved = float(info.get("margin") or 0)
        rid = await self._margin.reserve_tracked(reserved)
        if not rid:
            return self._follow_skip(key, parsed, "skipped", "可用保证金不足, 加仓未执行", mode)

        if cfg.dry_run:
            await self._margin.settle(rid, opened=False)
            self._finish_follow(
                key, parsed, "dry_run", "演练模式: 只记录不加仓", mode,
                qty=qty, price=last, margin=reserved, leverage=pos.leverage,
            )
            events.record(
                "info", "signal_follow",
                f"{pos.symbol} 演练: 加仓 {qty:g} @{last:.8g} 保证金≈{reserved:.2f}U",
            )
            return {"ok": True, "status": "dry_run", "qty": qty}

        try:
            res = await broker.add_to_position(
                position=pos, qty=qty, filters=filters,
                sl_price=sl_price, tp_price=tp_price, reason=f"外部信号加仓 {key}",
            )
        except Exception as exc:  # noqa: BLE001
            log.exception("信号跟单加仓异常")
            await self._margin.settle(rid, opened=False)
            return self._follow_skip(key, parsed, "error", f"加仓异常: {str(exc)[:150]}", mode)
        if not res.ok:
            await self._margin.settle(rid, opened=False)
            return self._follow_skip(key, parsed, "error", f"加仓失败: {res.message}"[:200], mode)
        await self._margin.settle(rid, opened=True)
        self._finish_follow(
            key, parsed, "added", res.message[:200], mode,
            qty=res.qty, price=res.price, margin=reserved, leverage=pos.leverage,
        )
        events.record(
            "info", "signal_follow",
            f"{pos.symbol} 跟单加仓 {res.qty:g} @{res.price:.8g} 保证金≈{reserved:.2f}U",
        )
        return {"ok": True, "status": "added", "qty": res.qty, "price": res.price}

    def signal_follow_status(self) -> dict[str, Any]:
        cfg = get_signal_follow()
        settings = cfg.model_dump()
        if settings.get("ingest_token"):
            settings["ingest_token"] = "******"  # 观测接口不回传直推密钥
        key = self._follow_cursor_key or (
            self._follow_offset_key(self._follow_path) if self._follow_path else ""
        )
        with session_scope() as s:
            recent = (
                s.query(SignalFollow).order_by(SignalFollow.created_at.desc()).limit(20).all()
            )
            counts = {
                status: s.query(SignalFollow).filter(SignalFollow.status == status).count()
                for status in (
                    "opened", "added", "reduced", "closed", "stop_moved",
                    "dry_run", "skipped", "error", "duplicate", "processing",
                )
            }
            items = [
                {
                    "id": r.id, "signal_id": r.signal_id, "symbol": r.symbol, "side": r.side,
                    "status": r.status, "reason": r.reason, "leverage": r.leverage,
                    "qty": r.qty, "price": r.price, "margin": r.margin,
                    "mode": r.mode, "created_at": r.created_at,
                }
                for r in recent
            ]
        return {
            "settings": settings,
            "ingest_token_set": bool(cfg.ingest_token),
            "running": bool(self._follow_task and not self._follow_task.done()),
            "engine_enabled": get_engine().enabled,
            "path": self._follow_path,
            "path_exists": bool(
                self._follow_path
                and (os.path.isfile(self._follow_path) or signal_feed.is_url(self._follow_path))
            ),
            "source_kind": "url" if signal_feed.is_url(self._follow_path) else "file",
            "offset": state.get_int(key, 0) if key else 0,
            "inflight_margin": round(self._margin.inflight, 6),
            "last_poll": int(self._follow_last_poll),
            "last_error": self._follow_last_error,
            "counts": counts,
            "items": items,
        }

    async def _open_entry(
        self,
        broker: Broker,
        sig,
        ticker,
        account,
        engine_cfg: EngineSettings,
        risk_cfg: RiskSettings,
        strat_cfg: StrategySettings,
        signal_id: int,
    ) -> None:
        async with self._sem:
            try:
                await self._do_open(broker, sig, ticker, account, engine_cfg, risk_cfg, strat_cfg, signal_id)
            except Exception as exc:  # noqa: BLE001
                log.exception("开仓任务异常")
                self._update_signal_reject(signal_id, f"开仓异常: {exc}"[:200])

    async def _do_open(self, broker, sig, ticker, account, engine_cfg, risk_cfg, strat_cfg, signal_id) -> None:
        # 交易规格只认标的池 (添加时已用币安合约表校验), 不再依赖行情模块
        filters = symbol_service.filters_for(sig.symbol)
        if filters is None:
            self._update_signal_reject(signal_id, "标的不在标的池中, 缺少交易规则")
            return
        filters.last_price = ticker.last
        filters.quote_volume_24h = ticker.quote_volume

        # 盘口点差闸门 (freqtrade SpreadFilter 思路): 超短线利润只有千分之几,
        # 点差过宽时一次往返就能把利润吃光, 宁可不做.
        if strat_cfg.max_spread_bps > 0:
            try:
                books = await binance_client.book_tickers([sig.symbol])
            except (BinanceError, NoCredentials) as exc:
                books = {}
                log.warning("盘口取价失败, 本轮跳过点差闸门: %s", str(exc)[:120])
            book = books.get(sig.symbol)
            if book:
                bid, ask = book
                mid = (bid + ask) / 2
                if mid > 0:
                    spread_bps = (ask - bid) / mid * 10000
                    if spread_bps > strat_cfg.max_spread_bps:
                        self._update_signal_reject(
                            signal_id,
                            f"盘口点差 {spread_bps:.1f}bps > 上限 {strat_cfg.max_spread_bps:.1f}bps, 超短线放弃",
                        )
                        return

        ai_cfg = get_ai()
        direction = sig.direction
        leverage = risk_mod.choose_leverage(
            sig.symbol, engine_cfg, risk_cfg, filters,
            strategy_range=(strat_cfg.leverage_min, strat_cfg.leverage_max),
        )
        sl_pct, tp_pct = sig.sl_pct, sig.tp_pct
        ai_meta: dict[str, Any] = {}
        ai_used = False
        injected_ids: list[int] = []

        from ..ai.client import providers_available

        ai_ready = providers_available() > 0
        if ai_cfg.enabled and not ai_ready:
            ai_meta = {"ok": False, "error": "未配置可用模型, 本轮回退纯量化决策", "degraded": True}
            if time.time() - self._ai_warn_ts > 600:
                self._ai_warn_ts = time.time()
                events.record(
                    "warn",
                    "ai",
                    "AI 已开启但未配置可用模型, 当前回退为纯量化决策 (可在「AI 模型」中接入)",
                )

        if ai_cfg.enabled and ai_ready:
            ctx = self._build_ai_context(sig, ticker, account, engine_cfg, risk_cfg, filters)
            injected_ids = [int(x) for x in ctx.get("injected_mantra_ids", [])]
            ai_used = True
            try:
                outcome = await asyncio.wait_for(decide(ctx), timeout=ai_cfg.timeout_sec + 15)
            except asyncio.TimeoutError:
                self._update_signal_reject(signal_id, f"AI 决策超时 ({ai_cfg.timeout_sec}s)")
                return
            ai_meta = outcome.as_dict()
            d = outcome.decision
            with session_scope() as s:
                row = s.get(Signal, signal_id)
                if row:
                    row.ai_action = d.action
                    row.ai_confidence = d.confidence
                    row.ai_reason = (d.reason or outcome.error)[:900]
            if not outcome.ok:
                self._update_signal_reject(signal_id, f"AI 不可用({outcome.error[:80]}), 按 fail-closed 跳过")
                return
            if ai_cfg.require_agreement:
                # 「AI 议会」多数票权重不足时不开仓：这个开关以前只存在设置页，没有任何代码读它，
                # 结果是三个席位吵成一团也会照着加权后的方向下单。
                blocked = ai_agreement_reject(True, outcome.agreement)
                if blocked:
                    self._update_signal_reject(signal_id, blocked)
                    return
            if d.action == "hold":
                self._update_signal_reject(signal_id, f"AI 否决: {d.reason[:150]}")
                return
            if d.action == "close":
                self._update_signal_reject(signal_id, "AI 建议观望")
                return
            ai_dir = "long" if d.action == "open_long" else "short"
            # 策略方向锁定优先于 AI 反向覆盖：策略只做多/只做空时，反向提案直接拒绝
            strategy = ctx.get("strategy") or {}
            side_lock = str(((strategy.get("filters") or {}).get("side")) or "any")
            if side_lock in ("long", "short") and ai_dir != side_lock:
                self._update_signal_reject(
                    signal_id,
                    f"策略「{strategy.get('name') or ''}」只允许"
                    f"{'做多' if side_lock == 'long' else '做空'}，AI 提案方向相反",
                )
                return
            if ai_dir != direction:
                if not ai_cfg.allow_ai_override:
                    self._update_signal_reject(signal_id, f"AI 方向({ai_dir})与量化信号({direction})冲突")
                    return
                direction = ai_dir
                events.record("warn", "ai", f"{sig.symbol} AI 反向覆盖量化信号 -> {direction}", ai_meta)
            if d.confidence < ai_cfg.min_confidence:
                self._update_signal_reject(
                    signal_id, f"AI 置信度 {d.confidence:.2f} < {ai_cfg.min_confidence}"
                )
                return
            if d.leverage > 0:
                # 模型（含注入心法）给出的杠杆只是区间内建议值，
                # 统一由风控全局「杠杆下限/上限」钳制；不再走 AI 模型页的独立上限
                leverage = risk_mod.choose_leverage(
                    sig.symbol, engine_cfg, risk_cfg, filters, d.leverage,
                    strategy_range=(strat_cfg.leverage_min, strat_cfg.leverage_max),
                )
            if d.sl_pct > 0:
                sl_pct = min(max(0.12, d.sl_pct), 3.0)
            if d.tp_pct > 0:
                tp_pct = min(max(sl_pct * 1.05, d.tp_pct), 8.0)

        # 波动率目标: ATR% 高于目标就缩仓, 反之放大 (关闭时 risk_scale=1)
        risk_scale = risk_mod.vol_risk_scale(
            risk_cfg.vol_target_atr_pct, float(sig.features.get("atr_pct") or 0.0)
        )
        qty, info = risk_mod.plan_position(
            price=ticker.last, filters=filters, account=account,
            cfg_risk=risk_cfg, sl_pct=sl_pct, leverage=leverage, risk_scale=risk_scale,
            available=self._margin.remaining,
        )
        if qty <= 0:
            self._update_signal_reject(signal_id, f"仓位不足: {info['note']}")
            return

        # 串行占额度：并发任务只能用到前一笔剩下的可用保证金
        reserved = float(info.get("margin") or 0.0)
        rid = await self._margin.reserve_tracked(reserved)
        if not rid:
            self._update_signal_reject(
                signal_id,
                f"本轮可用保证金已被其它开仓占用（剩余 {self._margin.remaining:.2f} USDT），按 fail-closed 跳过",
            )
            return

        sl_price = ticker.last * (1 - sl_pct / 100) if direction == "long" else ticker.last * (1 + sl_pct / 100)
        tp_price = ticker.last * (1 + tp_pct / 100) if direction == "long" else ticker.last * (1 - tp_pct / 100)
        sl_price = filters.align_price(sl_price)
        tp_price = filters.align_price(tp_price)

        try:
            res = await broker.open_position(
                symbol=sig.symbol,
                side=direction,
                qty=qty,
                leverage=leverage,
                sl_price=sl_price,
                tp_price=tp_price,
                filters=filters,
                ai_confidence=ai_meta.get("confidence", 0.0),
                ai_reason=(ai_meta.get("reason") or "")[:800],
                signal_score=sig.score,
                features={**sig.features, "ai": ai_meta, "plan": info},
            )
        except Exception:
            await self._margin.settle(rid, opened=False)
            raise
        if not res.ok:
            # 成交前失败 → 归还预留额度；成交后保留到下一轮 begin_cycle
            await self._margin.settle(rid, opened=False)
            # broker 的错误文案已带「下单失败 N:」，不要拼成「下单失败: 下单失败 N:」
            detail = res.message if res.message.startswith("下单失败") else f"下单失败: {res.message}"
            self._update_signal_reject(signal_id, detail[:200])
            events.record("error", "trade", f"{sig.symbol} 开仓失败: {res.message}"[:250])
            return
        await self._margin.settle(rid, opened=True)
        with session_scope() as s:
            row = s.get(Signal, signal_id)
            if row:
                row.decision = "opened"
                row.reject_reason = ""
        events.record(
            "info", "trade",
            f"{sig.symbol} {direction} 开仓 @{res.price:.8g} qty={res.qty} lev={leverage}x 止损 {sl_pct:.2f}% 止盈 {tp_pct:.2f}%",
            {
                "symbol": sig.symbol, "side": direction, "price": res.price, "qty": res.qty,
                "leverage": leverage, "sl_pct": sl_pct, "tp_pct": tp_pct, "mode": broker.mode,
                "ai": ai_meta, "plan": info,
                # 决策时快照: 用于「采用心法 vs 未采用」的观察性对照，复盘时还可回写命中率
                "ai_used": ai_used, "injected_mantra_ids": injected_ids,
            },
        )

    def _build_ai_context(self, sig, ticker, account, engine_cfg, risk_cfg, filters) -> dict[str, Any]:
        ai_cfg = get_ai()
        mantras: list[dict[str, Any]] = []
        injected_ids: list[int] = []
        if ai_cfg.inject_mantras:
            lessons = mantra_store.rank_lessons()
            # 只传标题与健康度，**不传规则原文**：心法文本里可能含「用 Nx 杠杆」之类的历史表述，
            # 开仓杠杆必须只由风控全局上下限决定，不能让模型照着心法给杠杆。
            mantras = [{"title": m["title"], "health": m["health"]} for m in lessons]
            # 只记 id，不把 id 塞进 prompt（快照用于事后「采用/未采用」对照）
            injected_ids = [int(m["id"]) for m in lessons if m.get("id")]
        news_items = news_service.digest(6) if ai_cfg.inject_news else []
        positions = risk_mod.open_positions()
        today_pnl = 0.0
        day_start = state.get_float("day_start_equity", account.equity)
        if day_start > 0:
            today_pnl = (account.equity - day_start) / day_start * 100
        lev_upper = max(1, min(int(risk_cfg.max_leverage), int(filters.max_leverage or 100), 100))
        lev_lower = max(1, min(int(risk_cfg.default_leverage), lev_upper))

        # 策略：按适用范围/优先级挑一条启用策略，交给 AI 作为分析依据；没有则 None（默认分析）
        from ..services import strategy_profiles as strategy_service

        strategy = strategy_service.pick_for(sig.symbol)
        applied = str((strategy or {}).get("name") or "")
        if applied != state.get("strategy_applied", ""):
            state.set("strategy_applied", applied)
        return {
            "symbol": sig.symbol,
            "price": ticker.last,
            "score": sig.score,
            "side_hint": sig.direction,
            "quant_reasons": sig.reasons,
            "features": sig.features,
            "klines": sig.features.get("_bars", []),
            "funding": sig.features.get("funding", 0.0),
            "change_24h": ticker.change_pct,
            "quote_volume": ticker.quote_volume,
            "account": {
                "equity": round(account.equity, 6),
                "available": round(account.available, 6),
                "open_positions": len(positions),
                "positions": [
                    {"symbol": p.symbol, "side": p.side, "qty": p.qty, "entry": p.entry_price, "sl": p.sl_price, "tp": p.tp_price}
                    for p in positions[:6]
                ],
                "today_pnl_pct": round(today_pnl, 3),
            },
            "constraints": {
                "min_leverage": lev_lower,
                "max_leverage": lev_upper,
                "leverage_range": f"{lev_lower}-{lev_upper}",
                "risk_per_trade_pct": risk_cfg.risk_per_trade_pct,
                "margin_pct": risk_cfg.margin_pct,
                "min_confidence": ai_cfg.min_confidence,
                "effective_min_notional": filters.effective_min_notional(ticker.last),
                "fee_rate_taker": risk_cfg.fee_rate_taker,
            },
            "news": news_items,
            "strategy": strategy,
            "mantras": mantras,
            "injected_mantra_ids": injected_ids,
        }

    async def _health_check(self) -> None:
        """实时健康检查（每 60s 一次）：WS 断线、引擎心跳停滞。告警去重防刷屏。"""
        now = time.time()
        if now - self._last_health_check < 60:
            return
        self._last_health_check = now

        # 1) Ticker WebSocket 断线 → 行情走 REST 兜底（无盘口，点差闸门失效）
        try:
            from ..exchange.ticker_stream import ticker_hub

            if not ticker_hub.connected:
                if now - self._last_ws_warn > 600:
                    self._last_ws_warn = now
                    events.record(
                        "warn", "health",
                        "Ticker WebSocket 已断开，行情走 REST 兜底（无盘口，点差闸门失效）",
                    )
        except Exception:  # noqa: BLE001
            pass

        # 2) 引擎心跳：超过 5 分钟没有新信号落库，说明扫描可能卡死
        try:
            with session_scope() as s:
                row = (
                    s.query(Signal)
                    .order_by(Signal.ts.desc())
                    .first()
                )
                last_ts = int(row.ts) if row else 0
            if last_ts and now - last_ts > 300:
                events.record(
                    "warn", "health",
                    f"引擎已 {int(now - last_ts)} 秒没有产生新信号，扫描可能停滞",
                )
        except Exception:  # noqa: BLE001
            pass

    async def _check_liquidation_risk(self, broker: Broker, risk_cfg: Any) -> None:
        """强平价监控：标记价逼近强平价时告警，并写进 engine_events。

        高杠杆短线的尾部风险是爆仓，而不是止损多打几笔。这个检查每轮都跑，
        但只在距离低于阈值时才告警，用状态去重避免刷屏。
        """
        warn_pct = getattr(risk_cfg, "liquidation_warn_pct", 0.0)
        if warn_pct <= 0:
            return
        try:
            positions = await broker.positions()
        except Exception:  # noqa: BLE001
            return
        for p in positions:
            liq = float(getattr(p, "liquidation_price", 0) or 0)
            mark = float(getattr(p, "mark_price", 0) or 0)
            if liq <= 0 or mark <= 0:
                continue
            dist = abs(mark - liq) / mark * 100
            if dist < warn_pct:
                key = f"liq_{p.symbol}_{p.side}"
                last_warn = self._last_liq_warn.get(key, 0)
                now = time.time()
                if now - last_warn < 300:  # 5 分钟内不重复
                    continue
                self._last_liq_warn[key] = now
                events.record(
                    "warn", "risk",
                    f"{p.symbol} {p.side} 距强平价仅 {dist:.1f}%（强平价 {liq:.5g}）",
                    {"symbol": p.symbol, "side": p.side, "mark": mark,
                     "liquidation_price": liq, "distance_pct": round(dist, 2)},
                )

    async def _sample_equity(self, account) -> None:
        now = int(time.time())
        self._cleanup_if_due(now)
        self._dlq_if_due(now)
        self._digest_if_due(now)
        self._review_backfill_if_due(now)
        last = state.get_int("last_equity_sample", 0)
        if now - last < 60:
            return
        state.set("last_equity_sample", str(now))
        with session_scope() as s:
            s.add(
                EquityPoint(
                    ts=now,
                    mode=get_engine().mode,
                    wallet=account.wallet,
                    available=account.available,
                    unrealized=account.unrealized,
                    margin_used=account.margin_used,
                    positions=len(risk_mod.open_positions()),
                )
            )

    def _cleanup_if_due(self, now: int) -> None:
        """每小时清理一次历史数据.

        清理放到后台任务 + 分批删除: 一次性 DELETE 上千行会长时间持有写锁,
        在 WAL 下可能把同进程的读请求拖到 busy_timeout (实测偶发 30s 卡顿).
        """
        if now - self._last_cleanup < 3600:
            return
        if self._started_at and now - self._started_at < 60:
            return
        self._last_cleanup = now
        asyncio.create_task(self._cleanup_async(now))

    def _digest_if_due(self, now: int) -> None:
        """每分钟检查一次告警摘要是否到点 (真正间隔由 alerts 内部判断)."""
        if now - self._last_digest < 60:
            return
        self._last_digest = now
        asyncio.create_task(self._flush_digest())

    async def _flush_digest(self) -> None:
        from ..services import alerts

        try:
            await alerts.flush_digest()
        except Exception as exc:  # noqa: BLE001
            log.warning("告警摘要发送失败: %s", exc)

    def _dlq_if_due(self, now: int) -> None:
        """每 10 分钟自动补偿一次死信 (最多 3 次/条)."""
        if now - self._last_dlq < 600:
            return
        if self._started_at and now - self._started_at < 120:
            return
        self._last_dlq = now
        asyncio.create_task(self._dlq_async())

    async def _dlq_async(self) -> None:
        from ..services.dead_letter import auto_replay

        try:
            stats = await asyncio.to_thread(auto_replay, 20)
            if stats["replayed"] or stats["failed"]:
                log.info("死信自动补偿: %s", stats)
        except Exception as exc:  # noqa: BLE001
            log.warning("死信自动补偿失败: %s", exc)

    async def _cleanup_async(self, now: int) -> None:
        """按保留策略归档并清理历史数据 (在线程里跑, 不阻塞事件循环)."""
        from ..services.retention import purge_expired

        try:
            stats = await asyncio.to_thread(purge_expired, now)
            if any(stats.values()):
                log.info("历史数据清理: %s", stats)
        except Exception as exc:  # noqa: BLE001
            log.warning("数据清理失败: %s", exc)

    # ----------------------------------------------------------------- status
    async def status(self) -> dict[str, Any]:
        cfg = get_engine()
        mode = normalize_mode(cfg.mode)
        broker = get_broker(mode)
        now = int(time.time())
        # 账户与仓位并行拉取：两者在同一时刻取样，持仓行与顶部 KPI 才不会差几秒
        account_res, venue_res = await asyncio.gather(
            broker.account(), broker.positions(), return_exceptions=True
        )
        if isinstance(account_res, Exception):
            # 交易所临时报错（限频/网络抖动）不能把整个账户面板刷成 0:
            # 保留上一轮成功读到的权益与浮盈亏，并标记 stale 让前端提示
            account_d = {
                **(
                    dict(self._last_account)
                    if self._last_account
                    else {"equity": 0, "wallet": 0, "available": 0, "unrealized": 0, "margin_used": 0}
                ),
                "error": str(account_res)[:200],
                "stale": True,
            }
        else:
            account_d = account_res.as_dict()
        venue_map: dict[str, Any] = {}
        if not isinstance(venue_res, Exception):
            venue_map = {vp.symbol: vp for vp in venue_res}
        with session_scope() as s:
            # 只展示当前交易环境的仓位, 模拟盘与实盘互不串台
            positions = (
                s.query(PositionRecord)
                .filter(PositionRecord.status == "open", PositionRecord.mode == mode)
                .all()
            )
            pos_d = []
            for p in positions:
                view = {
                    "id": p.id,
                    "symbol": p.symbol,
                    "side": p.side,
                    "qty": p.qty,
                    "entry_price": p.entry_price,
                    "mark_price": p.mark_price,
                    "leverage": p.leverage,
                    "margin": p.margin,
                    "sl_price": p.sl_price,
                    "tp_price": p.tp_price,
                    "peak_price": p.peak_price,
                    "opened_at": p.opened_at,
                    **position_view.derived(p),
                    "ai_confidence": p.ai_confidence,
                    "ai_reason": p.ai_reason,
                    "mode": p.mode,
                    "status": p.status,
                }
                vp = venue_map.get(p.symbol)
                if vp is not None:
                    # 与 account.totalUnrealizedProfit 同一次请求取样，避免两个面板不同步
                    view = position_view.merge_venue(
                        view,
                        qty=float(p.qty or 0),
                        margin=float(p.margin or 0),
                        mark_price=vp.mark_price,
                        unrealized=vp.unrealized,
                        ts=now,
                    )
                pos_d.append(view)
            trades = (
                s.query(TradeRecord)
                .filter(TradeRecord.mode == mode)
                .order_by(TradeRecord.closed_at.desc())
                .limit(200)
                .all()
            )
            order_rows = (
                s.query(OrderRecord.latency_ms)
                .filter(OrderRecord.mode == mode, OrderRecord.latency_ms > 0)
                .order_by(OrderRecord.ts.desc())
                .limit(100)
                .all()
            )
        wins = sum(1 for t in trades if t.pnl > 0)
        realized = sum(t.pnl for t in trades)
        latencies = sorted(ms for (ms,) in order_rows if ms > 0)
        slippages = [t.slippage_bps for t in trades if t.slippage_bps]

        def _pct(arr: list[float], q: float) -> float:
            if not arr:
                return 0.0
            idx = min(len(arr) - 1, int(len(arr) * q))
            return round(arr[idx], 2)

        execution = {
            "order_count": len(latencies),
            "latency_p50_ms": _pct(latencies, 0.50),
            "latency_p95_ms": _pct(latencies, 0.95),
            "avg_slippage_bps": round(sum(slippages) / len(slippages), 2) if slippages else 0.0,
        }
        try:
            from ..services.dead_letter import summary as dlq_summary

            dead_letters = await asyncio.to_thread(dlq_summary)
        except Exception:  # noqa: BLE001
            dead_letters = {"pending": 0, "stuck": 0, "last_ts": 0}
        heartbeat = 0
        try:
            from ..config import paths

            heartbeat = int(paths.HEARTBEAT.read_text(encoding="utf-8").splitlines()[0])
        except (OSError, ValueError, IndexError):
            heartbeat = 0
        from ..services import strategy_profiles as strategy_service

        try:
            strategy_stats = strategy_service.stats()
        except Exception as exc:  # noqa: BLE001
            log.warning("策略统计失败: %s", exc)
            strategy_stats = {"total": 0, "enabled": 0, "default": True, "by_side": {}}
        return {
            "enabled": cfg.enabled,
            "mode": mode,
            # 交易环境: demo=币安模拟盘(测试网) / live=币安实盘(主网)
            "env": mode,
            "env_label": "模拟盘 · 币安测试网" if mode == "demo" else "实盘 · 币安主网",
            "running": self.running,
            "heartbeat_age": int(time.time()) - heartbeat if heartbeat else -1,
            "uptime": int(time.time()) - self._started_at if self._started_at else 0,
            "last_cycle_ts": state.get_int("last_cycle_ts", 0),
            "last_cycle_ms": state.get_int("last_cycle_ms", 0),
            "last_error": state.get("last_error"),
            "halted": state.normalize_halted(state.get("halted")),
            "halted_reason": state.get("loss_halt_until"),
            "account": account_d,
            "positions": pos_d,
            "stats": {
                "trades": len(trades),
                "win_rate": round(wins / len(trades) * 100, 2) if trades else 0.0,
                "realized_pnl": round(realized, 6),
                "consecutive_losses": state.get_int("consecutive_losses", 0),
                "cooldowns": state.active_cooldowns(int(time.time())),
                "stoploss_guard_remaining": max(0, int(state.get_float("sl_guard_until", 0) - time.time())),
                "execution": execution,
                "dead_letters": dead_letters,
            },
            "ai_ready": self._ai_ready(),
            # 交易所限频观测: 前端据此提示「冷却剩余 Ns」，而不是反复弹 -1003 错误
            "rate_limit": {
                "banned": binance_client.weight.banned,
                # 本地试探倒计时；交易所声明的长封禁单独给出，避免误导为「还有 Ns 就恢复」
                "ban_remaining": int(binance_client.weight.ban_remaining()),
                "declared_until": int(binance_client.weight.ban_declared_until),
                "declared_remaining": int(binance_client.weight.ban_declared_remaining()),
                "declared_label": (
                    time.strftime("%H:%M", time.localtime(binance_client.weight.ban_declared_until))
                    if binance_client.weight.ban_declared_until > 0
                    else ""
                ),
                "used_weight": binance_client.weight.used_weight,
                "capacity": binance_client.weight.capacity,
            },
            # 策略统计：未配置时 default=true，表示 AI 走通用分析（默认）
            "strategy": {
                **strategy_stats,
                "last_applied": state.get("strategy_applied", ""),
            },
            # 交易模式体检（双向持仓 + 逐仓）：最近一次探测结果
            "trade_mode": __import__("json").loads(state.get("trade_mode", "{}") or "{}"),
            "cycle_timings": __import__("json").loads(state.get("cycle_timings", "{}") or "{}"),
        }

    @staticmethod
    def _ai_ready() -> bool:
        from ..ai.client import providers_available

        return providers_available() > 0


def state_file():
    from ..config import paths

    return paths.LOCK


engine = TradingEngine()
