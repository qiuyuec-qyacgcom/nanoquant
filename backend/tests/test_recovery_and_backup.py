"""熔断恢复路径与每日自动备份.

两块都是"配置看着存在、实际不生效"的重灾区：
- 日切锚点每天重置，但 ``halted`` 从不清理 → 引擎被昨天的亏损永久锁死；
- ``backup.auto_daily`` 默认开启，却没有任何调度者 → data/backups/ 永远是空目录。

这两个洞不会让测试变红，只会让人在真出事的那天发现"按钮没用"。
"""
from __future__ import annotations

import time
from types import SimpleNamespace

import pytest

from nanoquant.db import init_db, session_scope
from nanoquant.engine import risk
from nanoquant.models import BackupJob
from nanoquant.services import backup as backup_service
from nanoquant.services import state
from nanoquant.services.settings import get_engine, get_risk, update_group


@pytest.fixture(scope="module", autouse=True)
def _database():
    init_db()
    yield


@pytest.fixture(autouse=True)
def _clean_state():
    state.set_many({"halted": "", "day_anchor": "", "day_start_equity": "", "equity_peak": ""})
    yield
    state.set_many({"halted": "", "day_anchor": "", "day_start_equity": "", "equity_peak": ""})


# ------------------------------------------------------------ 熔断恢复
def test_day_rollover_clears_daily_loss_halt():
    """新的一天开始时，昨天的日亏熔断必须解除（基准已归零，判定不再成立）."""
    state.set_many(
        {
            "day_anchor": "2000-01-01",
            "day_start_equity": "10.0",
            "equity_peak": "10.0",
            "halted": "日内亏损 -67.43% 超过上限 25.0%",
        }
    )

    equity = risk.ensure_day_anchor(3.26, get_engine())

    assert equity == pytest.approx(3.26)
    assert state.get("halted") == ""
    assert state.get("day_anchor") != "2000-01-01"


def test_day_rollover_keeps_drawdown_halt():
    """回撤是累计事实，日切不能自动解除——否则每天开盘都把风控清零."""
    state.set_many(
        {
            "day_anchor": "2000-01-01",
            "day_start_equity": "10.0",
            "equity_peak": "10.0",
            "halted": "最大回撤 67.4% 超过上限 50.0%",
        }
    )

    risk.ensure_day_anchor(3.26, get_engine())

    assert "回撤" in state.get("halted")


def test_halt_only_clears_on_day_cut_not_every_cycle():
    """同一天内重复调用不能反复清熔断（否则熔断等于没有）."""
    state.set_many({"day_anchor": "2000-01-01", "day_start_equity": "10.0", "equity_peak": "10.0"})
    risk.ensure_day_anchor(10.0, get_engine())  # 触发日切
    state.set("halted", "日内亏损 -30.0% 超过上限 25.0%")

    risk.ensure_day_anchor(10.0, get_engine())  # 同一天第二次

    assert "日内亏损" in state.get("halted")


def test_global_gates_drawdown_halt_carries_recovery_hint():
    """面板只显示"已熔断"时操作员无从下手，原因里要写清怎么恢复."""
    from types import SimpleNamespace

    state.set_many({"day_anchor": "2099-01-01", "day_start_equity": "100", "equity_peak": "100"})
    account = SimpleNamespace(equity=40.0, available=40.0, unrealized=0.0)

    gate = risk.global_gates(account, get_engine(), _risk_cfg())

    assert gate.ok is False
    assert "重置日亏与回撤基准" in (gate.halt or "")


def _risk_cfg():
    from nanoquant.services.settings import get_risk

    return get_risk()


# ------------------------------------------------------------ 每日自动备份
def test_auto_backup_runs_at_most_once_per_day():
    base = int(time.time())
    update_group("backup", {"auto_daily": True, "last_run_ts": base - 2 * 86400})

    first = backup_service.run_if_due(now=base)
    assert first and first["ok"] is True
    assert first["size"] > 0

    again = backup_service.run_if_due(now=base + 3600)
    assert again is None, "同一天内重复备份只会把磁盘写满"

    next_day = backup_service.run_if_due(now=base + 86400)
    assert next_day and next_day["ok"] is True


def test_auto_backup_respects_switch():
    base = int(time.time())
    update_group("backup", {"auto_daily": False, "last_run_ts": 0})

    assert backup_service.run_if_due(now=base) is None


def test_auto_backup_failure_schedules_retry_in_an_hour():
    """失败不能占掉一整天：让下一轮检查（1 小时后）再试."""
    base = int(time.time())
    update_group("backup", {"auto_daily": True, "last_run_ts": 0})

    def _boom(**_kwargs):
        raise OSError("磁盘已满")

    original = backup_service.create_backup
    backup_service.create_backup = _boom  # type: ignore[assignment]
    try:
        out = backup_service.run_if_due(now=base)
    finally:
        backup_service.create_backup = original  # type: ignore[assignment]

    assert out and out["ok"] is False
    last = backup_service.get_backup().last_run_ts
    assert base - last == backup_service.AUTO_INTERVAL_SEC - backup_service.AUTO_RETRY_SEC


