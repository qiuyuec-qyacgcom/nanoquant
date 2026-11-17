# -*- coding: utf-8 -*-
"""量化无方向时，必须让模型知道"方向由你判断".

事故复盘（2026-11 线上）：
1. 量化 flat 时 `_scan` 直接 `continue`，AI 一次都没被调用；
2. 即使被调用（少数非 flat 信号），模型看到 `direction_hint=flat` 也只会按
   "信号矛盾/数据不足即 hold" 一律否决。
这两条叠加的结果就是「AI 已开启、模型可用，但交易链路由被弃用的纯量化策略独占」。
本文件锁住第 2 条：上下文里必须有显式说明，v1/v2 两套协议都要带上。
"""
from __future__ import annotations

from nanoquant.ai.compact import format_snapshot_v2
from nanoquant.ai.prompts import format_snapshot

CTX = {
    "symbol": "DOGEUSDT",
    "price": 0.15,
    "score": 0.0,
    "side_hint": "flat",
    "quant_note": "量化模型本轮无方向（仅通过风险闸门），方向与是否入场由你独立判断",
    "quant_reasons": ["偏离不足 |z|=0.6 < 2.5"],
    "features": {"rsi": 52.0, "atr_pct": 0.8, "vol_ratio": 1.2, "band_z": 0.6},
    "klines": [],
    "change_24h": 1.0,
    "quote_volume": 5e7,
    "funding": 0.0001,
    "account": {"equity": 20.0, "available": 15.0, "open_positions": 0},
    "constraints": {"min_leverage": 8, "max_leverage": 30, "min_confidence": 0.45,
                    "fee_rate_taker": 0.0005},
    "news": [],
    "mantras": [],
    "strategy": None,
}


def test_v2_marks_quant_without_direction():
    payload = format_snapshot_v2(CTX)
    assert '"d":"-"' in payload, "无方向候选的方向提示必须是 '-'"
    assert '"q0":1' in payload, "必须显式标记「量化本轮无方向」，避免模型把 '-' 当否决票"


def test_v1_carries_quant_note():
    payload = format_snapshot(CTX)
    assert "量化模型本轮无方向" in payload


def test_direction_hint_is_not_marked_when_quant_has_direction():
    ctx = dict(CTX, side_hint="long", quant_note="")
    v2 = format_snapshot_v2(ctx)
    assert '"q0"' not in v2
    assert '"d":"L"' in v2
    assert "量化模型本轮无方向" not in format_snapshot(ctx)
