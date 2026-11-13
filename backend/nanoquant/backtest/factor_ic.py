# -*- coding: utf-8 -*-
"""因子 IC 评估（已融合进 :mod:`factor_lab`，此文件仅为向后兼容保留）。

新代码请直接用 ``nanoquant.backtest.factor_lab``：
- ``evaluate_factors`` 等价于本文件的 ``evaluate``
- ``load_klines`` 等价于本文件的 ``_load_klines``
- ``main`` 保持原 CLI（``python -m nanoquant.backtest.factor_ic ...``）不变
"""
from __future__ import annotations

from .factor_lab import (
    evaluate_factors as evaluate,
    hit_rate,
    load_klines as _load_klines,
    main_factors as main,
    spearman,
)

__all__ = ["evaluate", "spearman", "hit_rate", "_load_klines", "main"]


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
