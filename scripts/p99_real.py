"""真实下单 p99 采样（测试网）.

按固定频率「开仓 → 平仓」，采集真实链路的分段耗时：
    plan / wait / sign / http / persist / protect / total
输出 p50/p95/p99/max，用于和 mock 压测的应用侧基线对比。

用法:
    python scripts/p99_real.py --orders 15 --per-minute 5
    python scripts/p99_real.py --orders 30 --per-minute 12 --symbol ETHUSDT

注意:
- 使用临时数据库，不污染运行数据；凭证从当前配置复制（主密钥相同，密文可复用）。
- 每单都是最小名义额（约 5 USDT），只用于测延迟，不构成策略。
"""
from __future__ import annotations

import argparse
import asyncio
import math
import os
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "backend"))

os.environ["NANOQUANT_DB"] = str(Path(tempfile.mkdtemp(prefix="nq_p99_")) / "p99.db")


def pct(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, int(len(ordered) * q))
    return round(ordered[idx], 2)


async def run(orders: int, per_minute: float, symbol: str) -> int:
    from nanoquant.db import init_db, session_scope
    from nanoquant.exchange import client as bc
    from nanoquant.exchange.broker import get_broker
    from nanoquant.models import PositionRecord
    from nanoquant.services.db_writer import db_writer
    from nanoquant.services.exec_metrics import exec_metrics
    from nanoquant.services.settings import update_group

    # 直接从真实库读 data 分组（此时 ORM 指向临时库，还没建表）
    import json
    import sqlite3

    real_db = ROOT / "data" / "nanoquant.db"
    with sqlite3.connect(str(real_db)) as conn:
        row = conn.execute("select value from app_settings where key='data'").fetchone()
    real = json.loads(row[0]) if row else {}
    if not real.get("demo_api_key_enc"):
        print("真实库里没有模拟盘凭证")
        return 1

    init_db()
    # 复制凭证（密文与主密钥同源，可直接复用）
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
    filters_map = await client.symbol_filters(force=True)
    filters = filters_map.get(symbol)
    if filters is None:
        print(f"没有 {symbol} 的合约信息")
        return 1

    price = filters.last_price
    if price <= 0:
        tick = (await client.book_tickers([symbol])).get(symbol)
        price = tick[0] if tick else 0
    if price <= 0:
        print("取不到价格")
        return 1

    step = filters.qty_step or 0.001
    min_notional = max(filters.min_notional or 5.0, 5.0) * 1.2
    qty = math.ceil((min_notional / price) / step) * step
    qty = max(qty, filters.min_qty or step)
    filters.last_price = price  # 回执缺均价时用它兜底
    print(f"symbol={symbol} price={price} qty={qty} interval={60 / per_minute:.1f}s")

    broker = get_broker("demo")
    interval = 60.0 / max(0.1, per_minute)
    lat: list[float] = []
    ok_count = 0
    for i in range(orders):
        t0 = time.perf_counter()
        res = await broker.open_position(
            symbol=symbol,
            side="long",
            qty=qty,
            leverage=5,
            sl_price=round(price * 0.97, 2),
            tp_price=round(price * 1.03, 2),
            filters=filters,
        )
        cost = (time.perf_counter() - t0) * 1000
        lat.append(cost)
        if not res.ok:
            print(f"#{i + 1} 开仓失败: {res.message[:90]}")
            await asyncio.sleep(interval)
            continue

        db_writer.drain()
        with session_scope() as s:
            row = (
                s.query(PositionRecord)
                .filter(PositionRecord.status == "open", PositionRecord.symbol == symbol)
                .order_by(PositionRecord.id.desc())
                .first()
            )
            if row is not None:
                s.expunge(row)
        if row is not None:
            close = await broker.close_position(position=row, reason="p99_probe")
            if not close.ok:
                print(f"#{i + 1} 平仓失败: {close.message[:90]}")
            else:
                ok_count += 1
        print(f"#{i + 1} open={cost:.0f}ms total_ok={ok_count}")
        await asyncio.sleep(interval)

    db_writer.drain()
    summary = exec_metrics.summary()
    print(f"\n=== 完成 {ok_count}/{orders} 组 ===")
    print(f"open 整单: p50={pct(lat, 0.5)}ms p95={pct(lat, 0.95)}ms p99={pct(lat, 0.99)}ms max={round(max(lat), 2)}ms")
    for key in ("total_ms", "send_ms", "persist_ms", "protect_ms", "http_ms", "sign_ms", "wait_ms"):
        stage = summary.get(key) or {}
        if stage.get("p99") is not None:
            print(f"  {key:11s} p50={stage['p50']} p95={stage['p95']} p99={stage['p99']} max={stage['max']}")
    await client.close()
    return 0 if ok_count else 2


def main() -> int:
    parser = argparse.ArgumentParser(description="真实下单 p99 采样")
    parser.add_argument("--orders", type=int, default=15)
    parser.add_argument("--per-minute", type=float, default=5)
    parser.add_argument("--symbol", default="BTCUSDT")
    args = parser.parse_args()
    return asyncio.run(run(args.orders, args.per_minute, args.symbol.upper()))


if __name__ == "__main__":
    raise SystemExit(main())
