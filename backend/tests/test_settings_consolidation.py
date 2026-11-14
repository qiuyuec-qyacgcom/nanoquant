"""配置收敛的防回归：删掉的影子字段不许再长回来.

背景：同一个概念曾经存在两份（system.timezone_offset / engine.timezone_offset），
UI 上出现两次、默认值一样，但只有 engine 那份被读；还有三个"控制杠杆"的入口，
其中一个（ai.max_position_leverage）从没被任何代码读过。
"""
from __future__ import annotations

from collections import defaultdict

from nanoquant.api.routes.backtest import _fee_bps  # noqa: SLF001
from nanoquant.api.schemas import BacktestIn
from nanoquant.services.settings import GROUPS, _migrate_legacy_group, get_risk, update_group  # noqa: SLF001


def test_only_one_timezone_setting_and_it_belongs_to_engine():
    engine = GROUPS["engine"].model_fields
    system = GROUPS["system"].model_fields
    assert "timezone_offset" in engine, "交易时段需要时区，engine/risk._now_local 读的是这份"
    assert "timezone_offset" not in system, "system 那份是死开关，别再加回来"


def test_disabled_ai_leverage_cap_is_gone():
    assert "max_position_leverage" not in GROUPS["ai"].model_fields
    # 杠杆只保留两个真相源：风控全局上下限，以及策略自选（只能收紧）
    assert {"default_leverage", "max_leverage"} <= set(GROUPS["risk"].model_fields)
    assert {"leverage_min", "leverage_max"} <= set(GROUPS["strategy"].model_fields)


def test_legacy_keys_are_dropped_on_load():
    system = _migrate_legacy_group("system", {"timezone_offset": 3, "log_level": "INFO"})
    assert "timezone_offset" not in system
    assert system["log_level"] == "INFO"

    ai = _migrate_legacy_group("ai", {"max_position_leverage": 35, "enabled": True})
    assert "max_position_leverage" not in ai
    assert ai["enabled"] is True


#: 同名但语义确实不同的字段，必须登记理由；新增同名不登记就报错，
#: 这样下次再复制粘贴出第二份 timezone_offset 会立刻被测试挡住。
HOMONYMS = {
    "enabled": "各子系统自己的总闸（引擎 / AI / 资讯 / 自进化 / 跟单）",
    "mode": "engine=交易环境, strategy=信号模型, ai=模型编排",
    "min_confidence": "ai=模型裁决置信度, evolution=心法入库置信度",
    "proxy": "data=行情代理, news=资讯代理（留空回落到 data）",
    "request_timeout_sec": "data=行情请求, news=资讯抓取",
}


def test_duplicate_keys_across_groups_must_be_declared():
    owner: dict[str, list[str]] = defaultdict(list)
    for group, model in GROUPS.items():
        for name in model.model_fields:
            owner[name].append(group)
    duplicates = {name: groups for name, groups in owner.items() if len(groups) > 1}

    undeclared = sorted(set(duplicates) - set(HOMONYMS))
    assert not undeclared, f"这些同名字段没有登记语义差异，请确认不是重复配置：{undeclared}"

    stale = sorted(set(HOMONYMS) - set(duplicates))
    assert not stale, f"这些字段已经不再重复，请从 HOMONYMS 里删掉：{stale}"


def test_backtest_fee_follows_risk_when_omitted():
    """回测手续费不该有第二个真相源：不传就取 risk.fee_rate_taker。"""
    before = get_risk().fee_rate_taker
    update_group("risk", {"fee_rate_taker": 0.0008})
    try:
        assert _fee_bps(BacktestIn(symbol="BTCUSDT")) == 8.0
        assert _fee_bps(BacktestIn(symbol="BTCUSDT", fee_bps=1.5)) == 1.5
    finally:
        update_group("risk", {"fee_rate_taker": before})
