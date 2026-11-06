"""执行层端到端压测: 用本地 mock 交易所跑真实代码路径 (不需要任何凭证).

测的是"应用侧 p99":
    签名 → 下单(POST mock) → 异步落库入队 → 保护单并发
网络 RTT 由 `--delay-ms` 模拟, 便于对比"应用开销"与"网络开销"的量级。

用法:
    python scripts/bench_exec.py --orders 200 --delay-ms 0
    python scripts/bench_exec.py --orders 200 --delay-ms 120
"""
from __future__ import annotations

import argparse
import asyncio
import os
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "backend"))
sys.path.insert(0, str(ROOT / "scripts"))

# 用临时库, 绝不碰运行中的数据
os.environ["NANOQUANT_DB"] = str(Path(tempfile.mkdtemp(prefix="nq_bench_")) / "bench.db")

from mock_exchange import serve  # noqa: E402


def pct(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, int(len(ordered) * q))
    return round(ordered[idx], 3)


async def run(orders: int, delay_ms: float) -> int:
    from nanoquant.db import init_db, session_scope
    from nanoquant.exchange.broker import get_broker
    from nanoquant.exchange.client import client as binance
    from nanoquant.exchange.precision import SymbolFilters
    from nanoquant.models import PositionRecord
    from nanoquant.security import encrypt_secret
    from nanoquant.services.db_writer import db_writer
    from nanoquant.services.exec_metrics import exec_metrics
    from nanoquant.services.settings import update_group

    server = serve(port=0, delay_ms=delay_ms)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()

    init_db()
    update_group(
        "data",
        {
            "demo_base_url": f"http://127.0.0.1:{port}",
            "demo_backup_urls": [],
            "demo_api_key_enc": encrypt_secret("bench-key"),
            "demo_api_secret_enc": encrypt_secret("bench-secret"),
        },
    )

    broker = get_broker("demo")
    filters = SymbolFilters(
        symbol="BTCUSDT",
        price_tick=0.1,
        qty_step=0.001,
        min_qty=0.001,
        min_notional=5.0,
        last_price=100.0,
    )

    open_lat: list[float] = []
    errors = 0
    for _ in range(orders):
        t0 = time.perf_counter()
        res = await broker.open_position(
            symbol="BTCUSDT",
            side="long",
            qty=0.01,
            leverage=20,
            sl_price=99.0,
            tp_price=102.0,
            filters=filters,
        )
        open_lat.append((time.perf_counter() - t0) * 1000)
        if not res.ok:
            errors += 1

    db_writer.drain()

    with session_scope() as s:
        ids = [row.id for row in s.query(PositionRecord).filter(PositionRecord.status == "open").all()]

    close_lat: list[float] = []
    for pid in ids:
        with session_scope() as s:
            row = s.get(PositionRecord, pid)
            if row is None:
                continue
            s.expunge(row)
        t0 = time.perf_counter()
        res = await broker.close_position(position=row, reason="bench")
        close_lat.append((time.perf_counter() - t0) * 1000)
        if not res.ok:
            errors += 1

    db_writer.drain()
    summary = exec_metrics.summary()

    def line(name: str, values: list[float]) -> str:
        return (
            f"{name}: p50={pct(values, 0.50)}ms p95={pct(values, 0.95)}ms "
            f"p99={pct(values, 0.99)}ms max={round(max(values), 2) if values else 0}ms"
        )

    print(f"=== delay={delay_ms}ms orders={orders} errors={errors} ===")
    print(line("open ", open_lat))
    print(line("close", close_lat))
    for key in ("total_ms", "send_ms", "persist_ms", "protect_ms", "http_ms", "sign_ms"):
        stage = summary.get(key) or {}
        if stage.get("p99") is not None:
            print(
                f"  {key:11s} p50={stage['p50']} p95={stage['p95']} p99={stage['p99']} max={stage['max']}"
            )

    await binance.close()
    server.shutdown()
    return 0 if errors == 0 else 1


def main() -> int:
    parser = argparse.ArgumentParser(description="执行层应用侧 p99 压测")
    parser.add_argument("--orders", type=int, default=200)
    parser.add_argument("--delay-ms", type=float, default=0.0, help="mock 侧模拟的 RTT")
    args = parser.parse_args()
    return asyncio.run(run(args.orders, args.delay_ms))


if __name__ == "__main__":
    raise SystemExit(main())
