"""回测 vs 实盘逐笔对账.

回测是"样本内"的理想化结果, 实盘才包含真实滑点、成交延迟与部分成交.
这个脚本把两边按「方向 + 开仓时间(容差)」配对, 输出:
    - 匹配率 (多少回测单在实盘里找不到)
    - 成交价偏差 (bps, 正=实盘更差)
    - 出场原因不一致的单子

用法:
    # 回测 CSV (回测页「导出 CSV」) + 实盘成交 (直接读库)
    python scripts/reconcile.py --backtest bt.csv --mode demo

    # 两边都是 CSV
    python scripts/reconcile.py --backtest bt.csv --live live.csv

    # 没有实盘样本时, 用合成数据自检脚本本身
    python scripts/reconcile.py --self-test
"""
from __future__ import annotations

import argparse
import csv
import os
import statistics
import sys
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "backend"))

TIME_FORMATS = ("%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M", "%m-%d %H:%M", "%Y/%m/%d %H:%M")


def parse_time(text: str) -> float | None:
    text = (text or "").strip().strip('"')
    if not text:
        return None
    if text.isdigit():
        value = int(text)
        return value / 1000 if value > 1e11 else float(value)
    for fmt in TIME_FORMATS:
        try:
            return datetime.strptime(text, fmt).timestamp()
        except ValueError:
            continue
    return None


def side_of(text: str) -> str:
    raw = (text or "").strip().lower()
    return "long" if raw in {"long", "多", "做多", "buy"} else "short"


def load_rows(path: str) -> list[dict]:
    rows: list[dict] = []
    with open(path, newline="", encoding="utf-8-sig") as fh:
        reader = csv.DictReader(fh)
        for raw in reader:
            ts = parse_time(raw.get("开仓时间") or raw.get("entry_time") or raw.get("opened_at") or "")
            entry = raw.get("开仓价") or raw.get("entry") or raw.get("entry_price")
            exit_price = raw.get("平仓价") or raw.get("exit") or raw.get("exit_price")
            if ts is None or entry in (None, ""):
                continue
            rows.append(
                {
                    "side": side_of(raw.get("方向") or raw.get("side") or ""),
                    "ts": ts,
                    "entry": float(entry),
                    "exit": float(exit_price) if exit_price not in (None, "") else 0.0,
                    "reason": (raw.get("出场原因") or raw.get("reason") or raw.get("exit_reason") or "").strip(),
                    "pnl": float(raw.get("盈亏") or raw.get("pnl") or 0) or 0.0,
                }
            )
    return rows


def load_live_db(mode: str) -> list[dict]:
    from nanoquant.db import init_db, session_scope
    from nanoquant.models import TradeRecord

    init_db()
    with session_scope() as s:
        rows = (
            s.query(TradeRecord)
            .filter(TradeRecord.mode == mode)
            .order_by(TradeRecord.opened_at.asc())
            .all()
        )
        return [
            {
                "side": row.side,
                "ts": float(row.opened_at),
                "entry": float(row.entry_price),
                "exit": float(row.exit_price),
                "reason": row.exit_reason,
                "pnl": float(row.pnl),
            }
            for row in rows
        ]


