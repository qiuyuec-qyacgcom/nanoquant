"""路由层集成测试（进程内，不联网、不碰真实数据）.

覆盖: 健康检查 / 鉴权（含越权）/ 配置读写与校验 / 只读观测接口 / 回测入参校验。
不启动 lifespan，因此引擎、行情流、数据库写线程的常驻任务都不会运行
（写线程在 import 时就存在，但只写临时库，不会污染真实 data/）。
"""
from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

API = "/api/v1"


@pytest.fixture(scope="module")
def client():
    from nanoquant.main import bootstrap, create_app

    bootstrap()  # 建表 + 种子 admin
    # 不用 with: TestClient 的上下文会跑 lifespan, 从而启动引擎与行情流,
    # 测试只需要路由与 DB, 不跑常驻任务
    c = TestClient(create_app())
    try:
        yield c
    finally:
        c.close()


def _token(client: TestClient) -> dict[str, str]:
    res = client.post(f"{API}/auth/login", json={"username": "admin", "password": "admin123"})
    assert res.status_code == 200, res.text
    return {"Authorization": f"Bearer {res.json()['token']}"}


def test_health_is_public(client):
    res = client.get(f"{API}/health")
    assert res.status_code == 200
    body = res.json()
    assert body["ok"] is True and body["app"] == "NanoQuant"


def test_protected_route_requires_token(client):
    res = client.get(f"{API}/system/exec-sidecar")
    assert res.status_code == 401
    # 伪造签名必须被拒
    res = client.get(f"{API}/system/exec-sidecar", headers={"Authorization": "Bearer abc.def"})
    assert res.status_code == 401


def test_login_rejects_wrong_password_then_returns_profile(client):
    bad = client.post(f"{API}/auth/login", json={"username": "admin", "password": "wrong-password"})
    assert bad.status_code == 401

    headers = _token(client)
    me = client.get(f"{API}/auth/me", headers=headers)
    assert me.status_code == 200
    assert me.json()["username"] == "admin"


def test_settings_read_and_validate(client):
    headers = _token(client)
    res = client.get(f"{API}/settings/system", headers=headers)
    assert res.status_code == 200
    data = res.json()["data"]
    assert data["exec_mode"] in {"off", "shadow", "enforce"}

    # 越界值必须 400，且不能落库
    bad = client.put(f"{API}/settings/system", headers=headers, json={"patch": {"exec_sample_pct": 0}})
    assert bad.status_code == 400
    # 合法值可写，随后恢复
    ok = client.put(f"{API}/settings/system", headers=headers, json={"patch": {"exec_sample_pct": 42}})
    assert ok.status_code == 200 and ok.json()["data"]["exec_sample_pct"] == 42
    restore = client.put(f"{API}/settings/system", headers=headers, json={"patch": {"exec_sample_pct": 100}})
    assert restore.json()["data"]["exec_sample_pct"] == 100

    unknown = client.get(f"{API}/settings/not-a-group", headers=headers)
    assert unknown.status_code == 404


def test_engine_mode_cannot_be_switched_via_settings(client):
    headers = _token(client)
    res = client.put(f"{API}/settings/engine", headers=headers, json={"patch": {"mode": "live"}})
    assert res.status_code == 400
    assert "交易环境" in res.json()["detail"]


def test_observability_endpoints_shape(client):
    headers = _token(client)
    sidecar = client.get(f"{API}/system/exec-sidecar", headers=headers).json()
    assert sidecar["mode"] in {"off", "shadow", "enforce"}
    assert set(sidecar["counts"]) >= {"matched", "mismatched", "errors", "skipped"}
    assert "verdict" in sidecar

    dead = client.get(f"{API}/system/dead-letters", headers=headers).json()
    assert "total" in dead and "items" in dead

    health = client.get(f"{API}/system/write-health", headers=headers).json()
    assert "queue" in health and "dead_letters" in health and health["verdict"]


def test_backtest_rejects_symbol_outside_pool(client):
    headers = _token(client)
    res = client.post(
        f"{API}/backtest/run",
        headers=headers,
        json={"symbol": "NOT_IN_POOL", "interval": "5m", "limit": 200},
    )
    assert res.status_code == 404


