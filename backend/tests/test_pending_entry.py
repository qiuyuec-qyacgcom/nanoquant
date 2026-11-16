"""挂单待成交 ≠ 开仓失败（ExecResult.pending 语义）.

背景：post-only / 限价入场时交易所回执是 NEW + executedQty=0，
旧实现返回 `ExecResult(False, ...)` 且 worker 一律记 error「开仓失败」，
导致「委托已提交、止损止盈已预挂」被误报成失败并触发告警。
"""
from nanoquant.exchange.broker import ExecResult


def test_default_is_not_pending():
    """其他失败路径（下单失败、拒单）必须保持 pending=False。"""
    res = ExecResult(False, message="下单失败 -2019: 保证金不足")
    assert res.pending is False
    assert res.ok is False


def test_pending_entry_is_flagged_and_not_a_failure_message():
    res = ExecResult(
        False,
        message="挂单已提交，等待成交（NEW）；止损止盈已预挂，成交后自动生效并由对账接管仓位",
        order_id="123",
        pending=True,
    )
    assert res.pending is True
    assert res.ok is False  # 仍未建仓，不能当成功
    assert "失败" not in res.message, "待成交文案不应出现「失败」字样"
    assert "等待成交" in res.message
    assert res.order_id == "123"


def test_success_result_is_not_pending():
    res = ExecResult(True, price=100.0, qty=0.01, message="实盘开仓")
    assert res.ok is True
    assert res.pending is False
