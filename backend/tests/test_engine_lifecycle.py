"""引擎进程生命周期：单实例锁、启停、空转心跳.

`start()` 的锁是整个系统唯一的"同一时间只能有一个引擎在交易"的保证。
它失效就意味着两个进程各下一单——重复开仓、重复平仓，账面立刻对不上。
"""
from __future__ import annotations

import asyncio
import time
from pathlib import Path

import pytest

from nanoquant.db import init_db
from nanoquant.engine import worker as worker_mod
from nanoquant.models import EngineEvent
from nanoquant.services import state
from nanoquant.services.settings import update_group


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


def _events_like(pattern: str) -> int:
    from nanoquant.db import session_scope

    with session_scope() as s:
        return s.query(EngineEvent).filter(EngineEvent.message.like(f"%{pattern}%")).count()


def test_start_is_idempotent_and_stop_is_clean():
    async def scenario():
        eng = worker_mod.TradingEngine()
        first = await eng.start()
        second = await eng.start()
        running = eng.running
        task_count = 1 + (1 if eng._follow_task else 0)
        await eng.stop()
        return first, second, running, task_count, eng.running

    first, second, running, task_count, still_running = asyncio.run(scenario())

    assert first is True
    assert second is True, "重复 start 应当是无害的（返回 True），而不是再起一个循环"
    assert running is True and task_count == 2
    assert still_running is False, "stop 之后必须释放任务与锁"


def test_second_process_cannot_start_engine():
    """两个 TradingEngine 抢同一把文件锁：第二个必须被拒绝."""

    async def scenario():
        a = worker_mod.TradingEngine()
        b = worker_mod.TradingEngine()
        ok_a = await a.start()
        before = _events_like("已在其它进程中运行")
        ok_b = await b.start()
        after = _events_like("已在其它进程中运行")
        await a.stop()
        await b.stop()
        return ok_a, ok_b, after - before

    ok_a, ok_b, warned = asyncio.run(scenario())

    assert ok_a is True
    assert ok_b is False, "第二个实例必须被文件锁挡住，否则会双份下单"
    assert warned == 1


def test_idle_tick_updates_cycle_timestamp(monkeypatch):
    """引擎未启用时循环仍在跑：写心跳、更新 last_cycle_ts，让面板知道进程活着."""
    real_sleep = asyncio.sleep

    async def _fast_sleep(sec, *args, **kwargs):
        # 循环体末尾会 sleep(cycle_interval_sec>=15)；否则 stop() 要等满 8s 超时
        return await real_sleep(0 if sec and sec >= 15 else sec, *args, **kwargs)

    monkeypatch.setattr(worker_mod.asyncio, "sleep", _fast_sleep)

    async def scenario():
        prev = state.get("last_cycle_ts")
        update_group("engine", {"enabled": False})
        eng = worker_mod.TradingEngine()

        async def _idle():
            return None  # 真实 _idle_tick 会去拉资讯（联网），这里只验证循环本身

        eng._idle_tick = _idle
        await eng.start()
        await asyncio.sleep(0.05)  # 循环第一轮会立刻跑，不用等满一个周期
        ts = state.get_int("last_cycle_ts", 0)
        await eng.stop()
        return ts, prev

    ts, prev = asyncio.run(scenario())

    assert ts > 0
    assert str(ts) != prev, "空转也必须刷新时间戳，否则会被判成引擎卡死"


def test_heartbeat_file_is_written():
    async def scenario():
        eng = worker_mod.TradingEngine()
        await eng._heartbeat()
        from nanoquant.config import paths

        return Path(paths.HEARTBEAT)

    path = asyncio.run(scenario())

    assert path.exists()
    text = path.read_text(encoding="utf-8")
    assert text.splitlines()[0].isdigit()
    assert text.splitlines()[1] in ("running", "idle")


def test_stop_without_start_is_safe():
    eng = worker_mod.TradingEngine()
    asyncio.run(eng.stop())  # 不应抛异常
    assert eng.running is False
