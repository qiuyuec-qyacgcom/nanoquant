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


def test_cycle_interval_below_floor_is_rejected(client):
    headers = _token(client)
    bad = client.put(f"{API}/settings/engine", headers=headers, json={"patch": {"cycle_interval_sec": 5}})
    assert bad.status_code == 400
    ok = client.put(f"{API}/settings/engine", headers=headers, json={"patch": {"cycle_interval_sec": 30}})
    assert ok.status_code == 200 and ok.json()["data"]["cycle_interval_sec"] == 30
    client.put(f"{API}/settings/engine", headers=headers, json={"patch": {"cycle_interval_sec": 15}})


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
    from nanoquant.services.settings import update_group

    update_group("strategy", {"mode": "trend"})
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


def test_signals_summary_exposes_scans_and_repeats(client):
    headers = _token(client)
    res = client.get(f"{API}/engine/signals?limit=5", headers=headers)
    assert res.status_code == 200
    body = res.json()
    assert "scans" in body["summary"]
    assert all("repeats" in row for row in body["items"])


def test_evolution_effectiveness_shape(client):
    headers = _token(client)
    res = client.get(f"{API}/evolution/effectiveness?limit=50", headers=headers)
    assert res.status_code == 200
    body = res.json()
    assert {"injected", "control", "ai_off", "legacy", "delta_win_rate_pct", "sample_ok", "verdict"} <= set(body)
    assert body["window"]["limit"] == 50
    assert body["injected"]["trades"] == 0  # 临时库无成交


def test_evolution_batch_review_and_health_endpoints(client):
    """批量复盘样本不足时 NO_CHANGE，健康分重算与注入披露接口可用."""
    headers = _token(client)
    batch = client.post(f"{API}/evolution/review-batch", headers=headers)
    assert batch.status_code == 200, batch.text
    body = batch.json()
    assert body["ok"] and body["change_status"] == "NO_CHANGE"
    assert body["applied"] == []  # 样本不足/AI 失败都不允许造心法

    health = client.post(f"{API}/evolution/health-refresh", headers=headers)
    assert health.status_code == 200
    assert {"status", "rescored", "archived", "changes"} <= set(health.json())

    inj = client.get(f"{API}/evolution/injection", headers=headers).json()
    assert {"active", "injected", "limit", "not_injected", "unscored"} <= set(inj["report"])
    assert inj["stats"]["min_lesson_sample"] >= 2


def test_trade_mode_endpoints(client, monkeypatch):
    """交易模式体检与一键切换接口（双向持仓 + 逐仓）."""
    from nanoquant.api.routes import trade as trade_routes

    class FakeBroker:
        async def trade_mode_status(self, symbols=None):  # noqa: ARG002
            return {
                "hedge_mode": False, "need_hedge": True, "need_isolated": ["BTCUSDT"],
                "with_positions": [], "can_switch": True,
                "margin": {"BTCUSDT": "cross"}, "hint": "账户为单向持仓",
            }

        async def switch_trade_mode(self, *, hedge=True, isolated=True):  # noqa: ARG002
            return {"ok": True, "results": [{"target": "hedge", "ok": True, "detail": "已切换为双向持仓"}]}

    monkeypatch.setattr(trade_routes, "get_broker", lambda mode=None: FakeBroker())  # noqa: ARG005
    headers = _token(client)
    got = client.get(f"{API}/engine/trade-mode", headers=headers)
    assert got.status_code == 200
    assert got.json()["need_hedge"] is True
    assert got.json()["need_isolated"] == ["BTCUSDT"]

    posted = client.post(
        f"{API}/engine/trade-mode", headers=headers, json={"hedge": True, "isolated": False}
    )
    assert posted.status_code == 200
    assert posted.json()["ok"] is True


def test_engine_status_cache_avoids_duplicate_exchange_reads(client, monkeypatch):
    """多标签页 2s 内轮询 status 只向交易所取一次，控制操作后立即失效."""
    from nanoquant.api.routes import trade as trade_routes

    calls = {"n": 0}

    async def fake_status():
        calls["n"] += 1
        return {"ok": True, "stamp": calls["n"]}

    monkeypatch.setattr(trade_routes.engine, "status", fake_status)
    trade_routes.invalidate_status_cache()
    headers = _token(client)
    try:
        a = client.get(f"{API}/engine/status", headers=headers).json()
        b = client.get(f"{API}/engine/status", headers=headers).json()
        assert a["stamp"] == b["stamp"] == 1
        assert calls["n"] == 1

        trade_routes.invalidate_status_cache()
        c = client.get(f"{API}/engine/status", headers=headers).json()
        assert c["stamp"] == 2 and calls["n"] == 2
    finally:
        trade_routes.invalidate_status_cache()


