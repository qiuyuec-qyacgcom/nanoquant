# -*- coding: utf-8 -*-
"""换信息源因子 IC（已融合进 :mod:`factor_lab`，此文件仅为向后兼容保留）。

新代码请直接用 ``nanoquant.backtest.factor_lab``：
- ``evaluate_funding`` / ``evaluate_cvd`` 直接复用
- ``main`` 保持原 CLI（``python -m nanoquant.backtest.factor_ic_advanced ...``）不变
"""
from __future__ import annotations

from .factor_lab import (
    evaluate_cvd,
    evaluate_funding,
    load_funding,
    main_advanced as main,
    percentile,
)

__all__ = ["evaluate_funding", "evaluate_cvd", "load_funding", "percentile", "main"]


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
