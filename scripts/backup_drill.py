"""备份 → 还原演练（在真实数据的**副本**上跑，绝不碰运行库）.

为什么需要它：
    备份模块有 create/restore/sha256 校验，但"能不能还原"只被单测用几行数据证过。
    真出事时面对的是 60MB 的库、几十张表、加密 zip 和主密钥——
    这条路没走过一次，等于没有备份。

做法：
    1. 用 sqlite 的 backup API 把 data/nanoquant.db 复制到临时目录（连 WAL 一起拍平）；
    2. 复制 data/.master_key（否则备份包解不开）；
    3. 在临时库里建备份 → 故意删数据/改配置 → 还原 → 逐项比对;
    4. 打印 PASS/FAIL，返回码可直接接进发布前检查。

用法：
    python scripts/backup_drill.py            # 跑完清理临时目录
    python scripts/backup_drill.py --keep     # 保留临时目录便于排查
    python scripts/backup_drill.py --db path/to/other.db
"""
from __future__ import annotations

import argparse
import os
import shutil
import sqlite3
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REAL_DB = ROOT / "data" / "nanoquant.db"
REAL_KEY = ROOT / "data" / ".master_key"


def _copy_db(src: Path, dst: Path) -> None:
    """用 SQLite 在线备份 API：会一并把 -wal 里的未 checkpoint 事务拍平."""
    dst.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(str(src)) as source, sqlite3.connect(str(dst)) as target:
        source.backup(target)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default=str(REAL_DB))
    ap.add_argument("--keep", action="store_true", help="保留临时目录")
    args = ap.parse_args()

    src_db = Path(args.db)
    if not src_db.exists():
        print(f"[FAIL] 找不到数据库: {src_db}")
        return 2

    home = Path(tempfile.mkdtemp(prefix="nq_drill_"))
    data = home / "data"
    os.environ["NANOQUANT_HOME"] = str(home)
    os.environ["NANOQUANT_DATA_DIR"] = str(data)
    os.environ["NANOQUANT_DB"] = str(data / "drill.db")
    os.environ["NANOQUANT_KEY_FILE"] = str(data / ".master_key")
    os.environ["NANOQUANT_LOG_DIR"] = str(home / "logs")

    print(f"临时工作区: {home}")
    _copy_db(src_db, Path(os.environ["NANOQUANT_DB"]))
    if REAL_KEY.exists():
        data.mkdir(parents=True, exist_ok=True)
        shutil.copy2(REAL_KEY, data / ".master_key")
    else:
        print("[WARN] 没有 .master_key：新建的备份将用新密钥加密（旧密文仍然解不开）")

    sys.path.insert(0, str(ROOT / "backend"))
    from nanoquant.db import init_db, session_scope
    from nanoquant.models import TradeRecord, TradingSymbol
    from nanoquant.services import backup as backup_service
    from nanoquant.services.settings import get_group, update_group

    init_db()

    # 备份范围：一张大表 + 一张小表 + 两组配置，覆盖"配置/表"两条还原路径
    tables = ["trades", "trading_symbols", "positions"]
    groups = ["risk", "strategy"]
    with session_scope() as s:
        before_trades = s.query(TradeRecord).count()
        before_symbols = s.query(TradingSymbol).count()
    before_risk = dict(get_group("risk", use_cache=False))
    before_strategy = dict(get_group("strategy", use_cache=False))

    print(f"备份前: trades={before_trades} symbols={before_symbols}")
    t0 = time.time()
    job = backup_service.create_backup(
        name="drill", settings_groups=groups, tables=tables, created_by="drill"
    )
    cost = time.time() - t0
    print(f"备份完成: #{job['job_id']} {job['size'] / 1024:.1f} KB, 耗时 {cost:.1f}s, sha256={job['sha256'][:12]}…")

    failures: list[str] = []

    # 校验 1：清单行数与真实库一致
    manifest = job["manifest"]["row_counts"]
    if manifest.get("trades") != before_trades:
        failures.append(f"清单 trades={manifest.get('trades')} 与库 {before_trades} 不一致")

    # 校验 2：列表接口的 sha256 自校验必须通过
    listed = next((j for j in backup_service.list_jobs() if j["id"] == job["job_id"]), None)
    if not listed or listed.get("sha256_ok") is not True:
        failures.append("备份文件 sha256 自校验未通过")

    # 破坏现场：删掉一半成交、清空标的池、改两组配置
    with session_scope() as s:
        ids = [r.id for r in s.query(TradeRecord.id).limit(max(1, before_trades // 2)).all()]
        if ids:
            s.query(TradeRecord).filter(TradeRecord.id.in_(ids)).delete(synchronize_session=False)
        s.query(TradingSymbol).delete(synchronize_session=False)
    update_group("risk", {"risk_per_trade_pct": 99.0})
    update_group("strategy", {"ema_fast": 5})
    with session_scope() as s:
        damaged_trades = s.query(TradeRecord).count()
        damaged_symbols = s.query(TradingSymbol).count()
    print(f"破坏后: trades={damaged_trades} symbols={damaged_symbols} risk_per_trade=99 ema_fast=5")

    # 还原
    t0 = time.time()
    result = backup_service.restore(job["job_id"], restore_settings=True, restore_tables=True)
    cost = time.time() - t0
    if not result.get("ok"):
        failures.append(f"还原失败: {result.get('error')}")
    print(f"还原完成: {cost:.1f}s -> {result.get('restored')}")

    with session_scope() as s:
        after_trades = s.query(TradeRecord).count()
        after_symbols = s.query(TradingSymbol).count()
    after_risk = dict(get_group("risk", use_cache=False))
    after_strategy = dict(get_group("strategy", use_cache=False))

    if after_trades != before_trades:
        failures.append(f"还原后 trades={after_trades} != 备份前 {before_trades}")
    if after_symbols != before_symbols:
        failures.append(f"还原后 symbols={after_symbols} != 备份前 {before_symbols}")
    if abs(float(after_risk.get("risk_per_trade_pct", 0)) - float(before_risk.get("risk_per_trade_pct", 0))) > 1e-9:
        failures.append("还原后 risk 配置与备份前不一致")
    if abs(
        float(after_strategy.get("ema_fast", 0))
        - float(before_strategy.get("ema_fast", 0))
    ) > 1e-9:
        failures.append("还原后 strategy 配置与备份前不一致（改坏的 ema_fast 没有被还原覆盖）")

    print(f"还原后: trades={after_trades} symbols={after_symbols} "
          f"risk_per_trade={after_risk.get('risk_per_trade_pct')} "
          f"ema_fast={after_strategy.get('ema_fast')}")

    # 还原前自动做的 pre-restore 备份也应当可列出
    jobs = backup_service.list_jobs(limit=10)
    if not any(j.get("created_by") == "system" and "pre-restore" in (j.get("name") or "") for j in jobs):
        failures.append("没有生成 pre-restore 自保备份（还原失败时就没有退路了）")

    if args.keep:
        print(f"[保留] 临时目录未删除: {home}")
    else:
        shutil.rmtree(home, ignore_errors=True)

    if failures:
        print("\n[FAIL] 恢复演练未通过:")
        for item in failures:
            print(f"  - {item}")
        return 1
    print("\n[PASS] 备份 → 破坏 → 还原 全链路一致，sha256 自校验通过")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
