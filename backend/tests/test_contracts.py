"""语义契约测试: 把"改了不能坏"的规则固化成回归用例.

覆盖:
- 部分止盈的 R 计算 (含缺失止损的防御);
- 时间阶梯止盈的档位选择;
- 出场判定的优先级 (止损 / 止盈 / 阶梯止盈 / 移动止损 / 超时);
- 配置校验 (周期白名单、阶梯升序、部分止盈比例);
- 分段耗时的分位数计算;
- 回测引擎能跑通且不产生负数仓位;
- 执行 sidecar 协议/长连接复用与影子比对的判定.

这些用例**不联网、不碰数据库**, 适合放进 CI。
"""
from __future__ import annotations

from types import SimpleNamespace

import pytest
from pydantic import ValidationError

from nanoquant.backtest.engine import run_backtest
from nanoquant.engine.strategy import detect_exit, partial_tp_r, roi_target
from nanoquant.services.exec_metrics import ExecMetrics
from nanoquant.services.settings import StrategySettings


def bar(o: float, h: float, l: float, c: float, v: float = 1.0, t: int = 0):
    return SimpleNamespace(open=o, high=h, low=l, close=c, volume=v, open_time=t)


# ------------------------------------------------------------------ 部分止盈
def test_partial_tp_r_long_and_short():
    assert partial_tp_r("long", 100, 98, 102) == pytest.approx(1.0)
    assert partial_tp_r("long", 100, 98, 101) == pytest.approx(0.5)
    assert partial_tp_r("short", 100, 102, 98) == pytest.approx(1.0)
    assert partial_tp_r("short", 100, 102, 101) == pytest.approx(-0.5)


def test_partial_tp_r_guards_missing_stop():
    # 老数据没有初始止损时, 不能把 0 当成止损价算出假 R
    assert partial_tp_r("long", 100, 0, 110) == 0.0
    assert partial_tp_r("long", 0, 98, 110) == 0.0
    assert partial_tp_r("long", 100, 100, 110) == 0.0


# ------------------------------------------------------------ 时间阶梯止盈
def test_roi_target_picks_highest_reached_tier():
    table = [[0, 0.8], [15, 0.6], [30, 0.45], [60, 0.3], [120, 0.2]]
    assert roi_target(table, 0) == 0.8
    assert roi_target(table, 14.9) == 0.8
    assert roi_target(table, 15) == 0.6
    assert roi_target(table, 200) == 0.2
    assert roi_target([], 10) is None


# -------------------------------------------------------------- 出场判定优先级
def _cfg(**kwargs) -> StrategySettings:
    return StrategySettings(**kwargs)


def test_detect_exit_stop_loss_long():
    cfg = _cfg()
    hit, reason = detect_exit(
        side="long", entry_price=100, mark_price=97, peak_price=100,
        sl_price=98, tp_price=105, opened_at=0, now_ts=60, cfg=cfg, score=0.0,
    )
    assert hit and reason == "stop_loss"


def test_detect_exit_take_profit_long():
    cfg = _cfg()
    hit, reason = detect_exit(
        side="long", entry_price=100, mark_price=104, peak_price=104,
        sl_price=98, tp_price=103, opened_at=0, now_ts=60, cfg=cfg, score=0.0,
    )
    assert hit and reason == "take_profit"


def test_detect_exit_roi_tier():
    cfg = _cfg(roi_table=[[0, 0.5], [10, 0.2]])
    hit, reason = detect_exit(
        side="long", entry_price=100, mark_price=100.6, peak_price=100.6,
        sl_price=90, tp_price=120, opened_at=0, now_ts=60, cfg=cfg, score=0.0,
    )
    assert hit and reason == "roi"


def test_detect_exit_trailing_stop():
    cfg = _cfg(roi_table=[], trailing_pct=0.8)
    hit, reason = detect_exit(
        side="long", entry_price=100, mark_price=103.1, peak_price=104,
        sl_price=90, tp_price=0, opened_at=0, now_ts=60, cfg=cfg, score=0.0,
    )
    assert hit and reason == "trailing_stop"


def test_detect_exit_timeout():
    cfg = _cfg(roi_table=[], trailing_pct=0)
    hit, reason = detect_exit(
        side="long", entry_price=100, mark_price=100, peak_price=100,
        sl_price=90, tp_price=120, opened_at=0, now_ts=181 * 60, cfg=cfg, score=0.0,
    )
    assert hit and reason == "timeout"


# ------------------------------------------------------------------ 配置校验
def test_timeframe_whitelist():
    assert StrategySettings(timeframe="5m").timeframe == "5m"
    with pytest.raises(ValidationError):
        StrategySettings(timeframe="1m")


