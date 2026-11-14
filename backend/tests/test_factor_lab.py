"""因子实验室的统计口径：IC 就是 Spearman 秩相关，算错了整套选因子都白搭.

这里只测纯计算与"拿不到数据时的降级"，不联网。
"""
from __future__ import annotations

import pytest

from nanoquant.backtest import factor_lab


def test_percentile_counts_strictly_below():
    history = [1.0, 2.0, 3.0, 4.0]
    assert factor_lab.percentile(history, 0.0) == 0.0
    assert factor_lab.percentile(history, 2.5) == 0.5
    assert factor_lab.percentile(history, 99.0) == 1.0
    # 空样本没有参照物，取中位
    assert factor_lab.percentile([], 1.0) == 0.5


def test_rank_uses_average_for_ties():
    assert factor_lab._rank([10.0, 20.0, 30.0]) == [1.0, 2.0, 3.0]
    # 两个并列第一 → 平均秩 1.5
    assert factor_lab._rank([5.0, 5.0, 9.0]) == [1.5, 1.5, 3.0]
    assert factor_lab._rank([1.0, 1.0, 1.0]) == [2.0, 2.0, 2.0]


def test_spearman_perfect_and_inverse():
    x = [1.0, 2.0, 3.0, 4.0, 5.0]
    assert factor_lab.spearman(x, x) == pytest.approx(1.0)
    assert factor_lab.spearman(x, list(reversed(x))) == pytest.approx(-1.0)


def test_spearman_handles_short_and_constant_series():
    assert factor_lab.spearman([1.0, 2.0], [1.0, 2.0]) == 0.0, "少于 3 个点没有统计意义"
    flat = [3.0, 3.0, 3.0, 3.0]
    assert factor_lab.spearman(flat, [1.0, 2.0, 3.0, 4.0]) == 0.0, "分母为 0 时不能抛异常"


def test_spearman_ignores_monotonic_transform():
    """秩相关只看顺序：x 与 2x+1 的 IC 必须是 1。"""
    x = [1.0, 5.0, 3.0, 9.0, 7.0]
    y = [2 * v + 1 for v in x]
    assert factor_lab.spearman(x, y) == pytest.approx(1.0)


def test_hit_rate_counts_direction_and_zero_as_hit():
    assert factor_lab.hit_rate([1.0, -1.0, 0.0], [2.0, -3.0, 5.0]) == 1.0
    assert factor_lab.hit_rate([1.0, -1.0], [-1.0, 1.0]) == 0.0
    assert factor_lab.hit_rate([], []) == 0.0


def test_split_symbols_strips_but_keeps_case():
    """只做去空白与去空项；大小写由后续请求层处理（币安对大小写不敏感）。"""
    assert factor_lab._split_symbols("btcusdt, ETHUSDT  ,") == ["btcusdt", "ETHUSDT"]
    assert factor_lab._split_symbols("") == []


def test_evaluate_factors_reports_insufficient_data(monkeypatch):
    """数据不够时要给出人话，而不是抛异常或返回一堆 0。"""
    monkeypatch.setattr(factor_lab, "load_klines", lambda *a, **k: [])
    out = factor_lab.evaluate_factors("BTCUSDT", "15m", bars=500, forward=6)
    assert "error" in out and "数据不足" in out["error"]
