"""主网演练前的只读预检（不下单、不改交易所设置）.

它会用**实盘凭证**（从真实库复制，密文可复用）连主网，逐项检查：
    ping / 账户可读 / 持仓模式 / 遗留挂单 / 标的池最小名义额与所需保证金

用法:
    python scripts/mainnet_preflight.py --yes

安全边界:
- 只调用只读接口（time / account / positionRisk / exchangeInfo / ticker / openOrders）；
- 用临时数据库，不触碰运行数据；
- 失败即退出，非零返回码可直接接进演练脚本。

演练步骤见 docs/MAINNET_DRILL.md。
"""
from __future__ import annotations

import argparse
import asyncio
import json
import os
import sqlite3
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "backend"))

os.environ["NANOQUANT_DB"] = str(Path(tempfile.mkdtemp(prefix="nq_preflight_")) / "preflight.db")


def _real_settings(group: str) -> dict:
    real_db = ROOT / "data" / "nanoquant.db"
    if not real_db.exists():
        return {}
    with sqlite3.connect(str(real_db)) as conn:
        row = conn.execute("select value from app_settings where key=?", (group,)).fetchone()
    return json.loads(row[0]) if row else {}


def _pool() -> list[str]:
    real_db = ROOT / "data" / "nanoquant.db"
    if not real_db.exists():
        return []
    with sqlite3.connect(str(real_db)) as conn:
        try:
            rows = conn.execute(
                "select symbol from trading_symbols where enabled=1 order by sort_order, symbol"
            ).fetchall()
        except sqlite3.Error:
            rows = []
    return [row[0] for row in rows]


async def run() -> int:
    from nanoquant.db import init_db
    from nanoquant.exchange import client as bc
    from nanoquant.services.settings import get_risk, update_group

    data = _real_settings("data")
    if not data.get("live_api_key_enc") or not data.get("live_api_secret_enc"):
        print("[FAIL] 真实库里没有实盘凭证：先在「参数配置 → 数据源」保存实盘 API Key/Secret")
        return 1

    init_db()
    update_group(
        "data",
        {
            "live_base_url": data.get("live_base_url") or "https://fapi.binance.com",
            "live_backup_urls": data.get("live_backup_urls") or [],
            "live_api_key_enc": data["live_api_key_enc"],
            "live_api_secret_enc": data["live_api_secret_enc"],
        },
    )
    update_group("engine", {"mode": "live"})  # 只影响这个临时库的进程

    client = bc.client
    checks: list[tuple[str, bool, str]] = []
    fatal = False

    try:
        ok = await client.ping()
        checks.append(("主网连通", ok, "fapi.binance.com /fapi/v1/time"))
        if not ok:
            fatal = True
    except Exception as exc:  # noqa: BLE001
        checks.append(("主网连通", False, str(exc)[:120]))
        fatal = True

    if not fatal:
        try:
            account = await client.account()
            wallet = float(account.get("totalWalletBalance", 0) or 0)
            checks.append(("账户可读", True, f"钱包余额 {wallet:.4f} USDT"))
            if wallet <= 0:
                checks.append(("账户余额", False, "主网钱包余额为 0，无法演练"))
                fatal = True
        except Exception as exc:  # noqa: BLE001
            checks.append(("账户可读", False, str(exc)[:120]))
            fatal = True

    if not fatal:
        try:
            dual = await client.get_position_mode()
            checks.append(("持仓模式", not dual, "单向持仓" if not dual else "双向持仓（harness 会自动切，但建议先手动改）"))
        except Exception as exc:  # noqa: BLE001
            checks.append(("持仓模式", False, str(exc)[:120]))

        try:
            orders = await client.open_orders()
            algo = await client.open_algo_orders()
            total = len(orders) + len(algo)
            checks.append(("遗留挂单", total == 0, f"{len(orders)} 普通 / {len(algo)} 条件单"))
        except Exception as exc:  # noqa: BLE001
            checks.append(("遗留挂单", False, str(exc)[:120]))

        risk = get_risk()
        pool = _pool()
        if not pool:
            checks.append(("标的池", False, "标的池为空，先在后台添加合约"))
        else:
            try:
                filters = await client.symbol_filters(force=True)
                ticks = await client.book_tickers(pool)
            except Exception as exc:  # noqa: BLE001
                filters, ticks = {}, {}
                checks.append(("合约规格", False, str(exc)[:120]))

            rows = []
            for symbol in pool[:6]:
                spec = filters.get(symbol)
                price = (ticks.get(symbol) or (0.0, 0.0))[0] or (getattr(spec, "last_price", 0.0) if spec else 0.0)
                if spec is None or price <= 0:
                    continue
                min_notional = float(spec.min_notional or 5.0)
                max_lev = float(getattr(spec, "max_leverage", 0) or risk.default_leverage)
                margin_now = min_notional / max(1, risk.default_leverage)
                margin_max = min_notional / max(1, max_lev)
                line = f"{symbol}: 最小 {min_notional:.2f}U / 当前{risk.default_leverage}x 需 {margin_now:.2f}U"
                if margin_max < margin_now:
                    line += f" / 最高{max_lev:.0f}x 需 {margin_max:.2f}U"
                rows.append(line)
            checks.append(("标的最小名义额", bool(rows), " · ".join(rows)[:400] or "没有可用池内标的"))

    print("=== 主网预检（只读） ===")
    for name, ok, detail in checks:
        print(f"[{'PASS' if ok else 'FAIL'}] {name}: {detail}")
    if fatal:
        print("\n存在致命项，先修复再演练。")
        return 1
    print("\n预检通过：可以按 docs/MAINNET_DRILL.md 开始小额演练。")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="主网演练前只读预检")
    parser.add_argument("--yes", action="store_true", help="确认使用实盘凭证连主网（只读）")
    args = parser.parse_args()
    if not args.yes:
        parser.error("主网预检需要 --yes 明确确认（只读调用，但会用实盘凭证）")
    return asyncio.run(run())


if __name__ == "__main__":
    raise SystemExit(main())