def test_roi_table_sorted_and_bounded():
    cfg = StrategySettings(roi_table=[[5, 0.3], [0, 0.6]])
    assert cfg.roi_table == [[0, 0.6], [5, 0.3]]
    with pytest.raises(ValidationError):
        StrategySettings(roi_table=[[0, 99]])


def test_partial_tp_ratio_bounds():
    assert StrategySettings(partial_tp_ratio=0.5).partial_tp_ratio == 0.5
    with pytest.raises(ValidationError):
        StrategySettings(partial_tp_ratio=1.5)
    with pytest.raises(ValidationError):
        StrategySettings(partial_tp_at_r=-1)


# ---------------------------------------------------------------- 分段耗时
def test_exec_metrics_percentiles():
    metrics = ExecMetrics(capacity=100)
    for value in [1.0, 2.0, 3.0, 4.0, 5.0]:
        metrics.record({"total_ms": value})
    summary = metrics.summary()
    assert summary["count"] == 5
    assert summary["total_ms"]["p50"] == pytest.approx(3.0, abs=0.1)
    assert summary["total_ms"]["max"] == 5.0
    metrics.reset()
    assert metrics.summary()["count"] == 0


# --------------------------------------------------------------------- 回测
def test_backtest_runs_without_crashing():
    candles = []
    price = 100.0
    for i in range(400):
        price *= 1.0005
        candles.append(
            bar(
                o=price,
                h=price * 1.001,
                l=price * 0.999,
                c=price,
                v=1000.0,
                t=i * 300_000,
            )
        )
    result = run_backtest(candles, StrategySettings(), symbol="TESTUSDT")
    summary = result.summary
    assert summary.get("error") is None
    assert summary["trades"] >= 0
    assert summary["fee_total"] >= 0
    assert summary["final_equity"] > 0
    for key in ("by_hour", "by_weekday", "by_hold", "by_exit"):
        assert key in summary
    for trade in result.trades:
        assert trade.qty >= 0
        assert trade.reason in {"stop_loss", "take_profit", "roi", "trailing_stop", "timeout"}


def test_backtest_needs_enough_bars():
    result = run_backtest([bar(1, 1, 1, 1, t=i * 60_000) for i in range(10)], StrategySettings())
    assert "error" in result.summary


# -------------------------------------------------------------- 部分平仓数量
def test_partial_close_qty_rounding_and_min():
    from nanoquant.engine.strategy import partial_close_qty

    # 正常取整
    assert partial_close_qty(1.0, 0.5, 0.001, 0.001) == pytest.approx(0.5)
    # 取整后低于最小下单量 → 放弃
    assert partial_close_qty(0.015, 0.5, 0.001, 0.01) == 0.0
    # 取整后为 0 → 放弃
    assert partial_close_qty(0.001, 0.5, 0.001, 0.001) == 0.0
    # 无步进时按原值
    assert partial_close_qty(1.0, 0.3, 0.0, 0.001) == pytest.approx(0.3)
    # 非法输入
    assert partial_close_qty(0, 0.5, 0.001, 0.001) == 0.0
    assert partial_close_qty(1.0, 0, 0.001, 0.001) == 0.0


# ---------------------------------------------------------------- 密钥归一化
def test_normalize_pem_and_public_key_detection():
    import base64

    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

    from nanoquant.exchange.client import looks_like_public_key, normalize_pem

    pem = Ed25519PrivateKey.generate().public_key().public_bytes(
        Encoding.PEM, PublicFormat.SubjectPublicKeyInfo
    ).decode()
    flat = normalize_pem(pem)
    assert "\n" not in flat and "-----" not in flat
    assert flat == base64.b64encode(base64.b64decode(flat)).decode()
    assert looks_like_public_key(pem) is True
    assert looks_like_public_key("a" * 64) is False


def test_sign_query_accepts_pem_private_key():
    import base64

    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, NoEncryption, PrivateFormat

    from nanoquant.exchange.client import sign_query

    priv = Ed25519PrivateKey.generate()
    pem = priv.private_bytes(Encoding.PEM, PrivateFormat.PKCS8, NoEncryption()).decode()
    query = "x=9&timestamp=1700000000000"
    priv.public_key().verify(base64.b64decode(sign_query(query, pem)), query.encode())


# ------------------------------------------------------- 单行 PEM(真实踩坑)
def test_single_line_pem_private_key_signs():
    """币安页面/编辑器常把 PEM 压成一行, 归一化必须按标记片段删除而不是按行过滤."""
    import base64

    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, NoEncryption, PrivateFormat

    from nanoquant.exchange.client import normalize_pem, sign_query

    priv = Ed25519PrivateKey.generate()
    pem = priv.private_bytes(Encoding.PEM, PrivateFormat.PKCS8, NoEncryption()).decode()
    one_line = " ".join(pem.split())

    body = normalize_pem(one_line)
    assert body and "-----" not in body
    assert base64.b64decode(body)

    query = "symbol=BTCUSDT&timestamp=1700000000000"
    priv.public_key().verify(base64.b64decode(sign_query(query, one_line)), query.encode())


