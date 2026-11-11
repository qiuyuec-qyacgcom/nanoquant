# -*- coding: utf-8 -*-
"""最终裁决：trend vs meanrev 在指定周期长样本上的 walk-forward 样本外表现。

用法（在 backend 目录下，或从项目根）:
  PYTHONPATH=backend python scripts/final_verdict.py --interval 15m --bars 25000
  PYTHONPATH=backend python scripts/final_verdict.py --interval 15m --symbols BTCUSDT,ETHUSDT --bars 9000
"""
import argparse
import sys
import time

sys.stdout.reconfigure(encoding="utf-8", errors="replace")

from nanoquant.backtest.walkforward import run_walkforward
from nanoquant.backtest.factor_ic import _load_klines
from nanoquant.services.settings import StrategySettings

TREND_GRID = {
    "entry_threshold": [0.30, 0.45],
    "sl_atr_mult": [1.0, 1.5],
    "tp_atr_mult": [2.0, 3.0],
}
MR_GRID = {
    "mode": ["meanrev"],
    "mr_z_entry": [1.5, 2.0, 2.5],
    "mr_min_risk_reward": [0.6, 0.9],
}


def main() -> int:
    parser = argparse.ArgumentParser(description="trend vs meanrev 长样本 walk-forward 裁决")
    parser.add_argument("--interval", default="15m", help="主周期（默认 15m）")
    parser.add_argument("--symbols", default="BTCUSDT,ETHUSDT,SOLUSDT")
    parser.add_argument("--bars", type=int, default=25000, help="每标的 K 线根数（分页取）")
    parser.add_argument("--folds", type=int, default=4)
    parser.add_argument("--train-bars", type=int, default=600)
    args = parser.parse_args()

    interval = args.interval
    symbols = [s.strip() for s in args.symbols.split(",") if s.strip()]
    bars = args.bars
    folds = args.folds
    train = args.train_bars

    def load(sym):
        t0 = time.perf_counter()
        out = _load_klines(sym, interval, bars)
        print(f"  {sym}: {len(out)} 根  ({time.perf_counter()-t0:.1f}s)", flush=True)
        return out

    def run(sym, candles, mode, grid):
        base = StrategySettings(timeframe=interval, mode=mode)
        return run_walkforward(candles, base, symbol=sym, grid=grid, folds=folds, train_bars=train)

    print("=" * 108, flush=True)
    print(f"最终裁决 walk-forward：{interval}，每标的 {bars} 根，{folds} 折滚动，训练窗 {train} 根", flush=True)
    print("=" * 108, flush=True)

    data = {sym: load(sym) for sym in symbols}

    for mode, grid, label in (("trend", TREND_GRID, "trend 趋势跟随"), ("meanrev", MR_GRID, "meanrev 均值回归")):
        print(flush=True)
        print(f"### {label}", flush=True)
        hdr = (f"{'标的':10} {'折':>3} {'OOS笔':>6} {'OOS收益%':>10} {'最差折%':>9} {'正折':>7} "
               f"{'过拟合缺口%':>11} {'bootCI':>20} {'P(正)':>7} {'成本×2':>9}")
        print(hdr, flush=True)
        print("-" * len(hdr), flush=True)
        for sym in symbols:
            t0 = time.perf_counter()
            r = run(sym, data[sym], mode, grid)
            if r.get("error"):
                print(f"{sym:10} {r['error']}  ({time.perf_counter()-t0:.0f}s)", flush=True)
                continue
            a = r["aggregate"]
            b = r.get("bootstrap", {})
            cs = r.get("cost_sensitivity", {})
            ci = f"[{b.get('ci_low')},{b.get('ci_high')}]" if b else "-"
            print(
                f"{sym:10} {r['folds']:>3} {a['oos_trades']:>6} {a['oos_return_pct']:>10.2f} "
                f"{a['oos_worst_fold_pct']:>9.2f} {a['positive_folds']}/{r['folds']:<4} "
                f"{a['overfit_gap_pct']:>11.2f} {ci:>20} {b.get('p_positive', 0):>7.3f} "
                f"{cs.get('stressed_avg_return_pct', 0):>9.2f}  ({time.perf_counter()-t0:.0f}s)",
                flush=True,
            )
    print(flush=True)
    print("done", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
