"""下单路径本地 p99 基线 (无需交易所凭证).

只压测我们**可控**的那几段: 签名、落库、回执序列化。
网络 RTT 必须用真实凭证在 demo/实盘上测，这里不覆盖，也不应该用假数据代替。

用法:
    python scripts/p99_bench.py [N]      # 默认 N=3000

输出: 每段的 p50 / p95 / p99 / max (毫秒)。
"""
from __future__ import annotations

import hashlib
import hmac
import json
import sqlite3
import statistics
import sys
import tempfile
import time
from pathlib import Path
from urllib.parse import urlencode

SECRET = "bench-secret-key"


def percentile(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    pos = (len(ordered) - 1) * q
    low = int(pos)
    high = min(low + 1, len(ordered) - 1)
    weight = pos - low
    return ordered[low] * (1 - weight) + ordered[high] * weight


def report(name: str, samples: list[float]) -> None:
    print(
        f"{name:<12} n={len(samples):<6} "
        f"p50={percentile(samples, 0.50):7.3f}ms  "
        f"p95={percentile(samples, 0.95):7.3f}ms  "
        f"p99={percentile(samples, 0.99):7.3f}ms  "
        f"max={max(samples):7.3f}ms  "
        f"mean={statistics.fmean(samples):7.3f}ms"
    )


def bench_sign(n: int) -> list[float]:
    payload = {
        "symbol": "BTCUSDT",
        "side": "BUY",
        "type": "MARKET",
        "quantity": "0.002",
        "newClientOrderId": "nq0000000000000000000000000001",
        "recvWindow": 5000,
        "timestamp": 1793700000000,
    }
    samples: list[float] = []
    for i in range(n):
        payload["timestamp"] = 1793700000000 + i
        started = time.perf_counter()
        query = urlencode(payload, doseq=True)
        signature = hmac.new(SECRET.encode(), query.encode(), hashlib.sha256).hexdigest()
        url = f"/fapi/v1/order?{query}&signature={signature}"
        samples.append((time.perf_counter() - started) * 1000)
        if len(url) < 0:  # pragma: no cover - 防止被优化掉
            print(url)
    return samples


def bench_serialize(n: int) -> list[float]:
    resp = {
        "orderId": 1234567890,
        "symbol": "BTCUSDT",
        "status": "FILLED",
        "clientOrderId": "nq0000000000000000000000000001",
        "price": "0.00",
        "avgPrice": "78526.20",
        "origQty": "0.002",
        "executedQty": "0.002",
        "cumQuote": "157.0524",
        "timeInForce": "GTC",
        "type": "MARKET",
        "reduceOnly": False,
        "closePosition": False,
        "side": "BUY",
        "positionSide": "BOTH",
        "stopPrice": "0.00",
        "workingType": "CONTRACT_PRICE",
        "priceProtect": False,
        "origType": "MARKET",
        "time": 1793700000000,
        "updateTime": 1793700000123,
    }
    keys = ("orderId", "status", "avgPrice", "executedQty", "cumQuote", "updateTime", "clientOrderId")
    samples: list[float] = []
    for _ in range(n):
        started = time.perf_counter()
        compact = {k: resp.get(k) for k in keys if resp.get(k) is not None}
        json.dumps(compact)
        json.dumps(resp)  # 对照: 旧实现写的是整份回执
        samples.append((time.perf_counter() - started) * 1000)
    return samples


def bench_persist(n: int) -> list[float]:
    tmp = Path(tempfile.mkdtemp(prefix="nq_bench_")) / "bench.db"
    conn = sqlite3.connect(tmp)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.execute(
        """
        CREATE TABLE orders (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts INTEGER, symbol TEXT, side TEXT, order_type TEXT,
            qty REAL, price REAL, status TEXT, purpose TEXT,
            client_order_id TEXT, exchange_order_id TEXT, reduce_only INTEGER,
            mode TEXT, latency_ms INTEGER, raw TEXT
        )
        """
    )
    conn.execute(
        """
        CREATE TABLE positions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            symbol TEXT, side TEXT, qty REAL, entry_price REAL, mark_price REAL,
            leverage REAL, margin REAL, sl_price REAL, tp_price REAL, peak_price REAL,
            opened_at INTEGER, mode TEXT, status TEXT, entry_slippage_bps REAL
        )
        """
    )
    conn.commit()
    samples: list[float] = []
    raw = json.dumps({"orderId": 1, "status": "FILLED", "avgPrice": "78526.2"})
    for i in range(n):
        started = time.perf_counter()
        with conn:
            conn.execute(
                "INSERT INTO orders (ts,symbol,side,order_type,qty,price,status,purpose,"
                "client_order_id,exchange_order_id,reduce_only,mode,latency_ms,raw) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                (1793700000 + i, "BTCUSDT", "BUY", "MARKET", 0.002, 78526.2, "FILLED",
                 "entry", "nq1", "1", 0, "demo", 12, raw),
            )
            conn.execute(
                "INSERT INTO positions (symbol,side,qty,entry_price,mark_price,leverage,margin,"
                "sl_price,tp_price,peak_price,opened_at,mode,status,entry_slippage_bps) "
                "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                ("BTCUSDT", "long", 0.002, 78526.2, 78526.2, 20, 7.85,
                 77000.0, 80000.0, 78526.2, 1793700000 + i, "demo", "open", 0.8),
            )
        samples.append((time.perf_counter() - started) * 1000)
    conn.close()
    try:
        tmp.unlink()
    except OSError:
        pass
    return samples


def main() -> None:
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 3000
    print(f"nanoquant 下单路径本地基线 (N={n})")
    print("-" * 100)
    report("sign", bench_sign(n))
    report("serialize", bench_serialize(n))
    report("persist", bench_persist(min(n, 1500)))
    print("-" * 100)
    print("说明: 以上不含交易所网络 RTT; 真实 p99 需在 demo/实盘用 /engine/execution 观测。")


if __name__ == "__main__":
    main()
