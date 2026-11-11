"""执行层: 币安合约 (模拟盘=测试网 / 实盘=主网), 同一套接口.

Engine 只依赖 Broker 协议, 因此可以在同一套风控与策略下切换 demo / live;
两个环境都是真实交易所 API, 因此仓位、成交与风控链路完全同构.
"""
from __future__ import annotations

import asyncio
import time
from dataclasses import dataclass, field
from typing import Any, Protocol

from ..db import session_scope
from ..logging_setup import get_logger
from ..models import OrderRecord, PositionRecord, TradeRecord, now_ts
from ..security import new_client_order_id
from ..services.db_writer import db_writer
from ..services.exec_metrics import exec_metrics
from ..services.settings import get_engine, get_risk, normalize_mode
from . import client as binance_client
from .client import STAGE_SINK, BinanceError, NoCredentials, UncertainSubmission
from .precision import SymbolFilters, floor_to_step

log = get_logger("broker")


def _compact_raw(resp: dict[str, Any]) -> dict[str, Any]:
    """只保留排障需要的关键字段.

    交易所回执里还有大量与本地无关的字段 (自成交防护、时间窗、费率等),
    整份塞进 orders.raw 会在下单关键路径上多一次 KB 级 JSON 序列化.
    """
    keys = ("orderId", "status", "avgPrice", "executedQty", "cumQuote", "updateTime", "clientOrderId")
    return {k: resp.get(k) for k in keys if resp.get(k) is not None}


def _persist_entry(order_fields: dict[str, Any], position_fields: dict[str, Any]) -> None:
    """开仓落库 (在写线程执行, 不在下单关键路径上).

    订单与持仓放在同一个事务里, 避免出现"有订单无持仓"的中间态.
    另外做一层同标的去重: 信号跟单可以在引擎两轮之间成交, 交易所仓位对账
    (`record_external_position`) 可能先写了一条外部仓位; 若这里再无条件 INSERT,
    会变成同标的两条 open 仓位, 风控计数与平仓都会错乱。
    """
    with session_scope() as s:
        existing = (
            s.query(PositionRecord)
            .filter(
                PositionRecord.symbol == position_fields["symbol"],
                PositionRecord.mode == position_fields["mode"],
                PositionRecord.status == "open",
            )
            .first()
        )
        if existing is not None:
            for key, value in position_fields.items():
                setattr(existing, key, value)
        else:
            s.add(PositionRecord(**position_fields))
        s.add(OrderRecord(**order_fields))


#: 正在平仓的仓位 id (互斥闸门, 写库完成后释放)
_CLOSING: set[int] = set()