def test_strategy_crud_and_preview(client):
    """策略 CRUD + 停用后回落到默认分析 + 适用范围预览只做本地匹配."""
    from nanoquant.db import init_db, session_scope
    from nanoquant.models import StrategyProfile

    init_db()
    headers = _token(client)
    with session_scope() as s:
        s.query(StrategyProfile).delete()

    created = client.post(
        f"{API}/strategies",
        headers=headers,
        json={
            "name": "单元策略", "priority": 10,
            "rule": "当 15m 回踩 EMA21 不破且量比大于 1.2 时做多。",
            "filters": {"side": "long", "symbols": ["BTCUSDT"]},
        },
    )
    assert created.status_code == 200, created.text
    item = created.json()["item"]
    assert item["filters"]["side"] == "long"
    assert "回踩 EMA21" in item["rule"]
    assert "BTCUSDT" in item["summary"] and "只做多" in item["summary"]

    # 策略内容太短会被拒
    short = client.post(
        f"{API}/strategies", headers=headers, json={"name": "太短", "rule": "做多"},
    )
    assert short.status_code == 400

    dup = client.post(
        f"{API}/strategies", headers=headers,
        json={"name": "单元策略", "rule": "当 15m 回踩 EMA21 不破时做多。"},
    )
    assert dup.status_code == 400

    patched = client.patch(
        f"{API}/strategies/{item['id']}", headers=headers,
        json={"enabled": False, "priority": 30},
    )
    assert patched.status_code == 200
    assert patched.json()["item"]["enabled"] is False

    listed = client.get(f"{API}/strategies", headers=headers).json()
    assert listed["stats"]["total"] == 1
    assert listed["stats"]["default"] is True  # 唯一策略已停用 => 默认分析

    preview = client.get(f"{API}/strategies/preview", headers=headers)
    assert preview.status_code == 200
    body = preview.json()
    assert body["ok"] is True and body["default"] is True

    assert client.delete(f"{API}/strategies/{item['id']}", headers=headers).status_code == 200
    assert client.delete(f"{API}/strategies/{item['id']}", headers=headers).status_code == 404


def test_engine_protection_degrades_on_rate_limit(client, monkeypatch):
    """限频时保护单接口降级为 200 空列表，其它交易所错误仍应是 503 而不是 NameError 500."""
    from nanoquant.api.routes import trade as trade_routes
    from nanoquant.exchange.client import BinanceError

    headers = _token(client)

    async def limited(*args, **kwargs):  # noqa: ARG001
        raise BinanceError(-1003, "banned until 1794300000000")

    monkeypatch.setattr(trade_routes.binance_client, "open_algo_orders", limited)
    res = client.get(f"{API}/engine/protection", headers=headers)
    assert res.status_code == 200, res.text
    body = res.json()
    assert body["items"] == [] and body["degraded"] is True
    assert "限频" in body["reason"]

    async def broken(*args, **kwargs):  # noqa: ARG001
        raise BinanceError(-1021, "timestamp outside recvWindow")

    monkeypatch.setattr(trade_routes.binance_client, "open_algo_orders", broken)
    assert client.get(f"{API}/engine/protection", headers=headers).status_code == 503


def test_provider_reasoning_effort_roundtrip(client):
    headers = _token(client)
    body = {
        "name": "effort-test",
        "base_url": "http://127.0.0.1:9/v1",
        "api_key": "sk-test",
        "model": "deepseek-v4.1",
        "reasoning_effort": "high",
    }
    created = client.post(f"{API}/ai/providers", headers=headers, json=body)
    assert created.status_code == 200, created.text
    pid = created.json()["id"]
    try:
        items = client.get(f"{API}/ai/providers", headers=headers).json()["items"]
        row = next(p for p in items if p["id"] == pid)
        assert row["reasoning_effort"] == "high"
        assert row["max_tokens"] == 3000  # schema 默认值与模型默认值一致

        bad = client.post(
            f"{API}/ai/providers",
            headers=headers,
            json={**body, "name": "effort-bad", "reasoning_effort": "ultra"},
        )
        assert bad.status_code == 422
    finally:
        client.delete(f"{API}/ai/providers/{pid}", headers=headers)


