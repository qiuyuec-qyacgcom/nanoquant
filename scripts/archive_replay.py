"""把 data/archive/*.jsonl.gz 导入一个临时 SQLite 表, 便于离线复盘.

用法:
    python scripts/archive_replay.py data/archive/signals-20261104.jsonl.gz
    python scripts/archive_replay.py data/archive/events-20261104.jsonl.gz --db /tmp/replay.db

导入后每行原始 JSON 存在 `rows.payload`, 另外抽出 ts/level/category/symbol 等常用列,
可以直接用 SQL 查时间分布, 例如:
    sqlite> SELECT substr(datetime(ts,'unixepoch'),1,13), count(*) FROM rows GROUP BY 1;
"""
from __future__ import annotations

import argparse
import gzip
import json
import sqlite3
from pathlib import Path

COLUMNS = ("ts", "level", "category", "symbol", "direction", "decision", "reject_reason", "kind")


def main() -> int:
    parser = argparse.ArgumentParser(description="归档 gzip JSONL → 临时 SQLite")
    parser.add_argument("path", help="归档文件路径")
    parser.add_argument("--db", default="", help="输出 SQLite 路径 (默认与归档同名 .replay.db)")
    parser.add_argument("--limit", type=int, default=0, help="只导入前 N 行 (0=全部)")
    args = parser.parse_args()

    src = Path(args.path)
    if not src.exists():
        print(f"文件不存在: {src}")
        return 1
    out = Path(args.db) if args.db else src.with_suffix(".replay.db")

    conn = sqlite3.connect(out)
    conn.execute("DROP TABLE IF EXISTS rows")
    conn.execute(
        "CREATE TABLE rows (id INTEGER PRIMARY KEY AUTOINCREMENT, "
        + ", ".join(f"{c} TEXT" for c in COLUMNS)
        + ", payload TEXT NOT NULL)"
    )

    count = 0
    ts_values: list[int] = []
    with gzip.open(src, "rt", encoding="utf-8") as fh:
        for line in fh:
            if args.limit and count >= args.limit:
                break
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            values = [str(row.get(c, "")) if row.get(c) is not None else "" for c in COLUMNS]
            conn.execute(
                f"INSERT INTO rows ({', '.join(COLUMNS)}, payload) VALUES ({', '.join('?' * (len(COLUMNS) + 1))})",
                (*values, line),
            )
            if isinstance(row.get("ts"), int):
                ts_values.append(row["ts"])
            count += 1

    conn.commit()
    print(f"导入 {count} 行 → {out}")
    if ts_values:
        import datetime as dt

        lo = dt.datetime.fromtimestamp(min(ts_values))
        hi = dt.datetime.fromtimestamp(max(ts_values))
        print(f"时间范围: {lo:%Y-%m-%d %H:%M:%S} ~ {hi:%Y-%m-%d %H:%M:%S}")
    print("示例查询: SELECT substr(datetime(ts,'unixepoch'),1,13) hour, count(*) FROM rows GROUP BY 1;")
    conn.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