def _persist_close(payload: dict[str, Any], position_id: int) -> None:
    """平仓落库 (在写线程执行).

    数量、手续费、盈亏都在写线程里按**当时的行**重算,
    避免用下单前的快照把并发修改覆盖掉.
    """
    try:
        with session_scope() as s:
            row = s.get(PositionRecord, position_id)
            if row is None or row.status != "open":
                return
            risk = get_risk()
            close_qty = float(payload["close_qty"])
            avg = float(payload["avg"])
            pnl = float(payload["pnl"])
            fee = float(payload["fee"])
            direction = 1 if row.side == "long" else -1
            qty_before = float(row.qty)
            closed_at = now_ts()

            row.fees = (row.fees or 0) + fee
            row.realized_pnl = (row.realized_pnl or 0) + pnl

            base: dict[str, Any] = {
                "symbol": row.symbol,
                "side": row.side,
                "entry_price": row.entry_price,
                "exit_price": avg,
                "leverage": row.leverage,
                "exit_reason": payload["reason"],
                "opened_at": row.opened_at,
                "closed_at": closed_at,
                "hold_seconds": max(0, closed_at - row.opened_at),
                "ai_confidence": row.ai_confidence,
                "ai_reason": row.ai_reason,
                "signal_score": row.signal_score,
                "slippage_bps": row.entry_slippage_bps,
                "mode": payload["mode"],
                "features": row.features,
            }

            if close_qty >= qty_before - 1e-12:
                row.status = "closed"
                row.closed_at = closed_at
                row.exit_price = avg
                row.exit_reason = payload["reason"]
                entry_fee = qty_before * row.entry_price * risk.fee_rate_taker
                trade = TradeRecord(
                    **base,
                    qty=qty_before,
                    notional=qty_before * row.entry_price,
                    pnl=round(pnl - entry_fee, 8),
                    pnl_pct=(
                        round((avg / row.entry_price - 1) * 100 * direction, 6)
                        if row.entry_price
                        else 0.0
                    ),
                    roi_pct=round((pnl - entry_fee) / max(1e-9, row.margin) * 100, 4),
                    fees=round(entry_fee + fee, 8),
                )
                s.add(trade)
                s.flush()
                row.trade_id = trade.id
            else:
                # 部分平仓: 仓位继续持有, 但这一腿也要记成交,
                # 否则它的盈亏只落在 positions.realized_pnl 里, 复盘统计会漏掉
                row.qty = qty_before - close_qty
                margin_part = (row.margin or 0) * close_qty / max(qty_before, 1e-12)
                entry_fee = close_qty * row.entry_price * risk.fee_rate_taker
                s.add(
                    TradeRecord(
                        **base,
                        qty=close_qty,
                        notional=close_qty * row.entry_price,
                        pnl=round(pnl - entry_fee, 8),
                        pnl_pct=(
                            round((avg / row.entry_price - 1) * 100 * direction, 6)
                            if row.entry_price
                            else 0.0
                        ),
                        roi_pct=round((pnl - entry_fee) / max(1e-9, margin_part) * 100, 4),
                        fees=round(entry_fee + fee, 8),
                    )
                )

            s.add(
                OrderRecord(
                    symbol=row.symbol,
                    side="SELL" if row.side == "long" else "BUY",
                    order_type="MARKET",
                    qty=close_qty,
                    price=avg,
                    status=payload["status"],
                    purpose="exit",
                    client_order_id=payload["client_order_id"],
                    exchange_order_id=payload["exchange_order_id"],
                    reduce_only=True,
                    mode=payload["mode"],
                    latency_ms=payload["latency_ms"],
                    raw=payload["raw"],
                )
            )
    finally:
        _CLOSING.discard(position_id)


@dataclass
class Account:
    wallet: float
    available: float
    unrealized: float
    margin_used: float
    equity: float
    mode: str = "demo"
    source: str = "local"

    def as_dict(self) -> dict[str, Any]:
        return {
            "wallet": round(self.wallet, 8),
            "available": round(self.available, 8),
            "unrealized": round(self.unrealized, 8),
            "margin_used": round(self.margin_used, 8),
            "equity": round(self.equity, 8),
            "mode": self.mode,
            "source": self.source,
        }


@dataclass
class ExchangePosition:
    symbol: str
    side: str
    qty: float
    entry_price: float
    mark_price: float
    leverage: float
    margin: float
    unrealized: float
    source: str = "local"


@dataclass
class ExecResult:
    ok: bool
    price: float = 0.0
    qty: float = 0.0
    message: str = ""
    order_id: str = ""
    raw: dict[str, Any] = field(default_factory=dict)


class Broker(Protocol):
    mode: str

    async def account(self) -> Account: ...
    async def positions(self) -> list[ExchangePosition]: ...
    async def open_position(self, **kwargs: Any) -> ExecResult: ...
    async def close_position(self, **kwargs: Any) -> ExecResult: ...
    async def amend_stop(self, symbol: str, side: str, stop_price: float, qty: float) -> ExecResult: ...
    async def health(self) -> dict[str, Any]: ...
    async def trade_mode_status(self, symbols: list[str] | None = None) -> dict[str, Any]: ...
    async def switch_trade_mode(self, *, hedge: bool = True, isolated: bool = True) -> dict[str, Any]: ...


# -------------------------------------------------------------------- 执行层
def hedge_order_params(dual: bool, side: str, *, closing: bool = False) -> dict[str, str]:
    """下单方向参数.

    双向持仓(hedge)必须用 ``positionSide`` 标识仓位方向，且**不能**与 ``reduceOnly`` 同用；
    单向持仓沿用 ``reduceOnly`` 平仓。两种模式都能下单，账户模式由面板引导切换。
    """
    if not dual:
        return {"reduceOnly": "true"} if closing else {}
    return {"positionSide": "LONG" if side == "long" else "SHORT"}