def test_reasoning_models_endpoint(client):
    headers = _token(client)
    res = client.get(f"{API}/ai/reasoning-models", headers=headers)
    assert res.status_code == 200
    body = res.json()
    assert body["levels"][0] == "auto"
    assert any(f["key"] == "qwen3" for f in body["families"])

    matched = client.get(f"{API}/ai/reasoning-models?model=qwen3-max", headers=headers).json()
    assert matched["matched"]["key"] == "qwen3"
    assert matched["matched"]["extreme"] is False
    assert any(o["value"] == "xhigh" and not o["enabled"] for o in matched["options"])


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
        # 先清窗口: 否则"恢复配置为 0"这次 PUT 自己会被刚设的 2 次上限挡成 429
        limiter.clear()
        client.put(f"{API}/settings/system", headers=headers, json={"patch": {"api_rate_limit_per_min": 0}})
        limiter.clear()
    assert client.get(f"{API}/auth/me", headers=headers).status_code == 200


def test_signal_follow_routes_and_idempotent_claim(client):
    """路由必须真的挂载（上一版只 import 未 include），且同一信号只能占位一次."""
    from nanoquant.db import init_db, session_scope
    from nanoquant.engine.worker import TradingEngine
    from nanoquant.models import SignalFollow
    from nanoquant.services import signal_follow as sf

    init_db()
    assert client.get(f"{API}/signal-follow/status").status_code == 401
    headers = _token(client)
    status = client.get(f"{API}/signal-follow/status", headers=headers)
    assert status.status_code == 200
    body = status.json()
    assert {"settings", "counts", "path", "offset", "inflight_margin"} <= set(body)
    assert client.get(f"{API}/signal-follow/events", headers=headers).status_code == 200
    assert client.post(f"{API}/signal-follow/read-once", headers=headers).json()["processed"] == 0

    parsed = sf.parse_signal({"id": "unit-claim", "symbol": "BTCUSDT", "side": "long", "event": "开仓"})
    key = sf.signal_identity(parsed)
    eng = TradingEngine()
    with session_scope() as s:
        s.query(SignalFollow).filter(SignalFollow.signal_id == key).delete()
    try:
        assert eng._claim_follow(key, parsed, "demo") is True
        assert eng._claim_follow(key, parsed, "demo") is False  # HTTP 重推 / 文件重读
        eng._finish_follow(key, parsed, "skipped", "测试", "demo")
        with session_scope() as s:
            rows = s.query(SignalFollow).filter(SignalFollow.signal_id == key).all()
            assert len(rows) == 1 and rows[0].status == "skipped"
    finally:
        with session_scope() as s:
            s.query(SignalFollow).filter(SignalFollow.signal_id == key).delete()


def test_signal_follow_ingest_auth_and_event_filter(client):
    """直推鉴权 + 平仓事件不跟单 + 状态接口对密钥脱敏."""
    from nanoquant.db import init_db, session_scope
    from nanoquant.models import SignalFollow
    from nanoquant.services.settings import update_group

    init_db()
    headers = _token(client)
    off = client.post(f"{API}/signal-follow/ingest", headers=headers, json={"symbol": "X"})
    assert off.status_code == 200 and off.json()["status"] == "skipped"
    assert client.post(f"{API}/signal-follow/ingest", json={"symbol": "X"}).status_code == 401

    update_group("signal_follow", {"enabled": True, "ingest_token": "unit-secret"})
    try:
        assert client.post(f"{API}/signal-follow/ingest", json={"symbol": "X"}).status_code == 401
        res = client.post(
            f"{API}/signal-follow/ingest",
            headers={"X-Signal-Token": "unit-secret"},
            json={"id": "unit-exit", "event": "平仓", "symbol": "BTCUSDT", "side": "long"},
        )
        assert res.status_code == 200 and res.json()["status"] == "skipped"
        # 平仓不再被"非开仓事件"过滤，而是正常进入执行分支；
        # 这里引擎没开，所以在引擎闸门处被挡下
        assert "引擎" in res.json()["reason"]
        masked = client.get(f"{API}/signal-follow/status", headers=headers).json()
        assert masked["settings"]["ingest_token"] == "******" and masked["ingest_token_set"]
    finally:
        update_group("signal_follow", {"enabled": False, "ingest_token": ""})
        with session_scope() as s:
            s.query(SignalFollow).filter(SignalFollow.signal_id.like("unit-exit%")).delete()


