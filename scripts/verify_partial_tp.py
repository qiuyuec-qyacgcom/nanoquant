"""部分止盈实盘验证（测试网真实下单）.

验证 4 件事:
    1. reduce-only 部分平仓后, 交易所与本地数量一致;
    2. 剩余仓位仍为 open, 且保护单(Algo)没有被误撤;
    3. 部分平仓生成独立 TradeRecord, 数量与本次成交一致;
    4. 全平后保护单自动清除, 本地状态 closed.

用法: python scripts/verify_partial_tp.py --symbol BTCUSDT
"""
from __future__ import annotations

import argparse
import asyncio
import math
import os
import sqlite3
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "backend"))
os.environ["NANOQUANT_DB"] = str(Path(tempfile.mkdtemp(prefix="nq_ptp_")) / "ptp.db")


def check(label: str, ok: bool, detail: str = "") -> bool:
    print(f"[{'PASS' if ok else 'FAIL'}] {label}{(' · ' + detail) if detail else ''}")
    return ok


async def run(symbol: str) -> int:
    from nanoquant.db import init_db, session_scope
    from nanoquant.exchange import client as bc
    from nanoquant.exchange.broker import get_broker
    from nanoquant.models import PositionRecord, TradeRecord
    from nanoquant.services.db_writer import db_writer
    from nanoquant.services.settings import update_group

    with sqlite3.connect(str(ROOT / "data" / "nanoquant.db")) as conn:
        row = conn.execute("select value from app_settings where key='data'").fetchone()
    import json

    real = json.loads(row[0]) if row else {}
    init_db()
    update_group(
        "data",
        {
            "demo_base_url": real.get("demo_base_url") or "https://testnet.binancefuture.com",
            "demo_backup_urls": real.get("demo_backup_urls") or [],
            "demo_api_key_enc": real["demo_api_key_enc"],
            "demo_api_secret_enc": real["demo_api_secret_enc"],
        },
    )

    client = bc.client
    filters = (await client.symbol_filters(force=True)).get(symbol)
    if filters is None:
        print("没有合约信息")
        return 1
    tick = (await client.book_tickers([symbol])).get(symbol)
    price = (tick[0] + tick[1]) / 2 if tick else filters.last_price
    filters.last_price = price

    step = filters.qty_step or 0.001
    # 开到 2 倍最小名义额, 保证平一半后仍高于最小下单量
    unit = max(1, math.ceil((max(filters.min_notional or 5.0, 5.0) * 2.2 / price) / step))
    qty = round(unit * 2 * step, 10)
    half = round(unit * step, 10)
    print(f"symbol={symbol} price={price:.2f} qty={qty} half={half}\n")

    broker = get_broker("demo")
    opened = await broker.open_position(
        symbol=symbol,
        side="long",
        qty=qty,
        leverage=5,
        sl_price=round(price * 0.97, 2),
        tp_price=round(price * 1.03, 2),
        filters=filters,
    )
    if not opened.ok:
        print("开仓失败:", opened.message)
        return 2
    db_writer.drain()

    with session_scope() as s:
        row = (
            s.query(PositionRecord)
            .filter(PositionRecord.status == "open", PositionRecord.symbol == symbol)
            .order_by(PositionRecord.id.desc())
            .first()
        )
        local_qty = float(row.qty) if row else 0.0
        pid = row.id if row else 0
        s.expunge(row) if row else None

    algo_before = await client.open_algo_orders(symbol)
    exchange_qty = next(
        (abs(float(p.get("positionAmt", 0))) for p in await client.positions(symbol)), 0.0
    )
    ok = True
    ok &= check("开仓后数量一致", abs(local_qty - exchange_qty) < 1e-9, f"local={local_qty} exchange={exchange_qty}")
    ok &= check("保护单已挂上", len(algo_before) >= 2, f"{len(algo_before)} 条 algo")

    closed = await broker.close_position(position=row, qty=half, reason="partial_tp")
    db_writer.drain()
    ok &= check("部分平仓调用成功", closed.ok, closed.message)

    with session_scope() as s:
        pos = s.get(PositionRecord, pid)
        state = pos.status if pos else "missing"
        left_local = float(pos.qty) if pos else 0.0
        trades = (
            s.query(TradeRecord).filter(TradeRecord.mode == "demo").order_by(TradeRecord.id.desc()).all()
        )
        part_trade = trades[0] if trades else None

    exchange_left = next(
        (abs(float(p.get("positionAmt", 0))) for p in await client.positions(symbol)), 0.0
    )
    algo_after = await client.open_algo_orders(symbol)

    ok &= check("仓位仍为 open", state == "open", state)
    ok &= check("剩余数量一致", abs(left_local - exchange_left) < 1e-9, f"local={left_local} exchange={exchange_left}")
    ok &= check("剩余数量 = 原始一半", abs(left_local - (qty - half)) < 1e-9, f"{left_local}")
    ok &= check("生成了部分平仓成交", part_trade is not None and abs(part_trade.qty - half) < 1e-9,
                f"trade.qty={part_trade.qty if part_trade else '-'}")
    ok &= check("保护单未被误撤", len(algo_after) >= 2, f"{len(algo_after)} 条 algo")

    with session_scope() as s:
        pos = s.get(PositionRecord, pid)
        s.expunge(pos)
    final = await broker.close_position(position=pos, reason="final")
    db_writer.drain()
    ok &= check("全平调用成功", final.ok, final.message)

    with session_scope() as s:
        pos = s.get(PositionRecord, pid)
        ok &= check("本地状态 closed", (pos.status if pos else "") == "closed", pos.status if pos else "missing")
        total = s.query(TradeRecord).count()
    ok &= check("成交记录 2 笔", total == 2, f"{total} 笔")
    algo_end = await client.open_algo_orders(symbol)
    ok &= check("全平后保护单清空", len(algo_end) == 0, f"{len(algo_end)} 条")

    await client.close()
    print("\n=== 结论:", "全部通过" if ok else "存在失败项", "===")
    return 0 if ok else 3


def main() -> int:
    parser = argparse.ArgumentParser(description="部分止盈实盘验证")
    parser.add_argument("--symbol", default="BTCUSDT")
    args = parser.parse_args()
    return asyncio.run(run(args.symbol.upper()))


if __name__ == "__main__":
    raise SystemExit(main())