class BinanceBroker:
    """币安合约执行层.

    模拟盘(demo)对接币安模拟盘环境, 实盘(live)对接币安实盘; 两者都是真实 API,
    差别只有域名与凭证, 因此共用同一实现, 由当前交易环境决定用哪套.
    """

    def __init__(self) -> None:
        self._configured: set[str] = set()
        self._dual: bool | None = None

    @property
    def mode(self) -> str:
        return normalize_mode(get_engine().mode)

    async def _ensure_trade_mode(self) -> bool:
        """探测并缓存账户持仓模式（True=双向 hedge）.

        系统按「双向逐仓」设计，但**不再擅自把用户账户切成单向**：
        切换持仓模式属于账户级变更，且只在无持仓/无挂单时允许，统一交给面板
        「参数配置 → 引擎 → 交易模式」一键操作。单向账户仍可下单（参数自适应）。
        """
        if self._dual is not None:
            return self._dual
        try:
            self._dual = bool(await binance_client.client.get_position_mode())
        except BinanceError as exc:
            log.warning("持仓模式探测失败，按单向处理: %s", exc)
            self._dual = False
        if not self._dual:
            log.warning(
                "账户当前为单向持仓；系统按双向逐仓设计，建议在「参数配置 → 引擎 → 交易模式」一键开启双向持仓"
            )
        return self._dual

    async def _ensure_symbol(self, symbol: str, leverage: int) -> None:
        """确保逐仓 + 杠杆.

        全仓账户在这里**自动切逐仓**；有持仓时交易所会拒绝切换（-4046 表示已是逐仓），
        此时只记日志、不阻断开仓，由面板提示「平仓后再切换」。
        """
        if symbol in self._configured:
            return
        try:
            await binance_client.client.set_margin_type(symbol, "ISOLATED")
        except BinanceError as exc:
            log.warning("%s 切换逐仓失败（将沿用当前保证金模式）: %s", symbol, exc)
        await binance_client.client.set_leverage(symbol, int(leverage))
        self._configured.add(symbol)

    async def account(self) -> Account:
        data = await binance_client.client.account()
        wallet = float(data.get("totalWalletBalance", 0) or 0)
        unrealized = float(data.get("totalUnrealizedProfit", 0) or 0)
        margin = float(data.get("totalInitialMargin", 0) or 0)
        available = float(data.get("availableBalance", 0) or 0)
        equity = float(data.get("totalMarginBalance", wallet + unrealized) or wallet + unrealized)
        return Account(
            wallet=wallet,
            available=available,
            unrealized=unrealized,
            margin_used=margin,
            equity=equity,
            mode=self.mode,
            source="binance",
        )

    async def positions(self) -> list[ExchangePosition]:
        rows = await binance_client.client.positions()
        out: list[ExchangePosition] = []
        for row in rows:
            amt = float(row.get("positionAmt", 0) or 0)
            if abs(amt) < 1e-12:
                continue
            entry = float(row.get("entryPrice", 0) or 0)
            mark = float(row.get("markPrice", 0) or 0)
            lev = float(row.get("leverage", 1) or 1)
            notional = abs(amt) * (mark or entry)
            out.append(
                ExchangePosition(
                    symbol=row["symbol"],
                    side="long" if amt > 0 else "short",
                    qty=abs(amt),
                    entry_price=entry,
                    mark_price=mark,
                    leverage=lev,
                    margin=notional / max(1.0, lev),
                    unrealized=float(row.get("unRealizedProfit", 0) or 0),
                    source="binance",
                )
            )
        return out

    async def open_position(
        self,
        *,
        symbol: str,
        side: str,
        qty: float,
        leverage: float,
        sl_price: float,
        tp_price: float,
        filters: SymbolFilters,
        ai_confidence: float = 0.0,
        ai_reason: str = "",
        signal_score: float = 0.0,
        features: dict | None = None,
        **_: Any,
    ) -> ExecResult:
        risk = get_risk()
        engine = get_engine()
        t_call = time.perf_counter()
        stage: dict[str, float] = {}
        try:
            dual = await self._ensure_trade_mode()
            await self._ensure_symbol(symbol, int(leverage))
            cid = new_client_order_id("nq")
            side_txt = "BUY" if side == "long" else "SELL"
            params: dict[str, Any] = {
                "symbol": symbol,
                "side": side_txt,
                "type": "MARKET",
                "quantity": filters.format_qty(qty),
                "newClientOrderId": cid,
                "recvWindow": engine.recv_window_ms,
                **hedge_order_params(dual, side),
            }
            if engine.order_type == "limit_post_only":
                px = filters.last_price or 0
                if px > 0:
                    params["type"] = "LIMIT"
                    params["timeInForce"] = "GTX"
                    params["price"] = filters.format_price(px)
            stage["plan_ms"] = (time.perf_counter() - t_call) * 1000
            t0 = time.perf_counter()
            sink_token = STAGE_SINK.set(stage)
            try:
                try:
                    resp = await binance_client.client.place_order(**params)
                except UncertainSubmission as exc:
                    resolved = await self._resolve_entry(symbol, cid)
                    if not resolved:
                        return ExecResult(False, message=f"下单结果未知且查询失败: {exc.message}")
                    resp = resolved
            finally:
                STAGE_SINK.reset(sink_token)
            stage["send_ms"] = (time.perf_counter() - t0) * 1000
            latency_ms = int(stage["send_ms"])
            filled_qty = float(resp.get("executedQty", 0) or 0) or qty
            avg = float(resp.get("avgPrice", 0) or 0)
            if avg <= 0:
                avg = filters.last_price
            if avg <= 0:
                # 回执没带均价且缓存价为空时, 现取一次盘口, 否则会写出 entry_price=0 的脏仓位
                try:
                    tick = (await binance_client.client.book_tickers([symbol])).get(symbol)
                    avg = (tick[0] + tick[1]) / 2 if tick else 0.0
                except BinanceError:
                    avg = 0.0
            if avg <= 0:
                return ExecResult(False, message="下单成功但取不到成交价, 已放弃落库")
            # 入场滑点 (bps): 成交均价相对决策价的偏差, 方向校正后正值为不利滑点
            ref = filters.last_price or 0
            direction = 1 if side == "long" else -1
            slip = round((avg - ref) / ref * 10000 * direction, 4) if ref > 0 else 0.0
            t_persist = time.perf_counter()
            # 落库交给写线程: 关键路径只付出一次入队 (微秒级),
            # 不再承受 WAL checkpoint / fsync 的几十毫秒尖刺.
            db_writer.submit(
                _persist_entry,
                {
                    "symbol": symbol,
                    "side": side_txt,
                    "order_type": params["type"],
                    "qty": filled_qty,
                    "price": avg,
                    "status": str(resp.get("status", "NEW")),
                    "purpose": "entry",
                    "client_order_id": cid,
                    "exchange_order_id": str(resp.get("orderId", "")),
                    "mode": self.mode,
                    "latency_ms": latency_ms,
                    "raw": _compact_raw(resp),
                },
                {
                    "symbol": symbol,
                    "side": side,
                    "qty": filled_qty,
                    "entry_price": avg,
                    "mark_price": avg,
                    "leverage": leverage,
                    "margin": filled_qty * avg / max(1.0, leverage),
                    "sl_price": sl_price,
                    "tp_price": tp_price,
                    "peak_price": avg,
                    "opened_at": now_ts(),
                    "ai_confidence": ai_confidence,
                    "ai_reason": ai_reason,
                    "signal_score": signal_score,
                    "features": features or {},
                    "entry_slippage_bps": slip,
                    "initial_sl_price": sl_price,
                    "mode": self.mode,
                    "status": "open",
                },
            )
            stage["persist_ms"] = (time.perf_counter() - t_persist) * 1000
            t_protect = time.perf_counter()
            await self._place_protection(symbol, side, sl_price, tp_price, filters, dual=dual)
            stage["protect_ms"] = (time.perf_counter() - t_protect) * 1000
            stage["total_ms"] = (time.perf_counter() - t_call) * 1000
            exec_metrics.record(stage)
            label = "实盘" if self.mode == "live" else "模拟盘"
            return ExecResult(True, price=avg, qty=filled_qty, order_id=str(resp.get("orderId", "")), message=f"{label}开仓成功", raw=resp)
        except NoCredentials as exc:
            return ExecResult(False, message=str(exc))
        except BinanceError as exc:
            if exc.code == -2019:
                need = qty * (filters.last_price or 0) / max(1.0, leverage)
                return ExecResult(False, message=margin_insufficient_hint(need, leverage))
            if exc.code == -1111:
                return ExecResult(False, message=precision_hint(filters, qty))
            return ExecResult(False, message=f"下单失败 {exc.code}: {exc.message}")

    async def _place_protection(
        self,
        symbol: str,
        side: str,
        sl_price: float,
        tp_price: float,
        filters: Any = None,
        *,
        dual: bool = False,
    ) -> None:
        close_side = "SELL" if side == "long" else "BUY"
        # 双向持仓：closePosition 需配合 positionSide 指明平哪一侧；不能用 reduceOnly
        side_params = hedge_order_params(dual, side, closing=True) if dual else {}

        async def place(kind: str, price: float) -> None:
            if price <= 0:
                return
            if isinstance(filters, SymbolFilters) and filters.price_tick > 0:
                px = filters.format_price(price)
            else:
                px = f"{price:.8f}".rstrip("0").rstrip(".")
            # 币安已要求 closePosition 类条件单走 Algo 接口(-4120), 老接口仅作回落
            algo_params = {
                "algoType": "CONDITIONAL",
                "symbol": symbol,
                "side": close_side,
                "type": kind,
                "triggerPrice": px,
                "closePosition": "true",
                "workingType": "MARK_PRICE",
                "priceProtect": "TRUE",
                "clientAlgoId": new_client_order_id("np"),
                **side_params,
            }
            try:
                await binance_client.client.place_algo_order(**algo_params)
            except BinanceError as exc:
                if exc.code in (-4120, -1121, -1102):
                    legacy = {
                        "symbol": symbol,
                        "side": close_side,
                        "type": kind,
                        "stopPrice": px,
                        "closePosition": "true",
                        "workingType": "MARK_PRICE",
                        "priceProtect": "TRUE",
                        "newClientOrderId": new_client_order_id("np"),
                        **side_params,
                    }
                    try:
                        await binance_client.client.place_order(**legacy)
                    except BinanceError as exc2:
                        log.warning("挂保护单 %s %s 失败: %s", symbol, kind, exc2)
                else:
                    log.warning("挂保护单 %s %s 失败: %s", symbol, kind, exc)

        # 止损与止盈是两笔独立的 reduce-only 单, 并行发出:
        # 串行会让开仓关键路径多背一个 RTT, 直接体现在下单 p99 上.
        await asyncio.gather(place("STOP_MARKET", sl_price), place("TAKE_PROFIT_MARKET", tp_price))

    async def _resolve_entry(self, symbol: str, cid: str) -> dict | None:
        for _ in range(4):
            try:
                return await binance_client.client.get_order(symbol, client_id=cid)
            except BinanceError:
                await __import__("asyncio").sleep(1.2)
        return None

    async def close_position(self, *, position: PositionRecord, reason: str, qty: float | None = None, **_: Any) -> ExecResult:
        pid = position.id
        # 同一仓位的平仓互斥: 写库异步化后, 行状态会在几百微秒内仍是 open,
        # 没有这道闸门时引擎轮询与手动平仓可能重复下单.
        if pid in _CLOSING:
            return ExecResult(False, message="该仓位正在平仓中, 请稍候")
        _CLOSING.add(pid)

        side = "SELL" if position.side == "long" else "BUY"
        t_call = time.perf_counter()
        stage: dict[str, float] = {}
        try:
            # 只读校验: WAL 下读不阻塞写, 成本 ~0.05ms; 拿权威数量避免用过期快照
            with session_scope() as s:
                row = s.get(PositionRecord, pid)
                if row is None or row.status != "open":
                    _CLOSING.discard(pid)
                    return ExecResult(False, message="仓位已平或不存在")
                live_qty = float(row.qty)

            close_qty = min(live_qty, qty) if qty else live_qty
            # 平仓数量必须按步进对齐, 否则部分平仓会 -1111 (精度超限)
            try:
                fmap = await binance_client.client.symbol_filters()
                f = fmap.get(position.symbol)
            except BinanceError:
                f = None
            if f is not None and f.qty_step:
                close_qty = floor_to_step(close_qty, f.qty_step)
                if close_qty < (f.min_qty or 0) - 1e-12:
                    _CLOSING.discard(pid)
                    return ExecResult(False, message=f"平仓数量 {close_qty} 低于最小下单量 {f.min_qty}")
            qty_text = f.format_qty(close_qty) if f is not None else f"{close_qty:.8f}".rstrip("0").rstrip(".")
            t0 = time.perf_counter()
            sink_token = STAGE_SINK.set(stage)
            dual = await self._ensure_trade_mode()
            try:
                resp = await binance_client.client.place_order(
                    symbol=position.symbol,
                    side=side,
                    type="MARKET",
                    quantity=qty_text,
                    newClientOrderId=new_client_order_id("nx"),
                    **hedge_order_params(dual, position.side, closing=True),
                )
            finally:
                STAGE_SINK.reset(sink_token)
            stage["send_ms"] = (time.perf_counter() - t0) * 1000
            latency_ms = int(stage["send_ms"])
            avg = float(resp.get("avgPrice", 0) or 0) or position.mark_price or position.entry_price
            direction = 1 if position.side == "long" else -1
            gross = (avg - position.entry_price) * close_qty * direction
            risk = get_risk()
            fee = close_qty * avg * risk.fee_rate_taker
            pnl = gross - fee

            # 落库交给写线程: 关键路径只付一次入队
            t_persist = time.perf_counter()
            db_writer.submit(
                _persist_close,
                {
                    "close_qty": close_qty,
                    "avg": avg,
                    "reason": reason,
                    "fee": fee,
                    "pnl": pnl,
                    "latency_ms": latency_ms,
                    "status": str(resp.get("status", "")),
                    "client_order_id": str(resp.get("clientOrderId", "")),
                    "exchange_order_id": str(resp.get("orderId", "")),
                    "mode": self.mode,
                    "raw": _compact_raw(resp),
                },
                pid,
            )
            stage["persist_ms"] = (time.perf_counter() - t_persist) * 1000
            stage["total_ms"] = (time.perf_counter() - t_call) * 1000
            exec_metrics.record(stage)
            # 只有全平才撤保护单; 部分止盈后剩余仓位仍需止损/止盈保护
            if close_qty >= live_qty - 1e-12:
                try:
                    await binance_client.client.cancel_all(position.symbol)
                    await binance_client.client.cancel_all_algo(position.symbol)
                except BinanceError:
                    pass
            label = "实盘" if self.mode == "live" else "模拟盘"
            return ExecResult(True, price=avg, qty=close_qty, message=f"{label}平仓 {reason}", raw=resp)
        except NoCredentials as exc:
            _CLOSING.discard(pid)
            return ExecResult(False, message=str(exc))
        except BinanceError as exc:
            _CLOSING.discard(pid)
            return ExecResult(False, message=f"平仓失败 {exc.code}: {exc.message}")
        except Exception:
            _CLOSING.discard(pid)
            raise

    async def amend_stop(self, symbol: str, side: str, stop_price: float, qty: float) -> ExecResult:
        close_side = "SELL" if side == "long" else "BUY"
        try:
            dual = await self._ensure_trade_mode()
            await binance_client.client.cancel_all(symbol)
            await binance_client.client.cancel_all_algo(symbol)
            try:
                fmap = await binance_client.client.symbol_filters()
                pf = fmap.get(symbol)
            except BinanceError:
                pf = None
            px = pf.format_price(stop_price) if pf is not None else f"{stop_price:.8f}".rstrip("0").rstrip(".")
            params = {
                "algoType": "CONDITIONAL",
                "symbol": symbol,
                "side": close_side,
                "type": "STOP_MARKET",
                "triggerPrice": px,
                "closePosition": "true",
                "workingType": "MARK_PRICE",
                "priceProtect": "TRUE",
                "clientAlgoId": new_client_order_id("nt"),
                **(hedge_order_params(dual, side, closing=True) if dual else {}),
            }
            await binance_client.client.place_algo_order(**params)
            with session_scope() as s:
                row = (
                    s.query(PositionRecord)
                    .filter(PositionRecord.symbol == symbol, PositionRecord.status == "open")
                    .first()
                )
                if row:
                    row.sl_price = stop_price
            return ExecResult(True, price=stop_price, message="交易所止损已上移")
        except BinanceError as exc:
            return ExecResult(False, message=f"止损上移失败: {exc.message}")

    async def trade_mode_status(self, symbols: list[str] | None = None) -> dict[str, Any]:
        """持仓模式与保证金模式体检（只读）.

        ``hedge_mode`` 是否双向；``need_hedge`` 需要引导用户开启；
        ``need_isolated`` 仍为全仓、需要切换的池内标的；
        ``with_positions`` 有持仓的标的（有仓时不能改持仓模式/保证金模式）。
        """
        dual = await self._ensure_trade_mode()
        margin: dict[str, str] = {}
        with_positions: list[str] = []
        try:
            rows = await binance_client.client.positions()
        except (BinanceError, NoCredentials):
            rows = []
        for row in rows:
            sym = str(row.get("symbol") or "")
            if not sym:
                continue
            margin[sym] = str(row.get("marginType") or "").lower()
            try:
                if abs(float(row.get("positionAmt", 0) or 0)) > 1e-12:
                    with_positions.append(sym)
            except (TypeError, ValueError):
                continue
        pool = [s for s in (symbols or with_positions) if s]
        need_isolated = [s for s in pool if margin.get(s) == "cross"]
        # 无持仓时交易所通常不回传 marginType：此时不能断言「全部逐仓」
        margin_known = sum(1 for s in pool if margin.get(s) in ("cross", "isolated"))
        if dual and not need_isolated:
            hint = "已是双向逐仓"
        elif not dual:
            hint = "账户为单向持仓：请先开启双向持仓（需无持仓/无挂单）"
        elif not margin_known:
            hint = "无持仓；开仓时会自动把该标的切为逐仓"
        else:
            hint = f"{len(need_isolated)} 个标的仍是全仓，建议切为逐仓"
        return {
            "hedge_mode": dual,
            "need_hedge": not dual,
            "need_isolated": need_isolated,
            "margin_known": margin_known,
            "with_positions": sorted(with_positions),
            "can_switch": not with_positions,
            "margin": {s: margin.get(s, "unknown") for s in pool},
            "hint": hint,
        }

    async def switch_trade_mode(self, *, hedge: bool = True, isolated: bool = True) -> dict[str, Any]:
        """引导式切换：双向持仓 + 逐仓。逐项返回结果，失败给出可操作原因."""
        results: list[dict[str, Any]] = []
        if hedge:
            try:
                already = await binance_client.client.get_position_mode()
                if already:
                    self._dual = True
                    results.append({"target": "hedge", "ok": True, "detail": "已是双向持仓"})
                else:
                    await binance_client.client.set_position_mode(True)
                    self._dual = True
                    results.append({"target": "hedge", "ok": True, "detail": "已切换为双向持仓"})
            except BinanceError as exc:
                detail = (
                    "账户仍有持仓或挂单：双向/单向只能在无仓时切换，请先全部平仓（含保护单）后重试"
                    if exc.code in (-4059, -4061, -4068)
                    else exc.message
                )
                results.append({"target": "hedge", "ok": False, "detail": detail})
        if isolated:
            from ..services import symbols as symbol_service

            done: list[str] = []
            failed: list[dict[str, str]] = []
            for sym in sorted(symbol_service.known_symbols()):
                try:
                    await binance_client.client.set_margin_type(sym, "ISOLATED")
                    done.append(sym)
                except BinanceError as exc:
                    failed.append({"symbol": sym, "detail": str(exc.message)[:120]})
            results.append(
                {
                    "target": "isolated",
                    "ok": not failed,
                    "detail": f"已确保 {len(done)} 个标的为逐仓"
                    + (f"，{len(failed)} 个失败（有持仓需先平仓）" if failed else ""),
                    "failed": failed,
                }
            )
        return {"ok": all(r["ok"] for r in results), "results": results}

    async def health(self) -> dict[str, Any]:
        try:
            ok = await binance_client.client.ping()
            mode = await binance_client.client.get_position_mode()
            self._dual = mode
            return {
                "mode": self.mode,
                "ok": ok,
                "dual_side": mode,
                "weight_used": binance_client.client.weight.used_weight,
                "banned_sec": round(binance_client.client.weight.ban_remaining(), 1),
                "detail": "币安合约接口连通",
            }
        except NoCredentials as exc:
            return {"mode": self.mode, "ok": False, "detail": str(exc)}
        except Exception as exc:  # noqa: BLE001
            return {"mode": self.mode, "ok": False, "detail": str(exc)[:200]}