def test_follow_signal_end_to_end_uses_system_sizing(monkeypatch):
    """桩交易所跑通全链路：+10x / 自动补池 / 系统资金 / 幂等 / 平仓事件不跟."""
    import asyncio
    import time

    import nanoquant.engine.worker as worker
    from nanoquant.db import init_db, session_scope
    from nanoquant.models import SignalFollow, TradingSymbol
    from nanoquant.services import state
    from nanoquant.services import symbols as symbol_service
    from nanoquant.services.settings import get_group, update_group

    init_db()
    symbol = "FOLLOWUSDT"
    with session_scope() as s:
        s.query(SignalFollow).filter(SignalFollow.symbol == symbol).delete()
        s.query(TradingSymbol).filter(TradingSymbol.symbol == symbol).delete()

    async def fake_add_symbol(raw):
        with session_scope() as s:
            if s.get(TradingSymbol, raw) is None:
                s.add(TradingSymbol(
                    symbol=raw, enabled=True, max_leverage=100, precision=3,
                    tick_sz="0.01", min_sz="0.001", min_notional=5.0, sort_order=999,
                ))
        return {"symbol": raw}

    async def fake_tickers(symbols):  # noqa: ARG001
        from types import SimpleNamespace

        return {symbol: SimpleNamespace(last=100.0, change_pct=1.0, quote_volume=1e9)}

    calls: list[dict] = []

    class FakeBroker:
        mode = "demo"

        async def account(self):
            from types import SimpleNamespace

            return SimpleNamespace(
                equity=5000.0, available=3000.0, wallet=5000.0,
                unrealized=0.0, margin_used=0.0, as_dict=lambda: {},
            )

        async def open_position(self, **kwargs):
            from types import SimpleNamespace

            calls.append(kwargs)
            return SimpleNamespace(ok=True, price=100.0, qty=kwargs["qty"], message="ok")

    monkeypatch.setattr(symbol_service, "add_symbol", fake_add_symbol)
    monkeypatch.setattr(worker.binance_client, "tickers", fake_tickers)
    monkeypatch.setattr(worker, "get_broker", lambda mode: FakeBroker())  # noqa: ARG005
    for key in ("halted", "consecutive_losses", "loss_halt_until", "sl_guard_until"):
        state.set(key, "" if key == "halted" else "0")

    old_engine_cfg = get_group("engine", use_cache=False)
    old_follow_cfg = get_group("signal_follow", use_cache=False)
    eng = worker.engine
    old_lock = eng._follow_lock
    eng._follow_lock = asyncio.Lock()
    update_group("engine", {"enabled": True})
    update_group("signal_follow", {"enabled": True, "dry_run": False, "leverage_bonus": 10})

    row = {
        "id": "e2e-open", "symbol": symbol, "side": "long", "event": "开仓",
        "leverage": "20x", "ts": int(time.time()),
    }

    async def run():
        first = await eng.follow_signal_now(row)
        again = await eng.follow_signal_now(row)
        closed = await eng.follow_signal_now({**row, "id": "e2e-close", "event": "平仓"})
        return first, again, closed

    try:
        first, again, closed = asyncio.run(run())
        assert first["status"] == "opened" and first["leverage"] == 30  # 20x + 10x
        assert again["status"] == "duplicate"  # 文件重读 / HTTP 重推只跟一次
        assert closed["status"] == "skipped" and "持仓" in closed["reason"]
        # 平仓现在会真的进入执行分支，只是这笔仓位不在本地账本里才跳过；
        # 空仓时绝不能凭空下单
        assert len(calls) == 1
        kw = calls[0]
        assert kw["leverage"] == 30 and kw["qty"] > 0
        assert kw["features"]["source"] == "signal_follow"
        assert kw["features"]["plan"]["available"] > 0  # 资金按本系统账本算
        with session_scope() as s:
            assert s.get(TradingSymbol, symbol) is not None  # 自动补池
            rows = s.query(SignalFollow).filter(SignalFollow.symbol == symbol).all()
            assert sorted(r.status for r in rows) == ["opened", "skipped"]
    finally:
        eng._follow_lock = old_lock
        update_group("engine", old_engine_cfg)
        update_group("signal_follow", old_follow_cfg)
        with session_scope() as s:
            s.query(SignalFollow).filter(SignalFollow.symbol == symbol).delete()
            s.query(TradingSymbol).filter(TradingSymbol.symbol == symbol).delete()