def test_backup_retention_prunes_old_files():
    update_group("backup", {"retention": 2, "auto_daily": False})
    for i in range(3):
        backup_service.create_backup(name=f"retention-{i}", created_by="test")

    with session_scope() as s:
        jobs = s.query(BackupJob).order_by(BackupJob.ts.desc()).all()
    assert len(jobs) <= 2
    for job in jobs:
        from pathlib import Path

        assert Path(job.file_path).exists()


def test_encrypted_backup_sha256_matches_file_on_disk():
    """加密备份的摘要必须取落盘字节.

    以前算的是加密前的 zip，于是**默认配置下每一个备份**在列表里都显示校验失败，
    真的文件损坏反而分辨不出来。
    """
    job = backup_service.create_backup(
        name="sha-check", settings_groups=["risk"], tables=[], encrypt=True, created_by="test"
    )
    listed = next(j for j in backup_service.list_jobs() if j["id"] == job["job_id"])

    assert listed["encrypted"] is True
    assert listed["sha256_ok"] is True


def test_restore_refuses_tampered_backup():
    """坏包还原会把半截数据写进库，必须在校验这一关就拒绝."""
    job = backup_service.create_backup(
        name="tamper", settings_groups=["risk"], tables=[], encrypt=True, created_by="test"
    )
    from pathlib import Path

    path = Path(job["path"])
    original = path.read_bytes()
    path.write_bytes(original[:-8] + b"tampered")

    result = backup_service.restore(job["job_id"], restore_settings=True, restore_tables=False)

    assert result["ok"] is False
    assert "校验失败" in result["error"]
    path.write_bytes(original)


def test_backup_restore_roundtrip():
    """备份 → 改坏配置 → 还原，值必须回到备份时刻."""
    from nanoquant.services.settings import get_group

    update_group("backup", {"auto_daily": False})
    before = get_group("public", use_cache=False)["site_title"]
    job = backup_service.create_backup(
        name="roundtrip", settings_groups=["public"], tables=[], created_by="test"
    )

    update_group("public", {"site_title": "被改坏了"})
    assert get_group("public", use_cache=False)["site_title"] == "被改坏了"

    result = backup_service.restore(job["job_id"], restore_settings=True, restore_tables=False)

    assert result["ok"] is True, result
    assert get_group("public", use_cache=False)["site_title"] == before


def test_backup_restore_rebuilds_tables():
    """表还原是"整表替换"：备份后删掉的行必须回来，新插的行不能留."""
    from nanoquant.models import TradeRecord, TradingSymbol

    update_group("backup", {"auto_daily": False, "retention": 20})
    with session_scope() as s:
        s.query(TradingSymbol).filter(TradingSymbol.symbol.in_(["BKPTA", "BKPTB"])).delete(
            synchronize_session=False
        )
        s.add(TradingSymbol(symbol="BKPTA", name="备份甲", tier="tier_2_momentum"))
        s.add(TradingSymbol(symbol="BKPTB", name="备份乙", tier="tier_2_momentum"))
        s.add(TradeRecord(symbol="BKPTA", side="long", qty=1, entry_price=1, exit_price=1, mode="demo"))

    job = backup_service.create_backup(
        name="tables", settings_groups=[], tables=["trading_symbols", "trades"], created_by="test"
    )
    assert job["manifest"]["row_counts"]["trading_symbols"] >= 2

    with session_scope() as s:
        s.query(TradingSymbol).filter(TradingSymbol.symbol.in_(["BKPTA", "BKPTB"])).delete(
            synchronize_session=False
        )
        s.add(TradingSymbol(symbol="BKPTZ", name="备份后新增", tier="tier_2_momentum"))

    result = backup_service.restore(job["job_id"], restore_settings=False, restore_tables=True)
    assert result["ok"] is True, result

    with session_scope() as s:
        names = {r.symbol for r in s.query(TradingSymbol).all()}
    assert {"BKPTA", "BKPTB"} <= names
    # 整表替换语义：备份之后新增的行会被清掉（这一点必须明确，否则"还原"会变成"合并"）
    assert "BKPTZ" not in names

    with session_scope() as s:
        s.query(TradingSymbol).filter(TradingSymbol.symbol.in_(["BKPTA", "BKPTB"])).delete(
            synchronize_session=False
        )
        s.query(TradeRecord).filter(TradeRecord.symbol == "BKPTA").delete(synchronize_session=False)