# ---------------------------------------------------------------- 交易所签名
def test_sign_query_supports_ed25519_and_hmac():
    import base64
    import hashlib
    import hmac

    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, NoEncryption, PrivateFormat

    from nanoquant.exchange.client import sign_query

    query = "a=1&timestamp=1700000000000"

    # Ed25519: 签名是 base64, 能用公钥验证
    priv = Ed25519PrivateKey.generate()
    secret = base64.b64encode(
        priv.private_bytes(Encoding.DER, PrivateFormat.PKCS8, NoEncryption())
    ).decode()
    priv.public_key().verify(base64.b64decode(sign_query(query, secret)), query.encode())

    # HMAC: 行为保持不变
    hmac_secret = "plain-hmac-secret"
    expect = hmac.new(hmac_secret.encode(), query.encode(), hashlib.sha256).hexdigest()
    assert sign_query(query, hmac_secret) == expect


# ---------------------------------------------------------------- 告警静默
def test_quiet_hours_parsing():
    from nanoquant.services.alerts import _in_quiet_hours

    assert _in_quiet_hours("") is False
    assert _in_quiet_hours("bad-spec") is False
    # 全天区间本质上等价于 start == end -> 视为不静默
    assert _in_quiet_hours("08:00-08:00") is False


# ---------------------------------------------------------------- 写库容错
def test_write_retryable_classification():
    from nanoquant.services.db_writer import _is_retryable

    assert _is_retryable("sqlite3.OperationalError: database is locked")
    assert _is_retryable("database table is locked")
    assert not _is_retryable("boom")
    assert not _is_retryable("")


# ------------------------------------------------------------------ 参数网格
def test_grid_runs_and_ranks():
    from nanoquant.backtest.grid import run_grid

    candles = []
    price = 100.0
    for i in range(400):
        price *= 1.0006 if i % 3 else 0.9997
        candles.append(bar(o=price, h=price * 1.001, l=price * 0.999, c=price, v=1000.0, t=i * 300_000))

    result = run_grid(
        candles,
        StrategySettings(),
        symbol="TESTUSDT",
        grid={"entry_threshold": [0.2, 0.4], "sl_atr_mult": [1.0, 1.5]},
        leverage=20,
    )
    assert result["runs"] == 4
    assert result["best"] is not None
    returns = [row["summary"]["total_return_pct"] for row in result["items"]]
    assert returns == sorted(returns, reverse=True)
    assert set(result["items"][0]["params"]) == {"entry_threshold", "sl_atr_mult"}


# ------------------------------------------------------------ 执行 sidecar 协议
def test_exec_sidecar_signs_over_reused_connection():
    """协议: health / plan 都要能走同一条连接 (每单重建连接会把握手塞进签名路径)."""
    import asyncio
    import json

    from nanoquant.exchange.client import sign_query
    from nanoquant.exchange.exec_client import ExecSidecar

    secret = "unit-test-hmac-secret"
    query = "symbol=BTCUSDT&side=BUY&type=MARKET&timestamp=1700000000000"
    accepted = {"n": 0}

    async def handler(reader, writer):
        accepted["n"] += 1
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
                req = json.loads(line.decode())
                if req["op"] == "health":
                    resp = {"id": req["id"], "ok": True, "algo": "hmac-sha256"}
                else:
                    resp = {
                        "id": req["id"],
                        "ok": True,
                        "algo": "hmac-sha256",
                        "signature": sign_query(req["query"], secret),
                        "query": req["query"],
                    }
                writer.write((json.dumps(resp) + "\n").encode())
                await writer.drain()
        finally:
            writer.close()

    async def run():
        server = await asyncio.start_server(handler, "127.0.0.1", 0)
        port = server.sockets[0].getsockname()[1]
        sidecar = ExecSidecar(f"tcp:127.0.0.1:{port}", timeout=2)
        try:
            health = await sidecar.health()
            assert health["algo"] == "hmac-sha256"
            first = await sidecar.sign(query)
            second = await sidecar.sign(query)
            assert first["signature"] == sign_query(query, secret) == second["signature"]
            assert first["query"] == query
            assert sidecar.counters["connects"] == 1
        finally:
            await sidecar.close()
            server.close()
            await server.wait_closed()

    asyncio.run(run())


def test_exec_sidecar_unreachable_raises():
    import asyncio

    from nanoquant.exchange.exec_client import ExecSidecar, ExecSidecarError

    async def run():
        # 9 端口在本机恒不可用: 只验证"连接失败要有明确错误", 不依赖外部网络
        sidecar = ExecSidecar("tcp:127.0.0.1:9", timeout=0.3)
        try:
            await sidecar.sign("a=1")
        except ExecSidecarError as exc:
            assert "sidecar" in str(exc)
        else:  # pragma: no cover - 极少数环境 9 端口可写
            raise AssertionError("不可达地址竟然成功")
        assert sidecar.counters["errors"] >= 1
        assert sidecar.connected is False

    asyncio.run(run())


