"""老库升级：模型新增字段必须能自动补齐.

背景：``signal_follows.channel`` 是先在模型里加的、迁移函数漏写，开发库靠手工
``ALTER TABLE`` 糊过去了；换一台机器/老库升级时第一次查跟单就会 ``no such column``。
"""
from __future__ import annotations

from sqlalchemy import create_engine, inspect

from nanoquant import db as dbmod


def test_ensure_schema_columns_backfills_missing_column_and_index(tmp_path):
    eng = create_engine(f"sqlite:///{tmp_path / 'legacy.db'}")
    with eng.begin() as conn:
        # 模拟一个"旧版本"的跟单表：只有最初的两个字段
        conn.exec_driver_sql(
            "CREATE TABLE signal_follows (id INTEGER PRIMARY KEY, signal_id VARCHAR(64))"
        )

    added = dbmod.ensure_schema_columns(eng)

    assert "signal_follows.channel" in added
    cols = {c["name"] for c in inspect(eng).get_columns("signal_follows")}
    assert {"channel", "status", "payload", "reason"} <= cols
    indexes = {i["name"] for i in inspect(eng).get_indexes("signal_follows")}
    assert "ix_signal_follows_channel" in indexes

    # 幂等：结构已经对齐后再跑一次不应产生任何改动
    assert dbmod.ensure_schema_columns(eng) == []


def test_ensure_schema_columns_skips_tables_that_do_not_exist(tmp_path):
    """空库交给 create_all，对账函数不该自己建表。"""
    eng = create_engine(f"sqlite:///{tmp_path / 'empty.db'}")
    assert dbmod.ensure_schema_columns(eng) == []
    assert inspect(eng).get_table_names() == []
