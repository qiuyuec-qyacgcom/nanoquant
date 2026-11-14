"""告警推送：级别过滤 / 静默时段 / 冷却 / 摘要合并 / 发送结果落库.

告警是"出事时唯一会叫的东西"，本身却几乎没人验证过。
这里用替身配置 + 假 HTTP 客户端把每条分支走一遍，不碰真实网络。
"""
from __future__ import annotations

import asyncio
import time
from typing import Any

import pytest

from nanoquant.db import session_scope
from nanoquant.models import AlertLog
from nanoquant.services import alerts


class FakeCfg:
    alert_enabled = True
    alert_min_level = "warn"
    alert_webhook_url = "https://example.invalid/hook"
    alert_error_webhook_url = ""
    alert_telegram_token = ""
    alert_telegram_chat_id = ""
    alert_cooldown_sec = 120
    alert_quiet_hours = ""
    alert_digest_enabled = False
    alert_digest_minutes = 30
    alert_template = ""


@pytest.fixture(autouse=True)
def _clean_state(monkeypatch):
    alerts._LAST.clear()
    alerts._PENDING.clear()
    monkeypatch.setattr(alerts, "get_system", lambda: FakeCfg())
    yield
    alerts._LAST.clear()
    alerts._PENDING.clear()


def _at(hour: int, minute: int):
    return lambda: time.struct_time((2026, 1, 1, hour, minute, 0, 3, 1, 0))


# ------------------------------------------------------------------ 纯函数
def test_format_default_and_template():
    text = alerts._format("error", "engine", "下单失败")
    assert "🔴" in text and "ERROR" in text and "下单失败" in text

    # 模板首尾空白会被 strip 掉
    assert alerts._format("warn", "db", "慢", " [{level}] {category}: {message} ") == "[WARN] db: 慢"
    # 模板写错不能把告警本身搞丢
    assert "慢" in alerts._format("warn", "db", "慢", "{no_such_field}")


def test_quiet_hours_cross_midnight(monkeypatch):
    monkeypatch.setattr(alerts.time, "localtime", _at(23, 30))
    assert alerts._in_quiet_hours("23:00-07:00") is True
    assert alerts._in_quiet_hours("09:00-17:00") is False

    monkeypatch.setattr(alerts.time, "localtime", _at(6, 59))
    assert alerts._in_quiet_hours("23:00-07:00") is True

    monkeypatch.setattr(alerts.time, "localtime", _at(7, 0))
    assert alerts._in_quiet_hours("23:00-07:00") is False, "结束时刻不含在内"


@pytest.mark.parametrize("spec", ["", "not-a-range", "aa:bb-cc:dd", "09:00-09:00"])
def test_quiet_hours_invalid_specs_are_false(spec):
    assert alerts._in_quiet_hours(spec) is False


# ------------------------------------------------------------------ 门禁
def test_disabled_alerts_send_nothing(monkeypatch):
    class Off(FakeCfg):
        alert_enabled = False

    monkeypatch.setattr(alerts, "get_system", lambda: Off())
    asyncio.run(_run_on_event("error", "engine", "boom"))
    assert alerts._LAST == {}


def test_level_below_threshold_is_dropped():
    asyncio.run(_run_on_event("info", "engine", "普通信息"))
    assert "engine" not in alerts._LAST


def test_quiet_hours_blocks_warn_but_not_error(monkeypatch):
    class Quiet(FakeCfg):
        alert_quiet_hours = "23:00-07:00"

    monkeypatch.setattr(alerts, "get_system", lambda: Quiet())
    monkeypatch.setattr(alerts.time, "localtime", _at(23, 30))

    asyncio.run(_run_on_event("warn", "engine", "夜里的小问题"))
    assert "engine" not in alerts._LAST

    asyncio.run(_run_on_event("error", "engine", "夜里的大事"))
    assert "engine" in alerts._LAST


