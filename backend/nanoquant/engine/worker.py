"""交易引擎主循环.

节奏拆分:
- 每轮必做: 同步账户 -> 管理在场仓位 (止损/止盈/移动/超时) -> 权益采样
- 每轮择优: 扫描标的 -> 量化信号 -> 风控闸门 -> 交给独立的 AI 评估任务
AI 评估在后台任务中执行 (带并发闸), 绝不阻塞仓位管理与止损监控.

单实例: 文件锁 + 心跳文件; 面板读心跳判断"到底有没有在跑".
"""
from __future__ import annotations

import asyncio
import os
import time
from typing import Any

from ..ai.decision import decide
from ..db import session_scope
from ..exchange.broker import Broker, get_broker, record_external_position
from ..exchange.client import BinanceError, NoCredentials
from ..exchange.client import client as binance_client
from ..logging_setup import get_logger
from ..models import EquityPoint, OrderRecord, PositionRecord, Signal, TradeRecord
from ..news import service as news_service
from ..services import events, position_view, state
from ..services import symbols as symbol_service
from ..services.settings import (
    EngineSettings,
    RiskSettings,
    StrategySettings,
    get_ai,
    get_engine,
    get_evolution,
    get_news,
    get_risk,
    get_strategy,
    normalize_mode,
)
from ..evolution import mantra as mantra_store
from ..evolution import review as evolution_review
from . import risk as risk_mod
from .margin import MarginLedger
from .strategy import build_features, detect_exit, partial_close_qty, partial_tp_r, score_signal

log = get_logger("engine")

MAX_TASKS = 2
#: 同一标的、同一方向/拒绝原因的 flat 信号在这个窗口内合并成一行 (repeats 计数)
SIGNAL_DEDUP_SEC = 60