def get_broker(mode: str | None = None) -> Broker:
    """执行层: 模拟盘与实盘都走币安, 差别是域名与凭证 (由当前交易环境决定)."""
    return BinanceBroker()


def position_from_db(position_id: int) -> PositionRecord | None:
    with session_scope() as s:
        return s.get(PositionRecord, position_id)


#: 交易所按 availableBalance 校验初始保证金；留 10% 给手续费/资金费/保护单划转
MARGIN_SAFETY = 0.90


def margin_insufficient_hint(required_margin: float, leverage: float = 1.0) -> str:
    """-2019 的可读解释：不要再让用户只看到一句 Margin is insufficient."""
    return (
        f"下单失败 -2019: 可用保证金不足（本单约需 {required_margin:.2f} USDT 初始保证金 @ {leverage:g}x）。"
        "已有仓位/挂单占用了保证金；可先平掉部分仓位，或降低「单仓保证金/单笔风险」、提高杠杆。"
    )


def precision_hint(filters: SymbolFilters | None, qty: float) -> str:
    """-1111 的可读解释：数量/价格小数位超过该合约允许精度."""
    step = getattr(filters, "qty_step", 0) or 0
    tick = getattr(filters, "price_tick", 0) or 0
    return (
        f"下单失败 -1111: 数量/价格精度超出交易所限制（qty={qty!r}, qty_step={step:g}, price_tick={tick:g}）。"
        "已按合约步进对齐；若仍出现，请检查标的池里的 precision/tick_sz 是否与交易所一致（可删除后重新添加）。"
    )


