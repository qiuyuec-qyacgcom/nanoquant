# -*- coding: utf-8 -*-
"""信号源评估相关：channel 提取 + 事后命中率口径。"""
import pytest

from nanoquant.engine.worker import _channel_of


def test_channel_of_extracts_from_raw():
    assert _channel_of({"raw": {"channel": "某VIP频道", "symbol": "BTCUSDT"}}) == "某VIP频道"
    assert _channel_of({"raw": {"source": "t.me/xxx"}}) == "t.me/xxx"


def test_channel_of_falls_back_to_empty():
    assert _channel_of({"raw": {"symbol": "BTCUSDT"}}) == ""
    assert _channel_of({"raw": None}) == ""
    assert _channel_of({}) == ""


def test_channel_of_truncates_to_64():
    long_name = "频" * 100
    assert len(_channel_of({"raw": {"channel": long_name}})) == 64
