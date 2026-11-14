#!/usr/bin/env python
"""重置管理员密码（忘记密码 / 被出厂密码策略锁在外面时用）.

行为：
1. 生成一个一次性随机密码（打印在终端，不写日志文件）；
2. 把 ``security.password_is_default`` 置 True —— 用它登录后仍会被要求改密，
   避免"重置完又是长期弱口令"；
3. 同时清空登录失败计数，否则刚重置还会被 429 挡着。

用法（服务可以不停，改完立即生效）：
    python scripts/reset_password.py            # 随机密码
    python scripts/reset_password.py --password 'My-Pass-2026'

注意：需要在与数据目录同一台机器、同一个 NANOQUANT_HOME 下执行。
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "backend"))

from nanoquant.db import init_db  # noqa: E402
from nanoquant.security import generate_password, hash_password  # noqa: E402
from nanoquant.services import state  # noqa: E402
from nanoquant.services.settings import get_security, update_group  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser(description="重置 NanoQuant 管理员密码")
    parser.add_argument("--password", help="指定新密码（至少 8 位）；不填则随机生成")
    args = parser.parse_args()

    password = (args.password or "").strip()
    if password and len(password) < 8:
        print("新密码至少 8 位", file=sys.stderr)
        return 2
    if not password:
        password = generate_password()

    init_db()
    cfg = get_security()
    update_group(
        "security",
        {"password_hash": hash_password(password), "password_is_default": True},
    )
    state.set("login_fails", "0")

    print("=" * 52)
    print(f"用户名: {cfg.username}")
    print(f"新密码: {password}")
    print("=" * 52)
    print("已标记为出厂密码状态：登录后必须先改密，才能使用后台其他功能。")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
