"""决策日志 → 打标 → RAG 经验库 闭环.

借鉴「会自我学习的 Agent」的核心机制，但只保留可验证、不烧 token 的工程骨架:

1. **日志 (record)** —— AI 每次给出方向判断时留一份决策快照；
2. **打标 (label)** —— 24h 后回访，把预测方向 vs 实际价格对齐成对/错/部分；
3. **经验库 (experience)** —— 打标结论带标签归档，下次按相似场景检索 Top-k 注入。

与 ``evolution``（平仓复盘 → 规则级心法）互补，这里专注「决策级 + 场景化」。
"""
from __future__ import annotations

from .experience import (
    archive_labeled,
    delete_experience,
    experience_stats,
    injection,
    list_experiences,
    retrieve_top_k,
)
from .label import label_due
from .loop import run_cycle
from .record import journal_stats, list_journal, record_decision

__all__ = [
    "record_decision",
    "list_journal",
    "journal_stats",
    "label_due",
    "archive_labeled",
    "retrieve_top_k",
    "list_experiences",
    "experience_stats",
    "delete_experience",
    "injection",
    "run_cycle",
]
