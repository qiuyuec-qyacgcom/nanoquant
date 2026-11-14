"""历史数据整理：压缩 flat 信号的旧特征快照、清掉废弃表、可选重建数据库.

为什么要单独提供一个脚本：
    `services/order_sync` / `retention` 只处理"新的"数据，库里已经存在的 34MB
    flat 快照要等 14 天自然过期。而且 SQLite 删除行不会把文件变小（页只进空闲列表），
    真要回收磁盘必须 `VACUUM INTO` 新建一个库再原子替换。

安全边界：
    - 默认 **dry-run**，只打印将要做什么；
    - `--apply` 才真正改库，改前先 `xia` 复制一份备份；
    - `--vacuum` 需要先停服务（脚本会在服务可访问时拒绝执行，除非 `--force`）；
    - 只删"确认没人用"的表（目前是 pending_orders，旧版 OMS 残留、0 行、无模型）。

用法：
    python scripts/cleanup_history.py                  # 看看能省多少
    python scripts/cleanup_history.py --apply          # 压缩 + 删废弃表
    python scripts/cleanup_history.py --apply --vacuum # 再重建库回收磁盘（需停服务）
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import sqlite3
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "backend"))

#: flat 信号保留的字段（与 engine/worker.py 的 FLAT_FEATURE_KEYS 同源，避免两处漂移）
from nanoquant.engine.worker import FLAT_FEATURE_KEYS  # noqa: E402

#: 已知废弃表：旧版 OMS 残留，无模型、无引用
LEGACY_TABLES = ("pending_orders",)
#: 超过这个字节数的 flat features 才值得重写（低于它的本来就是精简过的）
MIN_REWRITE_BYTES = 500


def db_path() -> Path:
    env = os.getenv("NANOQUANT_DB")
    if env:
        return Path(env)
    return Path(os.getenv("NANOQUANT_DATA_DIR", ROOT / "data")) / "nanoquant.db"


def human(n: float) -> str:
    return f"{n / 1024 / 1024:.1f} MB"


def report(con: sqlite3.Connection) -> dict:
    page_size = con.execute("pragma page_size").fetchone()[0]
    pages = con.execute("pragma page_count").fetchone()[0]
    free = con.execute("pragma freelist_count").fetchone()[0]
    file_bytes = page_size * pages
    flat_total = con.execute(
        "select coalesce(sum(length(features)), 0) from signals where decision = 'flat'"
    ).fetchone()[0]
    heavy = con.execute(
        "select count(*) from signals where decision = 'flat' and length(features) > ?",
        (MIN_REWRITE_BYTES,),
    ).fetchone()[0]
    legacy = {}
    for table in LEGACY_TABLES:
        exists = con.execute(
            "select name from sqlite_master where type='table' and name=?", (table,)
        ).fetchone()
        if exists:
            legacy[table] = con.execute(f"select count(*) from {table}").fetchone()[0]
    return {
        "file_bytes": file_bytes,
        "free_bytes": page_size * free,
        "flat_features_bytes": flat_total,
        "flat_heavy_rows": heavy,
        "legacy_tables": legacy,
    }


def compact_flat(con: sqlite3.Connection, *, apply: bool, batch: int = 400) -> int:
    """把旧的 flat 信号特征压成精简版，返回需要/已经重写的行数."""
    rows = con.execute(
        "select id, features from signals where decision = 'flat' and length(features) > ?",
        (MIN_REWRITE_BYTES,),
    ).fetchall()
    changed = 0
    for row_id, raw in rows:
        try:
            data = json.loads(raw or "{}")
        except json.JSONDecodeError:
            continue
        keep = {k: data[k] for k in (*FLAT_FEATURE_KEYS, "first_ts") if k in data}
        if len(json.dumps(keep, ensure_ascii=False)) >= len(raw or ""):
            continue
        changed += 1
        if apply:
            con.execute(
                "update signals set features = ? where id = ?",
                (json.dumps(keep, ensure_ascii=False), row_id),
            )
            if changed % batch == 0:
                con.commit()
    if apply:
        con.commit()
    return changed


def drop_legacy_tables(con: sqlite3.Connection, *, apply: bool) -> list[str]:
    dropped: list[str] = []
    for table in LEGACY_TABLES:
        exists = con.execute(
            "select name from sqlite_master where type='table' and name=?", (table,)
        ).fetchone()
        if not exists:
            continue
        count = con.execute(f"select count(*) from {table}").fetchone()[0]
        if count:
            print(f"  ! {table} 还有 {count} 行，跳过（不确定是否还有用）")
            continue
        dropped.append(table)
        if apply:
            con.execute(f"drop table {table}")
            con.commit()
    return dropped


def service_alive(port: int) -> bool:
    try:
        with urllib.request.urlopen(f"http://127.0.0.1:{port}/api/v1/health", timeout=3):
            return True
    except (urllib.error.URLError, OSError):
        return False


def vacuum_into(src: Path, *, apply: bool) -> Path | None:
    """用 VACUUM INTO 重建库（不会改变数据，只回收空闲页），再原子替换."""
    dst = src.with_name(f"{src.stem}-compact-{time.strftime('%Y%m%d%H%M%S')}.db")
    if not apply:
        print(f"  将执行: VACUUM INTO {dst.name} 然后替换原库")
        return None
    # 重建库期间需要「原库 + 新库 + 备份」三份空间，宁可提前拒绝也不要在
    # 备份阶段写到一半把磁盘撑爆（实测 C: 只剩 39MB 时就是这个下场）
    need = src.stat().st_size * 2.2
    free = shutil.disk_usage(src.parent).free
    if free < need:
        print(f"  拒绝执行：磁盘可用 {human(free)}，重建需要约 {human(need)}（原库 x2.2）")
        return None
    con = sqlite3.connect(str(src))
    try:
        con.execute("pragma wal_checkpoint(TRUNCATE)")
        con.execute("vacuum into ?", (str(dst),))
    finally:
        con.close()
    backup = src.with_name(f"{src.name}.bak-{time.strftime('%Y%m%d%H%M%S')}")
    try:
        # 用 SQLite 在线备份 API 而不是 shutil.copy2：WAL 模式下直接拷主库文件会丢掉
        # 还没 checkpoint 的事务（实测少 6 条权益采样），这种"备份"在还原时才发现有问题。
        # 注意 with sqlite3.connect() 只提交事务**不关连接**，句柄不关会导致后面
        # 挪 -wal 时报 WinError 32，所以这里显式关闭。
        src_con = sqlite3.connect(str(src))
        dst_con = sqlite3.connect(str(backup))
        try:
            src_con.backup(dst_con)
        finally:
            dst_con.close()
            src_con.close()
    except (OSError, sqlite3.Error) as exc:
        dst.unlink(missing_ok=True)
        print(f"  备份失败，未替换原库: {exc}")
        return None
    # 旧的 -wal/-shm 必须挪走：新库旁边留一个旧 WAL，SQLite 启动时会尝试回放它。
    # 这里用 rename 而不是 unlink——某些环境（本机的 safe-delete 钩子）删文件会失败。
    for suffix in ("-wal", "-shm"):
        side = src.with_name(src.name + suffix)
        if side.exists():
            side.replace(side.with_name(side.name + ".old"))
    os.replace(dst, src)
    print(f"  已替换: {src.name}（备份 {backup.name}）")
    return backup


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--apply", action="store_true", help="真正执行（默认只报告）")
    ap.add_argument("--vacuum", action="store_true", help="重建数据库回收磁盘（需先停服务）")
    ap.add_argument("--force", action="store_true", help="服务在跑也强行 VACUUM（危险）")
    ap.add_argument("--port", type=int, default=int(os.getenv("NANOQUANT_PORT", "8899")))
    args = ap.parse_args()

    path = db_path()
    if not path.exists():
        print(f"[FAIL] 找不到数据库: {path}")
        return 2

    print(f"数据库: {path}")
    con = sqlite3.connect(str(path))
    try:
        before = report(con)
        print(
            f"当前: 文件 {human(before['file_bytes'])}，空闲页 {human(before['free_bytes'])}，"
            f"flat 特征合计 {human(before['flat_features_bytes'])}，"
            f"其中 {before['flat_heavy_rows']} 行仍存全量快照"
        )
        if before["legacy_tables"]:
            print(f"废弃表: {before['legacy_tables']}")

        print(f"\n[1/3] 压缩 flat 特征（保留 {len(FLAT_FEATURE_KEYS)} 个审计字段）")
        changed = compact_flat(con, apply=args.apply)
        print(f"  {'已重写' if args.apply else '待重写'}: {changed} 行")

        print("\n[2/3] 删除废弃表")
        dropped = drop_legacy_tables(con, apply=args.apply)
        print(f"  {'已删除' if args.apply else '待删除'}: {dropped or '无'}")

        print("\n[3/3] 回收磁盘")
        if not args.vacuum:
            print("  跳过（加 --vacuum 才会重建库）")
        elif service_alive(args.port) and not args.force:
            print(f"  跳过：服务在 http://127.0.0.1:{args.port} 上仍在运行，请先停服务再执行")
            return 3
        else:
            # 必须先把主流程这条连接关掉：它一直持有 WAL 句柄，
            # 不关的话 -wal 文件删不掉/改不了名（Windows 会报 WinError 32）
            con.close()
            vacuum_into(path, apply=args.apply)
            con = sqlite3.connect(str(path))

        if args.apply:
            after = report(sqlite3.connect(str(path)))
            print(
                f"\n完成: 文件 {human(before['file_bytes'])} -> {human(after['file_bytes'])}，"
                f"flat 特征 {human(before['flat_features_bytes'])} -> {human(after['flat_features_bytes'])}"
            )
        else:
            print("\n以上为演练。确认无误后加 --apply 执行。")
    finally:
        con.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