def test_cooldown_suppresses_repeat_within_window():
    async def scenario():
        alerts.on_event("error", "engine", "第一次")
        alerts.on_event("error", "engine", "第二次")
        await asyncio.sleep(0)

    asyncio.run(scenario())
    assert "engine" in alerts._LAST

    before = dict(alerts._LAST)
    asyncio.run(_run_on_event("error", "engine", "第三次"))
    assert alerts._LAST["engine"] == before["engine"], "冷却窗口内不该刷新时间戳"


def test_no_channel_configured_does_nothing(monkeypatch):
    class NoChannel(FakeCfg):
        alert_webhook_url = ""
        alert_error_webhook_url = ""

    monkeypatch.setattr(alerts, "get_system", lambda: NoChannel())
    asyncio.run(_run_on_event("error", "engine", "没通道"))
    assert alerts._LAST == {}


def test_warn_buffered_in_digest_mode(monkeypatch):
    class Digest(FakeCfg):
        alert_digest_enabled = True

    monkeypatch.setattr(alerts, "get_system", lambda: Digest())
    asyncio.run(_run_on_event("warn", "scanner", "扫描变慢"))
    assert alerts.pending_digest() == 1
    assert "scanner" not in alerts._LAST, "摘要模式下 warn 不进冷却表"


def test_flush_digest_merges_and_clears(monkeypatch):
    sent: list[tuple[str, str, str]] = []

    async def fake_send(level: str, category: str, message: str):
        sent.append((level, category, message))
        return [{"channel": "webhook", "ok": True}]

    monkeypatch.setattr(alerts, "send", fake_send)
    alerts._PENDING.extend([("scanner", "a"), ("scanner", "b"), ("db", "c")])
    alerts._LAST_DIGEST = 0.0
    result = asyncio.run(alerts.flush_digest(force=True))

    assert result is not None
    assert result["count"] == 3
    assert alerts.pending_digest() == 0
    assert sent[0][1] == "digest" and "scanner×2" in sent[0][2]


def test_flush_digest_noop_when_empty():
    assert asyncio.run(alerts.flush_digest(force=True)) is None


# ------------------------------------------------------------------ 发送
class _FakeResponse:
    def __init__(self, status: int, text: str = "ok"):
        self.status_code = status
        self.text = text


class _FakeClient:
    def __init__(self, status: int = 200):
        self.status = status
        self.calls: list[tuple[str, dict[str, Any]]] = []

    async def post(self, url: str, json: dict[str, Any]):
        self.calls.append((url, json))
        return _FakeResponse(self.status)


def test_send_posts_json_and_records_result(monkeypatch):
    client = _FakeClient(200)
    monkeypatch.setattr(alerts, "_http", lambda: client)

    results = asyncio.run(alerts.send("error", "broker", "下单被拒"))
    assert results[0]["ok"] is True
    assert client.calls[0][0] == "https://example.invalid/hook"
    assert client.calls[0][1]["msgtype"] == "text"

    with session_scope() as s:
        row = s.query(AlertLog).order_by(AlertLog.id.desc()).first()
        assert row is not None and row.category == "broker" and row.ok is True


def test_send_marks_failure_when_http_errors(monkeypatch):
    monkeypatch.setattr(alerts, "_http", lambda: _FakeClient(500))
    results = asyncio.run(alerts.send("warn", "db", "慢查询"))
    assert results[0]["ok"] is False
    assert results[0]["status"] == 500


def test_send_uses_error_channel_for_error_level(monkeypatch):
    class TwoChannel(FakeCfg):
        alert_webhook_url = "https://example.invalid/default"
        alert_error_webhook_url = "https://example.invalid/oncall, https://example.invalid/backup"

    monkeypatch.setattr(alerts, "get_system", lambda: TwoChannel())
    client = _FakeClient(200)
    monkeypatch.setattr(alerts, "_http", lambda: client)

    asyncio.run(alerts.send("error", "engine", "爆仓风险"))
    urls = [call[0] for call in client.calls]
    assert urls == ["https://example.invalid/oncall", "https://example.invalid/backup"]


async def _run_on_event(level: str, category: str, message: str) -> None:
    alerts.on_event(level, category, message)
    # 让 create_task 的发送任务有机会跑起来
    await asyncio.sleep(0)
