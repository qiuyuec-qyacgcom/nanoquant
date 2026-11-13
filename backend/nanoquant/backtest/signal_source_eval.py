# -*- coding: utf-8 -*-
"""信号源质量评估（已融合进 :mod:`factor_lab`，此文件仅为向后兼容保留）。

新代码请直接用 ``nanoquant.backtest.factor_lab`` 的 ``evaluate_signal_sources``；
``main`` 保持原 CLI（``python -m nanoquant.backtest.signal_source_eval ...``）不变。
"""
from __future__ import annotations

from .factor_lab import evaluate_signal_sources as evaluate, main_signals as main

__all__ = ["evaluate", "main"]


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
