"""资讯源体检 (离线脚本, 不依赖运行中的服务).

用法:
    python tests/news_probe.py
逐源打印: 是否可达 / 耗时 / 条数 / 失败原因 / 修复建议
"""
from __future__ import annotations

import asyncio
import sys

try:
    sys.stdout.reconfigure(encoding="utf-8")
except (AttributeError, OSError):
    pass

from nanoquant.db import init_db  # noqa: E402
from nanoquant.news import service  # noqa: E402


async def main() -> None:
    init_db()
    result = await service.diagnostics()
    print(f"正常 {result['ok_count']}/{result['total']} · 代理: {result['proxy']} · 超时 {result['timeout_sec']}s\n")
    for row in result["rows"]:
        flag = "OK  " if row["ok"] else ("SKIP" if not row["enabled"] else "FAIL")
        print(f"[{flag}] {row['label']:<20} {row['latency_ms']:>5}ms items={row['count']:<3} {row['error'][:80]}")
        if row["hint"]:
            print(f"        → {row['hint']}")
        if row["url"]:
            print(f"        {row['url']}")
    if result["suggestion"]:
        print("\n" + result["suggestion"])


if __name__ == "__main__":
    asyncio.run(main())
