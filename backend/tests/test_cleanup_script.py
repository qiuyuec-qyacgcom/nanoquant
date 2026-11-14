"""scripts/cleanup_history.py 的行为：默认只报告，--apply 才动数据.

这个脚本会直接改生产库，所以"演练不写入"和"压缩后可读"必须由测试钉住，
不能靠人记得加 --apply 之前先看一眼。
"""
from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "cleanup_history.py"


def _make_db(path: Path, rows: int = 5) -> None:
    con = sqlite3.connect(str(path))
    con.execute(
        "create table signals (id integer primary key, ts integer, decision text, features text)"
    )
    con.execute("create table pending_orders (id integer primary key, symbol text)")
    con.execute("create table trades (id integer primary key)")
    heavy = {
        "bars": 59, "close": 100.0, "ema_fast": 100.1, "ema_slow": 99.8, "rsi": 48.2,
        "atr": 0.9, "macd_hist": -0.002, "ret_1": 0.1, "ret_5": -0.2, "ret_15": 0.3,
        "vol_ratio": 0.9, "funding": 0.0001, "open_interest": 2.1e10,
        "change_24h": -1.2, "quote_volume": 6.1e8, "atr_pct": 0.9, "regime": "trend",
        "first_ts": 1794000000,
        # 真实快照约 1.3KB，这里补一段填充让它超过脚本的最小重写阈值
        "note": "x" * 600,
    }
    for i in range(rows):
        con.execute(
            "insert into signals (ts, decision, features) values (?, 'flat', ?)",
            (1794000000 + i, json.dumps(heavy)),
        )
    con.commit()
    con.close()


def _run(db: Path, *args: str) -> subprocess.CompletedProcess:
    env = {**os.environ, "NANOQUANT_DB": str(db)}
    return subprocess.run(
        [sys.executable, str(SCRIPT), *args],
        capture_output=True,
        text=True,
        env=env,
        timeout=120,
    )


def _tables(db: Path) -> set[str]:
    con = sqlite3.connect(str(db))
    try:
        return {r[0] for r in con.execute("select name from sqlite_master where type='table'")}
    finally:
        con.close()


def test_dry_run_changes_nothing(tmp_path):
    db = tmp_path / "nano.db"
    _make_db(db)
    before = db.read_bytes()

    out = _run(db)

    assert out.returncode == 0, out.stderr
    assert "待重写: 5 行" in out.stdout
    assert db.read_bytes() == before, "演练阶段绝不能写入"
    assert "pending_orders" in _tables(db), "演练阶段也不能删表"


def test_apply_compacts_features_and_drops_legacy_table(tmp_path):
    db = tmp_path / "nano.db"
    _make_db(db)

    out = _run(db, "--apply")

    assert out.returncode == 0, out.stderr
    con = sqlite3.connect(str(db))
    try:
        features = [json.loads(r[0]) for r in con.execute("select features from signals").fetchall()]
    finally:
        con.close()
    assert features, "压缩后信号还得在"
    for item in features:
        assert "ema_fast" not in item and "macd_hist" not in item
        assert "atr_pct" in item and "first_ts" in item

    tables = _tables(db)
    assert "pending_orders" not in tables, "废弃表应被删除"
    assert "signals" in tables and "trades" in tables


def test_apply_is_idempotent(tmp_path):
    db = tmp_path / "nano.db"
    _make_db(db)
    _run(db, "--apply")

    second = _run(db, "--apply")

    assert second.returncode == 0
    assert "已重写: 0 行" in second.stdout


@pytest.fixture()
def live_server():
    """假装有一个 NanoQuant 在 8898 上跑，用来验证 VACUUM 前的安全闸门."""

    class _H(BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'{"ok":true}')

        def log_message(self, *args):  # 静音
            return

    srv = HTTPServer(("127.0.0.1", 8898), _H)
    thread = threading.Thread(target=srv.serve_forever, daemon=True)
    thread.start()
    try:
        yield
    finally:
        srv.shutdown()


def test_refuses_vacuum_when_service_is_running(tmp_path, live_server):
    """服务在跑时换库会让活着的进程继续拿着旧文件句柄 —— 必须拒绝."""
    db = tmp_path / "nano.db"
    _make_db(db)

    out = _run(db, "--apply", "--vacuum", "--port", "8898")

    assert out.returncode == 3
    assert "仍在运行" in (out.stdout + out.stderr)