# ------------------------------------------------------------ 影子比对判定
class _FakeSidecar:
    """最小 sidecar 替身: 只回一个签名, 其余属性对齐 ExecSidecar."""

    def __init__(self, signature: str, *, query_echo: bool = True) -> None:
        self.signature = signature
        self.query_echo = query_echo
        self.timeout = 1.0
        self.target = "tcp:fake"
        self.connected = True
        self.algo = "hmac-sha256"
        self.last_error = ""
        self.counters = {"requests": 0, "errors": 0, "connects": 0, "timeouts": 0}

    def reconfigure(self, target, timeout=None):  # noqa: ARG002
        return None

    async def sign(self, query: str) -> dict:
        self.counters["requests"] += 1
        return {
            "ok": True,
            "algo": self.algo,
            "query": query if self.query_echo else query + "x",
            "signature": self.signature,
        }


def _shadow(client, mode: str):
    from nanoquant.services.exec_shadow import ExecShadow, ShadowConfig

    return ExecShadow(
        client=client,
        config_provider=lambda: ShadowConfig(mode=mode, target="", sample_pct=100, timeout_ms=300),
        notifier=lambda *_: None,
    )


def test_exec_shadow_enforce_blocks_mismatch():
    import asyncio

    from nanoquant.services.exec_shadow import ExecShadowError

    fake = _FakeSidecar("not-the-local-signature")
    shadow = _shadow(fake, "enforce")

    async def run():
        try:
            await shadow.observe("a=1", "local-signature")
        except ExecShadowError:
            return "blocked"
        return "passed"

    assert asyncio.run(run()) == "blocked"
    assert shadow.counts["mismatched"] == 1
    assert shadow.last_mismatch and shadow.last_mismatch["echo_ok"] is True
    assert "签名不一致" in shadow.status()["verdict"]


def test_exec_shadow_enforce_accepts_match_and_ignores_echo_bug():
    import asyncio

    fake = _FakeSidecar("same")
    shadow = _shadow(fake, "enforce")

    assert asyncio.run(shadow.observe("a=1", "same")) is None
    assert shadow.counts["matched"] == 1

    # 签名一致但 query 回显被改写: 说明协议层串了包, 同样视为不一致
    from nanoquant.services.exec_shadow import ExecShadowError

    fake2 = _FakeSidecar("same", query_echo=False)
    shadow2 = _shadow(fake2, "enforce")
    with pytest.raises(ExecShadowError):
        asyncio.run(shadow2.observe("a=1", "same"))
    assert shadow2.counts["mismatched"] == 1


def test_exec_shadow_shadow_mode_is_async_and_never_raises():
    import asyncio

    fake = _FakeSidecar("mismatch")
    shadow = _shadow(fake, "shadow")

    async def run():
        await shadow.observe("a=1", "local")
        assert shadow.counts["inflight"] == 1
        await asyncio.sleep(0.05)

    asyncio.run(run())
    assert shadow.counts["matched"] == 0
    assert shadow.counts["mismatched"] == 1
    assert shadow.counts["inflight"] == 0
    assert shadow.counts["skipped"] == 0


def test_exec_shadow_verdict_degrades_when_sidecar_down():
    fake = _FakeSidecar("x")
    fake.connected = False
    shadow = _shadow(fake, "shadow")
    # 历史一致过, 但现在连不上: 判据必须是"当前不可用", 不能继续显示"一致"
    shadow.counts["matched"] = 3
    shadow.counts["errors"] = 5
    shadow.last_error = "sidecar 响应超时(800ms)"

    verdict = shadow.status()["verdict"]
    assert "不可用" in verdict
    assert "降级" in verdict


def test_exec_shadow_latency_uses_client_roundtrip():
    """延迟口径必须是 sidecar 往返, 不能把排队时间算进去 (否则宕机时 p99 假性飙高)."""
    import asyncio

    fake = _FakeSidecar("same")
    fake.last_ms = 0.25
    shadow = _shadow(fake, "enforce")

    asyncio.run(shadow.observe("a=1", "same"))
    assert shadow.status()["latency_ms"]["p50"] == pytest.approx(0.25, abs=0.001)


def test_exec_shadow_off_skips_everything():
    import asyncio

    fake = _FakeSidecar("whatever")
    shadow = _shadow(fake, "off")

    asyncio.run(shadow.observe("a=1", "local"))
    assert fake.counters["requests"] == 0
    assert shadow.counts["matched"] == 0 and shadow.counts["mismatched"] == 0
    assert "未启用" in shadow.status()["verdict"]
