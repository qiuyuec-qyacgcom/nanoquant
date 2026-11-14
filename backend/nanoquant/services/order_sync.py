"""委托状态收敛（订单状态机的最小实现）.

问题：`orders` 表原来只记录下单那一刻的回执快照。市价单通常会立刻 FILLED，
但挂单（`order_type=limit_post_only`）回执是 NEW，之后到底是成交、撤销还是过期，
系统再也不知道——面板上永远停着一条 NEW，成交价与成交量也停在 0。

做法：按需向交易所查询**非终态**订单（NEW / PARTIALLY_FILLED / PENDING / 空状态），
把状态、成交量、成交均价写回同一条记录，并把"入场单最终一张都没成交"挑出来，
交给调用方告警——这种单子在账本里对应的是幽灵仓位（交易所根本没有这个仓）。

刻意不做的事：
- 不改持仓数量。持仓以「交易所对账」为准（`_reconcile_live` 会把交易所没有的仓按
  venue_sync 关掉），订单状态只负责把审计台账补真。
- 不重发任何订单。结果不明时查询，永远比重发安全。
"""
from __future__ import annotations

import time
from typing import Any

from ..db import session_scope
from ..logging_setup import get_logger
from ..models import OrderRecord
from . import events

log = get_logger("orders")

#: 这些状态不会再变，查也没用
TERMINAL = {"FILLED", "CANCELED", "CANCELLED", "EXPIRED", "REJECTED", "NEW_INSURANCE", "NEW_ADL"}
#: 只回看最近这么久的单，避免把几周前的历史单全捞出来查
LOOKBACK_SEC = 24 * 3600
#: 单轮最多查多少条（每查一条就是一次 REST，别把权重打满）
BATCH = 20


def _status_of(raw: dict[str, Any] | None) -> str:
    return str((raw or {}).get("status") or "").strip().upper()


def pending_orders(
    *, limit: int = BATCH, lookback_sec: int = LOOKBACK_SEC, mode: str | None = None
) -> list[dict[str, Any]]:
    """需要向交易所确认的委托（新→旧）.

    ``mode`` 必须按当前交易环境过滤：模拟盘的单子拿去实盘查询只会得到
    "Order does not exist"，反过来还会污染真实台账。
    """
    since = int(time.time()) - lookback_sec
    with session_scope() as s:
        q = s.query(OrderRecord).filter(OrderRecord.ts >= since)
        if mode:
            q = q.filter(OrderRecord.mode == mode)
        rows = q.order_by(OrderRecord.ts.desc()).limit(limit * 5).all()
        out: list[dict[str, Any]] = []
        for r in rows:
            status = str(r.status or "").strip().upper()
            if status in TERMINAL:
                continue
            out.append(
                {
                    "id": r.id,
                    "symbol": r.symbol,
                    "purpose": r.purpose,
                    "status": status,
                    "exchange_order_id": str(r.exchange_order_id or ""),
                    "client_order_id": str(r.client_order_id or ""),
                }
            )
            if len(out) >= limit:
                break
    return out


def _apply(order_id: int, resp: dict[str, Any]) -> dict[str, Any]:
    """把交易所回执写回订单行，返回摘要."""
    status = _status_of(resp)
    filled = float(resp.get("executedQty", 0) or 0)
    avg = float(resp.get("avgPrice", 0) or 0)
    with session_scope() as s:
        row = s.get(OrderRecord, order_id)
        if row is None:
            return {}
        prev = str(row.status or "").strip().upper()
        if status:
            row.status = status
        raw = dict(row.raw or {})
        raw.update(
            {
                "status": status or prev,
                "executedQty": filled,
                "avgPrice": avg,
                "synced_at": int(time.time()),
            }
        )
        row.raw = raw
        if filled > 0:
            row.qty = filled
        if avg > 0:
            row.price = avg
        return {
            "id": row.id,
            "symbol": row.symbol,
            "purpose": row.purpose,
            "prev": prev,
            "status": status or prev,
            "filled": filled,
        }


async def sync(*, limit: int = BATCH, mode: str | None = None) -> dict[str, Any]:
    """把非终态委托与交易所对齐；返回统计与"未成交入场单"清单.

    不传 ``mode`` 时只看当前交易环境的委托（模拟盘/实盘互不串台）。
    """
    if mode is None:
        from .settings import get_engine, normalize_mode

        mode = normalize_mode(get_engine().mode)
    items = pending_orders(limit=limit, mode=mode)
    if not items:
        return {"checked": 0, "updated": 0, "errors": 0, "unfilled": []}

    from ..exchange import client as binance_client
    from ..exchange.client import BinanceError, NoCredentials

    checked = updated = errors = 0
    unfilled: list[dict[str, Any]] = []
    for item in items:
        checked += 1
        try:
            if item["exchange_order_id"]:
                resp = await binance_client.client.get_order(
                    item["symbol"], order_id=item["exchange_order_id"]
                )
            elif item["client_order_id"]:
                # 回执丢了 orderId 的极端情况：用客户端 id 兜底
                resp = await binance_client.client.get_order(
                    item["symbol"], client_id=item["client_order_id"]
                )
            else:
                continue
        except (BinanceError, NoCredentials) as exc:
            errors += 1
            log.debug("订单同步跳过 %s#%s: %s", item["symbol"], item["id"], exc)
            continue

        result = _apply(item["id"], resp)
        if not result:
            continue
        updated += 1
        if (
            result["purpose"] == "entry"
            and result["status"] in {"CANCELED", "CANCELLED", "EXPIRED", "REJECTED"}
            and result["filled"] <= 0
        ):
            unfilled.append(result)

    if unfilled:
        for item in unfilled:
            events.record(
                "warn",
                "orders",
                f"{item['symbol']} 入场单 {item['prev']}→{item['status']} 且成交量为 0："
                f"交易所没有建仓，账本里的仓位会被对账按 venue_sync 清掉",
                {"order_id": item["id"], "symbol": item["symbol"], "status": item["status"]},
            )
    return {"checked": checked, "updated": updated, "errors": errors, "unfilled": unfilled}