def estimate_qty(
    equity: float,
    price: float,
    leverage: float,
    risk_cfg,
    sl_pct: float,
    filters: SymbolFilters,
    risk_scale: float = 1.0,
    available: float | None = None,
) -> tuple[float, str]:
    """风险预算 -> 名义额 -> 数量, 并对齐步进; 返回 (qty, 说明).

    - `risk_scale`: 波动率目标缩放系数 (1.0=不调整)
    - `available`: 账户可用余额。交易所按 **availableBalance** 校验初始保证金，
      而不是按总权益。老实现只用 equity 算，已有仓位占满保证金时会算出买不起的数量，
      最终被交易所以 -2019 Margin is insufficient 拒绝。
    """
    if equity <= 0 or price <= 0:
        return 0.0, "权益或价格无效"
    risk_budget = equity * risk_cfg.risk_per_trade_pct / 100.0 * max(0.0, risk_scale)
    risk_notional = risk_budget / max(sl_pct, 1e-6)
    margin_cap = equity * risk_cfg.margin_pct / 100.0 * leverage
    notional = min(risk_notional, margin_cap)

    affordable = 0.0
    if available is not None:
        # 本单初始保证金 = notional / leverage, 必须 <= 可用余额(留安全垫)
        affordable = max(0.0, float(available)) * MARGIN_SAFETY * leverage
        notional = min(notional, affordable)

    qty = floor_to_step(notional / price, filters.qty_step)
    min_qty = filters.effective_min_qty(price)
    if qty <= 0 or qty < min_qty - 1e-12:
        need = filters.effective_min_notional(price)
        if available is not None and affordable < need:
            return (
                0.0,
                f"可用保证金不足: 可用 {float(available):.2f} USDT, "
                f"按 {leverage:g}x 最多可开 {affordable:.2f} USDT 名义额, "
                f"但该标的最小名义额约 {need:.2f} USDT; "
                f"请平掉已有仓位、提高杠杆或降低单仓保证金占比",
            )
        return (
            0.0,
            f"可分配名义额 {notional:.4f} USDT 不足该标的最小可下单量 "
            f"(需约 {need:.2f} USDT, 最小数量 {min_qty} @ {price:.6g}); "
            f"可提高保证金比例/杠杆或改用更小面值标的",
        )
    ok, why = filters.validate(qty, price)
    if not ok:
        need = filters.effective_min_notional(price)
        return 0.0, f"{why} (该标的有效最小名义额约 {need:.2f} USDT)"
    return qty, "ok"


