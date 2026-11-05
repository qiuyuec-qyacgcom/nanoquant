"""端到端冒烟测试: 登录 -> 行情 -> 启动引擎 -> 观察信号/持仓 -> 备份 -> 心法.

用法:
    python tests/smoke_test.py [base_url]
默认 http://127.0.0.1:8899
"""
from __future__ import annotations

import json
import sys
import time

import httpx

try:  # Windows 控制台默认非 UTF-8, 避免中文输出乱码
    sys.stdout.reconfigure(encoding="utf-8")
except (AttributeError, OSError):
    pass

BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8899").rstrip("/")
API = f"{BASE}/api/v1"


def show(title: str, data) -> None:
    print(f"\n=== {title} ===")
    if isinstance(data, (dict, list)):
        print(json.dumps(data, ensure_ascii=False, indent=2)[:1600])
    else:
        print(data)


def main() -> int:
    with httpx.Client(timeout=90) as c:
        r = c.get(f"{API}/health")
        assert r.status_code == 200, r.text
        show("health", r.json())

        r = c.post(f"{API}/auth/login", json={"username": "admin", "password": "admin123"})
        if r.status_code != 200:
            print("登录失败:", r.status_code, r.text[:300])
            return 1
        token = r.json()["token"]
        h = {"Authorization": f"Bearer {token}"}
        show("login", {"username": r.json()["username"], "must_change": r.json()["must_change_password"]})

        r = c.get(f"{API}/system/diagnostics", headers=h)
        show("diagnostics", r.json())

        r = c.get(f"{API}/engine/signals", headers=h)
        m = r.json()
        show("signals.summary", m.get("summary"))
        print("signals:", len(m.get("items", [])), "rows")

        r = c.get(f"{API}/settings/risk", headers=h)
        show("risk cfg", r.json()["data"])

        # 启动引擎
        r = c.post(f"{API}/engine/control", headers=h, json={"enabled": True, "mode": "demo"})
        print("\nengine start ->", r.status_code)
        rounds = int(sys.argv[2]) if len(sys.argv) > 2 else 10
        for i in range(rounds):
            time.sleep(3)
            st = c.get(f"{API}/engine/status", headers=h).json()
            print(
                f"  t+{(i + 1) * 3:>2}s equity={st['account'].get('equity'):.4f} "
                f"pos={len(st['positions'])} trades={st['stats']['trades']} "
                f"cycle={st['last_cycle_ms']}ms err={st['last_error'][:60]}"
            )
        show("positions", c.get(f"{API}/engine/positions", headers=h).json()["items"])
        sig = c.get(f"{API}/engine/signals?limit=8", headers=h).json()["items"]
        show("signals", [{k: s[k] for k in ("symbol", "direction", "score", "decision", "reject_reason")} for s in sig])
        trades = c.get(f"{API}/engine/trades?limit=5", headers=h).json()
        show("trades.stats", trades["stats"])

        # 手工触发一次复盘(如果有交易)
        if trades["items"]:
            tid = trades["items"][0]["id"]
            r = c.post(f"{API}/evolution/review/{tid}?force=true", headers=h)
            show("review", r.json() if r.status_code == 200 else r.text[:400])
            r = c.post(f"{API}/evolution/global-review", headers=h)
            show("global_review", r.json() if r.status_code == 200 else r.text[:400])

        show("mantras", c.get(f"{API}/evolution/mantras", headers=h).json()["stats"])
        show("injection", c.get(f"{API}/evolution/injection", headers=h).json()["text"][:600])

        # 备份
        r = c.post(f"{API}/backup/create", headers=h, json={"name": "smoke", "encrypt": False})
        show("backup", {k: r.json().get(k) for k in ("ok", "job_id", "size", "sha256")} if r.status_code == 200 else r.text[:400])
        jobs = c.get(f"{API}/backup/jobs", headers=h).json()["items"]
        print("backup jobs:", len(jobs), "| verified:", [j.get("sha256_ok") for j in jobs[:3]])

        # 停止引擎, 避免污染
        c.post(f"{API}/engine/control", headers=h, json={"enabled": False})
        print("\n引擎已停止")

        # 前端静态资源
        r = c.get(f"{BASE}/")
        print("SPA:", r.status_code, "html" if "<div id=\"app\">" in r.text else r.text[:120])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