# ------------------------------------------------------- 回撤峰值口径
def test_rolling_peak_ignores_old_highs():
    """`drawdown_peak_window_days>0` 时，窗口外的历史高点不再压着当前权益."""
    from nanoquant.models import EquityPoint

    now = int(time.time())
    state.set_many({"equity_peak": "", "peak_anchor_ts": "0"})
    with session_scope() as s:
        s.add(EquityPoint(ts=now - 60 * 86400, mode="demo", wallet=100.0, unrealized=0.0))
        s.add(EquityPoint(ts=now - 3 * 86400, mode="demo", wallet=50.0, unrealized=0.0))

    # 60 天前的 100 在 30 天窗口外 → 峰值取窗口内的 50
    assert risk.update_peak(45.0, 30) == pytest.approx(50.0)

    # 窗口 0 = 历史最高：直接用持久化的峰值（不查库），所以这里手工写回 100
    state.set_many({"equity_peak": "100", "peak_anchor_ts": "0"})
    assert risk.update_peak(45.0, 0) == pytest.approx(100.0)


def test_rolling_peak_never_crosses_reset_anchor():
    """人工重置基准后，窗口内更早的高点不能再触发回撤熔断."""
    from nanoquant.models import EquityPoint

    now = int(time.time())
    with session_scope() as s:
        s.add(EquityPoint(ts=now - 2 * 86400, mode="demo", wallet=100.0, unrealized=0.0))
    state.set_many({"equity_peak": "40", "peak_anchor_ts": str(now - 3600)})

    assert risk.update_peak(40.0, 30) == pytest.approx(40.0)


def test_drawdown_window_unblocks_gates_where_all_time_peak_would_halt():
    """同一条权益：全历史峰值下熔断，30 天窗口下放行."""
    from nanoquant.models import EquityPoint

    now = int(time.time())
    with session_scope() as s:
        s.add(EquityPoint(ts=now - 60 * 86400, mode="demo", wallet=100.0, unrealized=0.0))
        s.add(EquityPoint(ts=now - 2 * 86400, mode="demo", wallet=50.0, unrealized=0.0))

    account = SimpleNamespace(equity=45.0, available=45.0, unrealized=0.0)
    state.set_many({"day_anchor": "2099-01-01", "day_start_equity": "45", "equity_peak": "100", "halted": ""})

    update_group("risk", {"max_drawdown_pct": 50.0, "drawdown_peak_window_days": 0})
    halted = risk.global_gates(account, get_engine(), get_risk())

    state.set_many({"equity_peak": "", "halted": ""})
    update_group("risk", {"drawdown_peak_window_days": 30})
    try:
        passed = risk.global_gates(account, get_engine(), get_risk())
    finally:
        update_group("risk", {"drawdown_peak_window_days": 0})

    assert halted.ok is False and "回撤" in (halted.halt or "")
    assert passed.ok is True, passed.reason


# ------------------------------------------------------- 权益曲线降采样
def test_equity_downsample_keeps_recent_and_thins_old():
    """48h 内每分钟一点保真；更早的压成每 10 分钟一点。"""
    from nanoquant.models import EquityPoint
    from nanoquant.services import retention

    now = int(time.time())
    with session_scope() as s:
        s.query(EquityPoint).delete(synchronize_session=False)
        # 3 小时前的一段"老数据"：每分钟一条，共 180 条
        base = now - 72 * 3600
        for i in range(180):
            s.add(EquityPoint(ts=base + i * 60, mode="demo", wallet=10.0, unrealized=0.0))
        # 最近 1 小时：每分钟一条，共 60 条
        for i in range(60):
            s.add(EquityPoint(ts=now - 3600 + i * 60, mode="demo", wallet=10.0, unrealized=0.0))

    removed = retention._downsample_equity(now)  # noqa: SLF001

    with session_scope() as s:
        old_kept = s.query(EquityPoint).filter(EquityPoint.ts < now - 48 * 3600).count()
        recent_kept = s.query(EquityPoint).filter(EquityPoint.ts >= now - 48 * 3600).count()
        s.query(EquityPoint).delete(synchronize_session=False)

    # 180 分钟 / 10 分钟桶 = 18 个点（±1 取决于桶边界）
    assert 17 <= old_kept <= 19, old_kept
    assert recent_kept == 60, "48 小时内不做降采样"
    assert removed == 180 - old_kept


def test_equity_downsample_is_noop_for_small_tables():
    from nanoquant.models import EquityPoint
    from nanoquant.services import retention

    now = int(time.time())
    with session_scope() as s:
        s.query(EquityPoint).delete(synchronize_session=False)
        # 两条落在不同的 10 分钟桶里 → 都该保留
        s.add(EquityPoint(ts=now - 72 * 3600, mode="demo", wallet=10.0))
        s.add(EquityPoint(ts=now - 72 * 3600 - 900, mode="demo", wallet=11.0))

    assert retention._downsample_equity(now) == 0  # noqa: SLF001
    with session_scope() as s:
        s.query(EquityPoint).delete(synchronize_session=False)