class TradingEngine:
    def __init__(self) -> None:
        self._task: asyncio.Task | None = None
        self._stop = asyncio.Event()
        self._entry_tasks: dict[str, asyncio.Task] = {}
        self._signal_dedup: dict[tuple, int] = {}
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
        self._task = asyncio.create_task(self._loop(), name="nanoquant-engine")
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

        await timed("positions", self._manage_positions(broker, strat_cfg, engine_cfg))
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
            state.set("halted", gate.reason if "熔断" in gate.reason else "")
            state.set("cycle_timings", __import__("json").dumps(timings))
            return {"skipped": gate.reason}
        state.set("halted", "")

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
                record_external_position(
                    symbol, vp.side, vp.qty, vp.entry_price, vp.mark_price, vp.leverage, mode,
                    unrealized=vp.unrealized,
                )
                events.record("warn", "engine", f"发现交易所外部仓位 {symbol} {vp.side} {vp.qty}", {"symbol": symbol})
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
                    res = await broker.amend_stop(pos.symbol, side, new_sl, pos.qty)
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
            result = await asyncio.wait_for(evolution_review.review_trade(trade_id), timeout=120)
            if result.get("ok") and not result.get("skipped"):
                events.record(
                    "info", "evolution",
                    f"交易 #{trade_id} 复盘完成, 产出/更新心法 {len(result.get('applied', []))} 条",
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
        from ..models import TradeRecord

        try:
            with session_scope() as s:
                row = (
                    s.query(TradeRecord.id)
                    .filter(TradeRecord.reviewed.is_(False))
                    .order_by(TradeRecord.closed_at.asc())
                    .first()
                )
                trade_id = int(row[0]) if row else 0
            if trade_id:
                await self._review_trade(trade_id)
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
        try:
            tickers = await binance_client.tickers(symbols)
            funding = await binance_client.funding_rates(symbols)
            oi = await binance_client.open_interest(symbols[:6])
        except (BinanceError, NoCredentials) as exc:
            events.record("warn", "engine", f"行情取数失败, 本轮跳过扫描: {str(exc)[:160]}")
            return []
        prices = {s: t.last for s, t in tickers.items()}
        results: list[dict[str, Any]] = []

        for symbol in symbols:
            ticker = tickers.get(symbol)
            if ticker is None:
                continue
            try:
                candles = await binance_client.klines(symbol, strat_cfg.timeframe, 60)
            except (BinanceError, NoCredentials) as exc:
                log.warning("%s K线取数失败: %s", symbol, str(exc)[:120])
                continue
            if len(candles) < 30:
                continue
            features = build_features(
                candles, strat_cfg,
                funding=funding.get(symbol, 0.0),
                open_interest=oi.get(symbol, 0.0),
                change_24h=ticker.change_pct,
                quote_volume=ticker.quote_volume,
            )
            features["_bars"] = [
                {"o": c.open, "h": c.high, "l": c.low, "c": c.close} for c in candles[-14:]
            ]
            sig = score_signal(symbol, features, strat_cfg)
            per_symbol_min = min_scores.get(symbol, 0.0)
            if sig.direction != "flat" and per_symbol_min > 0 and abs(sig.score) < per_symbol_min:
                sig.blocked = f"信号分 {sig.score:.3f} 低于该标的最低要求 {per_symbol_min:.2f}"
                sig.direction = "flat"
            rec = self._record_signal(sig, ticker, "scan")
            results.append(rec)
            if sig.direction == "flat":
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
                self._open_entry(broker, sig, ticker, account, engine_cfg, risk_cfg, strat_cfg, rec["signal_id"])
            )
            self._entry_tasks[symbol].add_done_callback(lambda t, s=symbol: self._entry_tasks.pop(s, None))
        return results

    def _record_signal(self, sig, ticker, source: str) -> dict[str, Any]:
        # 先取环境再开事务: 避免在写事务里再嵌套一次配置读取
        mode = normalize_mode(get_engine().mode)
        now = int(time.time())
        features = {**sig.features, "price": ticker.last, "source": source}
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
                        row.repeats = int(row.repeats or 1) + 1
                        row.ts = now
                        row.score = sig.score
                        row.features = {**features, "first_ts": first_ts, "repeat": row.repeats}
                        s.flush()
                        return self._signal_view(row, sig, reason)

            row = Signal(
                ts=now,
                symbol=sig.symbol,
                direction=sig.direction,
                score=sig.score,
                features={**features, "first_ts": now, "repeat": 1},
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
        leverage = risk_mod.choose_leverage(sig.symbol, engine_cfg, risk_cfg, filters)
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
            if d.action == "hold":
                self._update_signal_reject(signal_id, f"AI 否决: {d.reason[:150]}")
                return
            if d.action == "close":
                self._update_signal_reject(signal_id, "AI 建议观望")
                return
            ai_dir = "long" if d.action == "open_long" else "short"
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
                leverage = risk_mod.choose_leverage(
                    sig.symbol, engine_cfg, risk_cfg, filters, min(d.leverage, ai_cfg.max_position_leverage)
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
        reserved_gen = await self._margin.reserve(reserved)
        if not reserved_gen:
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
            await self._margin.release(reserved, reserved_gen)
            raise
        if not res.ok:
            # 成交前失败 → 归还预留额度；成交后保留到下一轮 begin_cycle
            await self._margin.release(reserved, reserved_gen)
            # broker 的错误文案已带「下单失败 N:」，不要拼成「下单失败: 下单失败 N:」
            detail = res.message if res.message.startswith("下单失败") else f"下单失败: {res.message}"
            self._update_signal_reject(signal_id, detail[:200])
            events.record("error", "trade", f"{sig.symbol} 开仓失败: {res.message}"[:250])
            return
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
            mantras = [{"title": m["title"], "rule": m["rule"], "health": m["health"]} for m in lessons]
            # 只记 id，不把 id 塞进 prompt（快照用于事后「采用/未采用」对照）
            injected_ids = [int(m["id"]) for m in lessons if m.get("id")]
        news_items = news_service.digest(6) if ai_cfg.inject_news else []
        positions = risk_mod.open_positions()
        today_pnl = 0.0
        day_start = state.get_float("day_start_equity", account.equity)
        if day_start > 0:
            today_pnl = (account.equity - day_start) / day_start * 100
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
                "max_leverage": min(risk_cfg.max_leverage, filters.max_leverage, 100),
                "leverage_range": "1-100",
                "risk_per_trade_pct": risk_cfg.risk_per_trade_pct,
                "margin_pct": risk_cfg.margin_pct,
                "min_confidence": ai_cfg.min_confidence,
                "effective_min_notional": filters.effective_min_notional(ticker.last),
                "fee_rate_taker": risk_cfg.fee_rate_taker,
            },
            "news": news_items,
            "mantras": mantras,
            "injected_mantra_ids": injected_ids,
        }

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
            "halted": state.get("halted"),
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
