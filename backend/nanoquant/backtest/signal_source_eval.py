# -*- coding: utf-8 -*-
"""信号源质量评估：按频道统计跟单信号的"事后命中率"。

动机：跟单能不能赚钱，取决于**信号源**好不好，而不是执行多快。
这里对每个信号源的 open 信号做"事后验证"——信号发出后，标的在未来 N 根 K 线里
有没有朝信号方向走。命中率 + 平均收益，是判断"这个频道值不值得跟"的硬指标。

与因子 IC 同一套方法：方向 vs 未来收益。命中率 55% 以上、且多标的一致，
才值得真金白银跟；50% 左右 = 没有信息量。

数据：读 signal_follows 表（channel 列），拉 K 线算未来收益。只读不写。

用法:
  python -m nanoquant.backtest.signal_source_eval --interval 15m --forward 6
"""
from __future__ import annotations

import argparse
import bisect
import sys
from collections import defaultdict
from typing import Any

from ..db import session_scope
from ..models import SignalFollow

sys.stdout.reconfigure(encoding="utf-8", errors="replace")


def _load_klines(symbol: str, interval: str, start_ms: int, forward_ms: int) -> list[tuple[int, float]]:
    """拉 [start_ms, start_ms+forward_ms] 附近的 K 线，返回 [(open_time, close), ...]。"""
    import httpx

    out: list[tuple[int, float]] = []
    end_ms = None
    per = 1000
    earliest = start_ms
    while True:
        params: dict[str, Any] = {"symbol": symbol, "interval": interval, "limit": per}
        if end_ms is not None:
            params["endTime"] = end_ms
        resp = httpx.get("https://fapi.binance.com/fapi/v1/klines", params=params, timeout=30)
        resp.raise_for_status()
        rows = resp.json()
        if not isinstance(rows, list) or not rows:
            break
        for k in rows:
            out.append((int(k[0]), float(k[4])))
        end_ms = int(rows[0][0]) - 1
        if len(rows) < per or (out and out[-1][0] <= earliest):
            break
    out.sort(key=lambda x: x[0])
    return out


def evaluate(interval: str = "15m", forward: int = 6) -> dict[str, Any]:
    """按频道聚合 open 信号的事后命中率。"""
    # 1) 收集 open 信号
    signals: list[dict[str, Any]] = []
    with session_scope() as s:
        rows = s.query(SignalFollow).order_by(SignalFollow.created_at).all()
        for r in rows:
            payload = r.payload or {}
            ts = payload.get("signal_ts") or r.created_at
            entry = payload.get("entry") or r.price or 0.0
            if r.side in ("long", "short") and ts:
                signals.append({
                    "channel": r.channel or "(未标注)",
                    "symbol": r.symbol,
                    "side": r.side,
                    "ts": int(ts),
                    "entry": float(entry),
                    "status": r.status,
                })

    if not signals:
        return {"error": "signal_follows 里没有可评估的 open 信号（先接入信号源跑 dry_run）"}

    # 2) 按 symbol 批量拉 K 线，缓存
    cache: dict[str, list[tuple[int, float]]] = {}
    bar_ms = {"1m": 60_000, "3m": 180_000, "5m": 300_000, "15m": 900_000,
              "30m": 1_800_000, "1h": 3_600_000, "4h": 14_400_000}[interval]

    def future_return(sym: str, ts: int, entry: float) -> float | None:
        if sym not in cache:
            cache[sym] = _load_klines(sym, interval, ts - bar_ms * 10, bar_ms * (forward + 12))
        klines = cache[sym]
        i = bisect.bisect_left([t for t, _ in klines], ts)
        if i + forward >= len(klines):
            return None
        base = entry if entry > 0 else klines[i][1]
        return (klines[i + forward][1] - base) / base * 100

    # 3) 逐条算命中
    per_channel: dict[str, dict[str, Any]] = defaultdict(
        lambda: {"n": 0, "hits": 0, "ret": 0.0, "symbols": set()})
    for sig in signals:
        if sig["status"] not in ("opened", "dry_run", "processing"):
            continue
        ret = future_return(sig["symbol"], sig["ts"], sig["entry"])
        if ret is None:
            continue
        d = per_channel[sig["channel"]]
        d["n"] += 1
        d["ret"] += ret
        d["symbols"].add(sig["symbol"])
        ok = (sig["side"] == "long" and ret > 0) or (sig["side"] == "short" and ret < 0)
        if ok:
            d["hits"] += 1

    result = {
        "interval": interval,
        "forward_bars": forward,
        "channels": [
            {
                "channel": ch,
                "signals": d["n"],
                "hit_rate": round(d["hits"] / d["n"] * 100, 1) if d["n"] else 0.0,
                "avg_return_pct": round(d["ret"] / d["n"], 3) if d["n"] else 0.0,
                "symbols": sorted(d["symbols"]),
            }
            for ch, d in sorted(per_channel.items(), key=lambda kv: -kv[1]["n"])
        ],
    }
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="信号源质量评估（事后命中率）")
    parser.add_argument("--interval", default="15m")
    parser.add_argument("--forward", type=int, default=6, help="预测未来 N 根")
    args = parser.parse_args(argv)

    r = evaluate(args.interval, args.forward)
    if r.get("error"):
        print(r["error"])
        return 1
    print(f"{'频道':20} {'信号':>5} {'命中率':>8} {'平均收益%':>10}  标的")
    print("-" * 72)
    for c in r["channels"]:
        verdict = "值得跟" if c["hit_rate"] >= 55 else ("存疑" if c["hit_rate"] >= 52 else "无信息量")
        print(f"{c['channel']:20} {c['signals']:>5} {c['hit_rate']:>7.1f}% "
              f"{c['avg_return_pct']:>9.2f}%  {','.join(c['symbols']):10}  {verdict}")
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