def test_backtest_run_and_walkforward_with_stubbed_klines(client, monkeypatch):
    """用桩 K 线跑通回测与样本外接口：不依赖交易所，也能守住字段契约。"""
    from types import SimpleNamespace

    from nanoquant.db import session_scope
    from nanoquant.models import TradingSymbol

    with session_scope() as s:
        if s.get(TradingSymbol, "TESTUSDT") is None:
            s.add(TradingSymbol(symbol="TESTUSDT", enabled=True, sort_order=1))

    import nanoquant.api.routes.backtest as route

    price = 100.0
    candles = []
    for i in range(600):
        price *= 1.0007 if i % 4 else 0.9995
        candles.append(
            SimpleNamespace(
                open=price,
                high=price * 1.002,
                low=price * 0.998,
                close=price,
                volume=1000.0,
                open_time=i * 300_000,
            )
        )

    async def fake_klines(symbol: str, interval: str, limit: int, fresh: bool = False):  # noqa: ARG001
        return candles[:limit]

    monkeypatch.setattr(route.binance_client, "klines", fake_klines)
    headers = _token(client)

    body = {"symbol": "TESTUSDT", "interval": "5m", "limit": 600, "spread_bps": 2, "funding_bps_8h": 1}
    res = client.post(f"{API}/backtest/run", headers=headers, json=body)
    assert res.status_code == 200, res.text
    data = res.json()
    assert data["bars"] == 600
    assert {"trade_count", "funding_total", "partial_tp_count", "assumptions"} <= set(data)
    assert data["assumptions"]["spread_bps"] == 2

    wf = client.post(
        f"{API}/backtest/walkforward",
        headers=headers,
        json={**body, "entry_threshold": [0.2, 0.3], "folds": 2, "train_bars": 200},
    )
    assert wf.status_code == 200, wf.text
    report = wf.json()
    assert report["folds"] == 2
    assert report["aggregate"]["oos_trades"] >= 0
    assert {"overfit_gap_pct", "consistency", "positive_folds"} <= set(report["aggregate"])


def test_backtest_rejects_bad_cost_params(client):
    headers = _token(client)
    # 参数校验先于"是否在标的池"（Pydantic 422）
    res = client.post(
        f"{API}/backtest/run",
        headers=headers,
        json={"symbol": "BTCUSDT", "interval": "5m", "limit": 200, "funding_bps_8h": 999},
    )
    assert res.status_code == 422


def test_evolution_effectiveness_shape(client):
    headers = _token(client)
    res = client.get(f"{API}/evolution/effectiveness?limit=50", headers=headers)
    assert res.status_code == 200
    body = res.json()
    assert {"injected", "control", "ai_off", "legacy", "delta_win_rate_pct", "sample_ok", "verdict"} <= set(body)
    assert body["window"]["limit"] == 50
    assert body["injected"]["trades"] == 0  # 临时库无成交


def test_metrics_is_public_text_without_secrets(client):
    res = client.get(f"{API}/metrics")
    assert res.status_code == 200
    assert "text/plain" in res.headers["content-type"]
    body = res.text
    assert "nanoquant_up 1" in body
    assert "nanoquant_write_queue_pending" in body
    assert "nanoquant_exec_shadow_mismatched" in body
    for secret in ("api_key", "secret", "equity", "balance"):
        assert secret not in body.lower()


def test_metrics_can_be_disabled(client):
    headers = _token(client)
    client.put(f"{API}/settings/system", headers=headers, json={"patch": {"metrics_enabled": False}})
    try:
        assert client.get(f"{API}/metrics").status_code == 404
    finally:
        client.put(f"{API}/settings/system", headers=headers, json={"patch": {"metrics_enabled": True}})
    assert client.get(f"{API}/metrics").status_code == 200


def test_api_rate_limit_returns_429_with_retry_after(client):
    from nanoquant.services.api_guard import limiter

    headers = _token(client)
    limiter.clear()
    client.put(f"{API}/settings/system", headers=headers, json={"patch": {"api_rate_limit_per_min": 2}})
    try:
        assert client.get(f"{API}/auth/me", headers=headers).status_code == 200
        assert client.get(f"{API}/auth/me", headers=headers).status_code == 200
        blocked = client.get(f"{API}/auth/me", headers=headers)
        assert blocked.status_code == 429
        assert int(blocked.headers["Retry-After"]) > 0
        # 健康检查与 metrics 是探针，不该被限频误伤
        assert client.get(f"{API}/health").status_code == 200
        assert client.get(f"{API}/metrics").status_code == 200
    finally:
        client.put(f"{API}/settings/system", headers=headers, json={"patch": {"api_rate_limit_per_min": 0}})
        limiter.clear()
    assert client.get(f"{API}/auth/me", headers=headers).status_code == 200
