"""公网 RTT 长尾采样: 不需要任何凭证, 用来确定"网络地板".

背景: 真实下单 p99 需要测试网凭证, 但其中最大的一块是网络往返.
先用公共接口把 RTT 的 p50/p95/p99 量出来, 就能判断应用侧优化(Rust)的收益上界:
    总延迟 ≈ RTT + 应用侧开销
如果 RTT 的 p99 已经是几百毫秒, 把应用侧从 2ms 优化到 0.2ms 基本没有意义。

用法:
    python scripts/rtt_probe.py --seconds 60 --rate 4
    python scripts/rtt_probe.py --target mainnet --seconds 30
"""
from __future__ import annotations

import argparse
import asyncio
import statistics
import time

import httpx

TARGETS = {
    "testnet": "https://testnet.binancefuture.com",
    "mainnet": "https://fapi.binance.com",
}


def _pct(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, int(len(ordered) * q))
    return ordered[idx]


async def probe(target: str, seconds: float, rate: float, path: str) -> dict:
    base = TARGETS[target]
    interval = 1.0 / max(0.1, rate)
    samples: list[float] = []
    errors = 0
    deadline = time.time() + seconds
    async with httpx.AsyncClient(timeout=15, http2=False) as client:
        # 预热: 握手 + DNS 不计入正式采样
        try:
            await client.get(f"{base}/fapi/v1/time")
        except Exception:  # noqa: BLE001
            pass
        while time.time() < deadline:
            started = time.perf_counter()
            try:
                r = await client.get(f"{base}{path}")
                if r.status_code == 200:
                    samples.append((time.perf_counter() - started) * 1000)
                else:
                    errors += 1
            except Exception:  # noqa: BLE001
                errors += 1
            await asyncio.sleep(interval)
    return {
        "target": target,
        "path": path,
        "count": len(samples),
        "errors": errors,
        "mean": round(statistics.fmean(samples), 2) if samples else 0.0,
        "p50": round(_pct(samples, 0.50), 2),
        "p95": round(_pct(samples, 0.95), 2),
        "p99": round(_pct(samples, 0.99), 2),
        "max": round(max(samples), 2) if samples else 0.0,
    }


async def main() -> int:
    parser = argparse.ArgumentParser(description="公网 RTT 采样")
    parser.add_argument("--target", choices=list(TARGETS), default="testnet")
    parser.add_argument("--seconds", type=float, default=60)
    parser.add_argument("--rate", type=float, default=4, help="每秒请求数")
    parser.add_argument("--path", default="/fapi/v1/time", help="采样接口(权重 1)")
    args = parser.parse_args()

    result = await probe(args.target, args.seconds, args.rate, args.path)
    print(
        f"{result['target']} {result['path']}  n={result['count']} err={result['errors']}  "
        f"mean={result['mean']}ms p50={result['p50']}ms p95={result['p95']}ms "
        f"p99={result['p99']}ms max={result['max']}ms"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
