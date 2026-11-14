"""让 pytest 能直接 import nanoquant (不要求先 pip install -e).

同时把运行目录指向临时目录: 测试绝不能读写真实的 data/ 与 .master_key。
环境变量必须在任何 nanoquant 模块被 import 之前设置, conftest 是唯一的时机。
"""
from __future__ import annotations

import os
import pathlib
import shutil
import sys
import tempfile

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

_TMP = pathlib.Path(tempfile.mkdtemp(prefix="nanoquant-tests-"))
os.environ.setdefault("NANOQUANT_HOME", str(_TMP))
os.environ.setdefault("NANOQUANT_DATA_DIR", str(_TMP / "data"))
os.environ.setdefault("NANOQUANT_LOG_DIR", str(_TMP / "logs"))
os.environ.setdefault("NANOQUANT_DB", str(_TMP / "data" / "test.db"))
os.environ.setdefault("NANOQUANT_FRONTEND", str(_TMP / "dist"))
os.environ.setdefault("NANOQUANT_KEY_FILE", str(_TMP / "data" / ".master_key"))


def pytest_sessionfinish(session, exitstatus):  # noqa: ARG001
    shutil.rmtree(_TMP, ignore_errors=True)


@pytest.fixture(scope="session", autouse=True)
def _seed_admin_password():
    """测试环境预置一个"已改过"的密码.

    出厂状态下服务会拒绝除改密外的所有后台接口（见 main._force_password_change），
    而绝大多数用例需要直接调 API。所以这里把 ``password_is_default`` 置 False，
    相当于"这台机器用户已经改过密码"。该策略本身由 test_password_policy.py 单独验证。
    """
    from nanoquant.db import init_db
    from nanoquant.security import hash_password
    from nanoquant.services.settings import update_group

    init_db()
    update_group(
        "security",
        {"password_hash": hash_password("admin123"), "password_is_default": False},
    )