def test_engine_reset_anchors_aligns_stale_baseline(client, monkeypatch):
    """资金重置后日亏基准没跟着重置 → 假日内亏损 -99% 熔断.

    这个坑真实发生过两次：账户从 2850 改成 10，基准还留在 2850，
    系统算出 -99.65% 直接停机。接口把基准对齐到当前权益并清掉这条熔断。
    """
    from types import SimpleNamespace

    from nanoquant.api.routes import trade as route
    from nanoquant.services import state

    class _FakeBroker:
        async def account(self):
            return SimpleNamespace(equity=10.0)

    monkeypatch.setattr(route, "get_broker", lambda mode: _FakeBroker())
    state.set_many(
        {
            "day_start_equity": "2850.23",
            "equity_peak": "2905.38",
            "halted": "日内亏损 -99.65% 超过上限 30.0%",
        }
    )

    res = client.post(f"{API}/engine/reset-anchors", headers=_token(client))
    assert res.status_code == 200, res.text
    data = res.json()
    assert data["ok"] is True
    assert abs(data["equity"] - 10.0) < 1e-9
    assert "日内亏损" in data["cleared_halt"]

    assert float(state.get("day_start_equity")) == pytest.approx(10.0)
    assert float(state.get("equity_peak")) == pytest.approx(10.0)
    assert state.get("halted") == ""


def test_engine_reset_anchors_clears_drawdown_halt(client, monkeypatch):
    """重置峰值必须同时解除回撤熔断，否则"重置了但引擎还是不动".

    判定依据（equity_peak）已经被改成当前权益，回撤归零；留着那条 halted 字符串，
    ``global_gates`` 会在第一步就返回"引擎已熔断"，形成一个没有任何出口的死锁
    ——调用方只能去改数据库。
    """
    from types import SimpleNamespace

    from nanoquant.api.routes import trade as route
    from nanoquant.services import state

    class _FakeBroker:
        async def account(self):
            return SimpleNamespace(equity=100.0)

    monkeypatch.setattr(route, "get_broker", lambda mode: _FakeBroker())
    state.set_many({"halted": "最大回撤 55.0% 超过上限 50.0%"})

    res = client.post(f"{API}/engine/reset-anchors", headers=_token(client))
    assert res.status_code == 200, res.text
    assert "回撤" in res.json()["cleared_halt"]
    assert state.get("halted") == ""


def test_engine_reset_anchors_keeps_unrelated_halt(client, monkeypatch):
    """与日亏/回撤无关的熔断（人工停机原因等）不该被顺手解除."""
    from types import SimpleNamespace

    from nanoquant.api.routes import trade as route
    from nanoquant.services import state

    class _FakeBroker:
        async def account(self):
            return SimpleNamespace(equity=100.0)

    monkeypatch.setattr(route, "get_broker", lambda mode: _FakeBroker())
    state.set_many({"halted": "运维手动停机：核对账单"})

    res = client.post(f"{API}/engine/reset-anchors", headers=_token(client))
    assert res.status_code == 200, res.text
    assert res.json()["cleared_halt"] == ""
    assert "运维手动停机" in state.get("halted")
    state.set("halted", "")


def test_metrics_exposes_ops_gauges_publicly_without_account_data(client):
    """运行态指标公开可抓；账户规模绝不能出现在免鉴权端点里。"""
    from nanoquant.services.settings import get_system, update_group

    old = get_system().metrics_enabled
    update_group("system", {"metrics_enabled": True})
    try:
        res = client.get(f"{API}/metrics")
        assert res.status_code == 200, res.text
        body = res.text
        for name in (
            "nanoquant_signals_last_1h",
            "nanoquant_halted",
            "nanoquant_last_cycle_age_seconds",
            "nanoquant_api_weight_used",
            "nanoquant_business_metrics_ok",
        ):
            assert f"{name} " in body, f"缺少指标 {name}"
        assert "nanoquant_account_equity_usdt" not in body, "公开端点不能暴露账户权益"
        assert "nanoquant_account_open_positions" not in body
    finally:
        update_group("system", {"metrics_enabled": old})


