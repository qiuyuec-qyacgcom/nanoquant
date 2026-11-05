"""AI 链路测试: 自动拉起 Mock 模型 -> 注册 -> 测试 -> 诊断 -> 决策演练 -> 议会 -> 分析.

用法: python tests/ai_test.py [base_url]
"""
from __future__ import annotations

import json
import subprocess
import sys
import time
from pathlib import Path

import httpx

try:
    sys.stdout.reconfigure(encoding="utf-8")
except (AttributeError, OSError):
    pass

BASE = (sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8899").rstrip("/")
API = f"{BASE}/api/v1"
MOCK_PORT = 8898


def show(title: str, data) -> None:
    print(f"\n=== {title} ===")
    print(json.dumps(data, ensure_ascii=False, indent=2)[:1800] if isinstance(data, (dict, list)) else data)


def start_mock() -> subprocess.Popen:
    root = Path(__file__).resolve().parents[1]
    proc = subprocess.Popen(
        [sys.executable, str(root / "tests" / "mock_llm.py"), str(MOCK_PORT)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    for _ in range(40):
        try:
            if httpx.get(f"http://127.0.0.1:{MOCK_PORT}/v1/models", timeout=1).status_code == 200:
                return proc
        except Exception:  # noqa: BLE001
            time.sleep(0.25)
    raise RuntimeError("Mock 模型未能启动")


def main() -> int:
    mock = start_mock()
    try:
        with httpx.Client(timeout=120) as c:
            token = c.post(f"{API}/auth/login", json={"username": "admin", "password": "admin123"}).json()["token"]
            h = {"Authorization": f"Bearer {token}"}

            # 清理同名 provider
            for p in c.get(f"{API}/ai/providers", headers=h).json()["items"]:
                if p["name"] == "本地Mock":
                    c.delete(f"{API}/ai/providers/{p['id']}", headers=h)

            r = c.post(f"{API}/ai/providers", headers=h, json={
                "name": "本地Mock",
                "protocol": "openai_chat",
                "base_url": f"http://127.0.0.1:{MOCK_PORT}/v1",
                "api_key": "sk-mock-key",
                "model": "mock-1",
                "priority": 1,
                "json_mode": True,
            })
            assert r.status_code == 200, r.text
            pid = r.json()["id"]
            print("provider id:", pid)

            show("test", c.post(f"{API}/ai/providers/{pid}/test", headers=h, json={"prompt": "ping"}).json())
            show("diagnose", c.get(f"{API}/ai/providers/{pid}/diagnose?deep=true", headers=h).json())
            show("models", c.get(f"{API}/ai/providers/{pid}/models", headers=h).json())

            c.put(f"{API}/settings/ai", headers=h, json={"patch": {"enabled": True, "mode": "single", "provider_id": pid, "min_confidence": 0.4}})

            show("decision-preview(single)", c.post(f"{API}/ai/decision-preview", headers=h, json={"symbol": "SOLUSDT", "interval": "1m", "include_news": True}).json())
            show("analyze", c.post(f"{API}/ai/analyze", headers=h, json={"symbol": "ETHUSDT", "interval": "1m", "include_news": True}).json())

            # 议会模式
            c.put(f"{API}/settings/ai", headers=h, json={"patch": {"mode": "committee"}})
            r = c.post(f"{API}/ai/decision-preview", headers=h, json={"symbol": "DOGEUSDT", "interval": "1m", "include_news": False}).json()
            show("decision-preview(committee)", {"action": r.get("action"), "confidence": r.get("confidence"), "agreement": r.get("agreement"), "votes": r.get("votes")})

            # 交易带 AI 置信度落库
            c.post(f"{API}/engine/control", headers=h, json={"enabled": True, "mode": "demo"})
            for _ in range(15):
                positions = c.get(f"{API}/engine/positions", headers=h).json()["items"]
                if positions:
                    break
                time.sleep(4)
            positions = c.get(f"{API}/engine/positions", headers=h).json()["items"]
            show("ai-backed positions", [{"symbol": p["symbol"], "side": p["side"], "ai": p["ai_confidence"], "reason": (p.get("ai_reason") or "")[:80]} for p in positions])
            for p in positions:
                c.post(f"{API}/engine/positions/{p['id']}/close", headers=h, json={"reason": "manual"})
            c.post(f"{API}/engine/control", headers=h, json={"enabled": False})

            show("usage", c.get(f"{API}/ai/usage", headers=h).json()["total_30d"])

            # 还原: 保留 provider 但取消主模型绑定
            c.put(f"{API}/settings/ai", headers=h, json={"patch": {"provider_id": 0, "mode": "single"}})
            c.delete(f"{API}/ai/providers/{pid}", headers=h)
            print("\n已清理 Mock 模型配置")
        return 0
    finally:
        mock.terminate()


if __name__ == "__main__":
    raise SystemExit(main())