def match(backtest: list[dict], live: list[dict], tolerance: float) -> dict:
    used: set[int] = set()
    pairs: list[dict] = []
    unmatched_bt: list[dict] = []
    for bt in backtest:
        best_idx = -1
        best_gap = tolerance + 1
        for idx, lv in enumerate(live):
            if idx in used or lv["side"] != bt["side"]:
                continue
            gap = abs(lv["ts"] - bt["ts"])
            if gap <= tolerance and gap < best_gap:
                best_idx, best_gap = idx, gap
        if best_idx < 0:
            unmatched_bt.append(bt)
            continue
        used.add(best_idx)
        lv = live[best_idx]
        diff_bps = (lv["entry"] - bt["entry"]) / bt["entry"] * 10000 if bt["entry"] else 0.0
        pairs.append(
            {
                "gap_s": round(best_gap, 1),
                "diff_bps": round(diff_bps, 2),
                "reason_same": lv["reason"] == bt["reason"],
                "bt": bt,
                "live": lv,
            }
        )
    unmatched_live = [lv for idx, lv in enumerate(live) if idx not in used]
    diffs = [p["diff_bps"] for p in pairs]
    # 偏差可正可负 (实盘也可能比回测价更好), 用绝对值的均值/中位数估"真实摩擦"
    abs_diffs = [abs(value) for value in diffs]
    return {
        "backtest": len(backtest),
        "live": len(live),
        "matched": len(pairs),
        "unmatched_backtest": len(unmatched_bt),
        "unmatched_live": len(unmatched_live),
        "match_rate": round(len(pairs) / len(backtest) * 100, 1) if backtest else 0.0,
        "entry_diff_bps_mean": round(statistics.fmean(diffs), 2) if diffs else 0.0,
        "entry_diff_bps_max": round(max(diffs), 2) if diffs else 0.0,
        "entry_diff_bps_median_abs": round(statistics.median(abs_diffs), 2) if abs_diffs else 0.0,
        # 回测默认 slippage_bps=2 就是拍出来的: 样本足够时用它校准
        "suggested_slippage_bps": round(statistics.median(abs_diffs), 1) if abs_diffs else 0.0,
        "exit_reason_mismatch": sum(1 for p in pairs if not p["reason_same"]),
        "avg_gap_s": round(statistics.fmean([p["gap_s"] for p in pairs]), 1) if pairs else 0.0,
        "pairs": pairs[:10],
    }


def self_test() -> int:
    base = 1_790_000_000.0
    backtest = [
        {"side": "long", "ts": base + i * 300, "entry": 100 + i, "exit": 101 + i, "reason": "take_profit", "pnl": 1.0}
        for i in range(5)
    ]
    live = [
        {"side": "long", "ts": base + i * 300 + 12, "entry": 100 + i + 0.02, "exit": 101 + i,
         "reason": "take_profit" if i < 4 else "stop_loss", "pnl": 1.0}
        for i in range(5)
    ]
    live.append({"side": "short", "ts": base + 9999, "entry": 120, "exit": 119, "reason": "roi", "pnl": 0.5})
    report = match(backtest, live, 300)
    print("=== self-test ===")
    for key, value in report.items():
        if key != "pairs":
            print(f"{key}: {value}")
    return 0 if report["matched"] == 5 else 1


def main() -> int:
    parser = argparse.ArgumentParser(description="回测 vs 实盘对账")
    parser.add_argument("--backtest", help="回测成交 CSV")
    parser.add_argument("--live", help="实盘成交 CSV (省略则读库)")
    parser.add_argument("--mode", default="demo", choices=["demo", "live"], help="读库时的交易环境")
    parser.add_argument("--tolerance", type=float, default=300, help="开仓时间容差(秒)")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.backtest:
        parser.error("需要 --backtest, 或用 --self-test 自检")

    backtest = load_rows(args.backtest)
    live = load_rows(args.live) if args.live else load_live_db(args.mode)
    if not live:
        print(f"没有实盘成交样本 (mode={args.mode}); 先用 --self-test 验证工具, 或配置凭证后再跑。")
        return 2

    report = match(backtest, live, args.tolerance)
    print(
        f"回测 {report['backtest']} 笔 / 实盘 {report['live']} 笔 → 匹配 {report['matched']} "
        f"({report['match_rate']}%), 未匹配 回测{report['unmatched_backtest']}/实盘{report['unmatched_live']}"
    )
    print(
        f"入场价偏差: mean={report['entry_diff_bps_mean']}bps max={report['entry_diff_bps_max']}bps "
        f"|abs中位数={report['entry_diff_bps_median_abs']}bps | "
        f"出场原因不一致 {report['exit_reason_mismatch']} 笔 | 平均时间差 {report['avg_gap_s']}s"
    )
    if report["matched"] >= 10:
        print(
            f"建议: 把回测 slippage_bps 从默认 2 调整为约 {report['suggested_slippage_bps']}bps "
            f"（基于 {report['matched']} 笔配对样本）"
        )
    else:
        print(f"样本仅 {report['matched']} 笔, 暂不建议据此校准滑点 (至少 10 笔)")
    for pair in report["pairs"]:
        print(
            f"  {pair['bt']['side']:5s} entry {pair['bt']['entry']}→{pair['live']['entry']} "
            f"({pair['diff_bps']:+.2f}bps) 出场 {pair['bt']['reason']}→{pair['live']['reason']}"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