def test_metrics_account_gauges_require_token(client):
    """带有效 token 时才追加账户类指标，供 Prometheus 配 bearer_token 抓取。"""
    from nanoquant.services.settings import get_system, update_group

    old = get_system().metrics_enabled
    update_group("system", {"metrics_enabled": True})
    try:
        res = client.get(f"{API}/metrics", headers=_token(client))
        assert res.status_code == 200, res.text
        body = res.text
        assert "nanoquant_account_equity_usdt" in body
        assert "nanoquant_account_today_pnl_pct" in body
        assert "nanoquant_account_open_positions" in body
    finally:
        update_group("system", {"metrics_enabled": old})


def test_settings_changes_are_audited_with_before_after(client):
    """改参数要能回答"谁改的、改前是多少"——engine_events 里没有 diff。"""
    from nanoquant.services.settings import get_risk, update_group

    headers = _token(client)
    original = get_risk().max_drawdown_pct
    try:
        res = client.put(
            f"{API}/settings/risk",
            headers=headers,
            json={"patch": {"max_drawdown_pct": 21.5}},
        )
        assert res.status_code == 200, res.text
        assert "max_drawdown_pct" in res.json()["changed"]

        audit = client.get(f"{API}/settings/audit", headers=headers, params={"group": "risk", "limit": 5})
        assert audit.status_code == 200, audit.text
        item = audit.json()["items"][0]
        assert item["group"] == "risk"
        assert item["user"]
        change = item["changes"]["max_drawdown_pct"]
        assert change["to"] == 21.5
        assert change["from"] == original
    finally:
        update_group("risk", {"max_drawdown_pct": original})


def test_settings_audit_masks_secrets(client):
    """审计日志本身不能成为泄密源：密钥类字段只记"改过"，不记内容。"""
    from nanoquant.services.settings import get_data, update_group

    headers = _token(client)
    original = get_data().demo_api_key_enc
    try:
        res = client.put(
            f"{API}/settings/data",
            headers=headers,
            json={"patch": {"demo_api_key_enc": "ciphertext-should-not-appear"}},
        )
        assert res.status_code == 200, res.text

        audit = client.get(f"{API}/settings/audit", headers=headers, params={"group": "data", "limit": 5})
        body = audit.text
        assert "ciphertext-should-not-appear" not in body
        item = next(i for i in audit.json()["items"] if "demo_api_key_enc" in i["changes"])
        assert item["changes"]["demo_api_key_enc"]["to"] == "***"
    finally:
        update_group("data", {"demo_api_key_enc": original})


def test_settings_audit_skips_noop_writes(client):
    """没有实际变化时不写审计记录，避免刷屏。"""
    from nanoquant.services.settings import get_risk

    headers = _token(client)
    before = client.get(f"{API}/settings/audit", headers=headers, params={"group": "risk", "limit": 50})
    count_before = len(before.json()["items"])

    res = client.put(
        f"{API}/settings/risk",
        headers=headers,
        json={"patch": {"max_drawdown_pct": get_risk().max_drawdown_pct}},
    )
    assert res.status_code == 200, res.text
    assert res.json()["changed"] == []

    after = client.get(f"{API}/settings/audit", headers=headers, params={"group": "risk", "limit": 50})
    assert len(after.json()["items"]) == count_before


def test_engine_flatten_requires_confirm_phrase(client, monkeypatch):
    """紧急全平必须带确认短语，避免前端误触把仓位清光。"""
    from nanoquant.api.routes import trade as route

    called: dict = {}

    async def _fake_flatten(*, reason: str = "", stop_engine: bool = True):
        called["hit"] = True
        return {"ok": True, "engine_stopped": stop_engine, "closed": [], "failed": [], "symbols": [], "errors": []}

    monkeypatch.setattr(route.engine, "flatten_all", _fake_flatten)
    headers = _token(client)

    bad = client.post(f"{API}/engine/flatten", headers=headers, json={"confirm": "nope"})
    assert bad.status_code == 400
    assert "FLATTEN" in bad.json()["detail"]
    assert "hit" not in called

    ok = client.post(f"{API}/engine/flatten", headers=headers, json={"confirm": "flatten", "stop_engine": False})
    assert ok.status_code == 200, ok.text
    assert ok.json()["ok"] is True
    assert called["hit"] is True