def record_external_position(
    symbol: str,
    side: str,
    qty: float,
    entry: float,
    mark: float,
    leverage: float,
    mode: str | None = None,
    unrealized: float = 0.0,
) -> None:
    """记录交易所已有仓位 (外部下单/崩溃恢复).

    以前这里用了 `self.mode`（模块级函数里没有 self，会 NameError），
    导致恢复出来的仓位根本写不进库。
    """
    resolved = normalize_mode(mode)
    now = int(time.time())
    with session_scope() as s:
        exists = (
            s.query(PositionRecord)
            .filter(PositionRecord.symbol == symbol, PositionRecord.mode == resolved, PositionRecord.status == "open")
            .first()
        )
        if exists:
            exists.qty = qty
            exists.mark_price = mark
            exists.entry_price = entry or exists.entry_price
            exists.leverage = leverage
            exists.venue_unrealized = float(unrealized or 0.0)
            exists.venue_ts = now
            return
        s.add(
            PositionRecord(
                symbol=symbol,
                side=side,
                qty=qty,
                entry_price=entry,
                mark_price=mark,
                leverage=leverage,
                margin=qty * entry / max(1.0, leverage),
                opened_at=now,
                mode=resolved,
                status="open",
                venue_unrealized=float(unrealized or 0.0),
                venue_ts=now,
                ai_reason="交易所已有仓位(外部/恢复)",
            )
        )
