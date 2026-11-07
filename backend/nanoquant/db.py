"""SQLite 访问层 (SQLAlchemy 2.0 同步引擎, WAL)."""
from __future__ import annotations

import contextlib
import logging
import time
from collections.abc import Iterator

from sqlalchemy import create_engine, event
from sqlalchemy.orm import DeclarativeBase, Session, sessionmaker

from .config import paths

log = logging.getLogger("NanoQuant.db")


class Base(DeclarativeBase):
    pass


engine = create_engine(
    f"sqlite:///{paths.DB_PATH}",
    connect_args={"check_same_thread": False, "timeout": 15},
    pool_pre_ping=True,
    future=True,
)


@event.listens_for(engine, "connect")
def _sqlite_pragmas(dbapi_conn, _record) -> None:  # pragma: no cover - 驱动回调
    """连接级 PRAGMA.

    注意: journal_mode 是数据库文件级且持久化的, 每次连接都执行一次
    `PRAGMA journal_mode=WAL` 会短暂申请写锁, 在并发连接下可能把读请求
    拖到 busy_timeout (表现为偶发 15~30s 的"卡顿"), 因此只在 init_db 里设置一次.
    """
    cur = dbapi_conn.cursor()
    cur.execute("PRAGMA synchronous=NORMAL")
    cur.execute("PRAGMA busy_timeout=8000")
    cur.execute("PRAGMA foreign_keys=ON")
    cur.close()


@event.listens_for(engine, "before_cursor_execute")
def _before_execute(conn, cursor, statement, parameters, context, executemany):  # pragma: no cover
    context._nq_t0 = time.perf_counter()


@event.listens_for(engine, "after_cursor_execute")
def _after_execute(conn, cursor, statement, parameters, context, executemany):  # pragma: no cover
    t0 = getattr(context, "_nq_t0", None)
    if t0 is None:
        return
    cost = (time.perf_counter() - t0) * 1000
    if cost > 1500:
        log.warning("慢SQL %.0fms: %s", cost, statement.replace("\n", " ")[:200])


SessionLocal = sessionmaker(bind=engine, expire_on_commit=False, autoflush=False, future=True)


def init_db() -> None:
    from . import models  # noqa: F401  确保模型注册

    # journal_mode 不能在事务内切换, 用 AUTOCOMMIT 连接设置一次即可 (写入库文件并持久化)
    with engine.connect().execution_options(isolation_level="AUTOCOMMIT") as conn:
        conn.exec_driver_sql("PRAGMA journal_mode=WAL")
    Base.metadata.create_all(engine)
    migrate_legacy_demo_naming()
    migrate_pool_columns()
    migrate_exec_columns()


#: 标的池新增列 (CREATE TABLE 只对新库生效, 老库必须 ALTER)
_POOL_COLUMNS = {
    "name": "VARCHAR(32) DEFAULT ''",
    "tier": "VARCHAR(24) DEFAULT 'tier_2_momentum'",
    "sl_atr_mult": "FLOAT DEFAULT 2.2",
    "precision": "INTEGER DEFAULT 3",
    "tick_sz": "VARCHAR(32) DEFAULT '0.01'",
    "min_sz": "VARCHAR(32) DEFAULT '0.001'",
    "min_notional": "FLOAT DEFAULT 5.0",
    "ct_val": "FLOAT DEFAULT 1.0",
    "sort_order": "INTEGER DEFAULT 0",
    "created_at": "INTEGER DEFAULT 0",
}


def migrate_pool_columns() -> None:
    """给老库的 trading_symbols 补齐 astra 标准池字段 (幂等)."""
    with engine.begin() as conn:
        try:
            rows = conn.exec_driver_sql("PRAGMA table_info(trading_symbols)").fetchall()
        except Exception as exc:  # noqa: BLE001
            log.warning("标的池列探测失败: %s", exc)
            return
        existing = {row[1] for row in rows}
        for column, ddl in _POOL_COLUMNS.items():
            if column in existing:
                continue
            try:
                conn.exec_driver_sql(f"ALTER TABLE trading_symbols ADD COLUMN {column} {ddl}")
                log.info("标的池补齐字段: %s", column)
            except Exception as exc:  # noqa: BLE001
                log.warning("标的池字段 %s 添加失败: %s", column, exc)
    _backfill_sort_order()


#: 执行质量观测新增列 (老库自动 ALTER)
_EXEC_COLUMNS = {
    "orders": {"latency_ms": "INTEGER DEFAULT 0"},
    "positions": {
        "entry_slippage_bps": "FLOAT DEFAULT 0.0",
        "initial_sl_price": "FLOAT DEFAULT 0.0",
        "partial_taken_at": "INTEGER DEFAULT 0",
    },
    "trades": {"slippage_bps": "FLOAT DEFAULT 0.0"},
    "signals": {"mode": "VARCHAR(8) DEFAULT 'demo'", "repeats": "INTEGER DEFAULT 1"},
    "write_failures": {"auto_attempts": "INTEGER DEFAULT 0"},
}


def migrate_exec_columns() -> None:
    """给 orders/positions/trades 补齐执行质量字段 (幂等)."""
    for table, columns in _EXEC_COLUMNS.items():
        try:
            with engine.begin() as conn:
                rows = conn.exec_driver_sql(f"PRAGMA table_info({table})").fetchall()
                existing = {row[1] for row in rows}
                for column, ddl in columns.items():
                    if column in existing:
                        continue
                    conn.exec_driver_sql(f"ALTER TABLE {table} ADD COLUMN {column} {ddl}")
                    log.info("%s 补齐字段: %s", table, column)
        except Exception as exc:  # noqa: BLE001
            log.warning("%s 字段补齐跳过: %s", table, exc)


def _backfill_sort_order() -> None:
    """老库没有 sort_order: 按 created_at (同秒按 symbol) 回填成 1..n, 保住历史顺序."""
    from .models import TradingSymbol

    try:
        with session_scope() as s:
            rows = (
                s.query(TradingSymbol)
                .order_by(TradingSymbol.created_at.asc(), TradingSymbol.symbol.asc())
                .all()
            )
            if not rows:
                return
            ordered = [r for r in rows if not r.sort_order]
            if not ordered:
                return
            nxt = max((r.sort_order or 0) for r in rows)
            for row in ordered:
                nxt += 1
                row.sort_order = nxt
            log.info("标的池回填添加顺序: %d 条", len(ordered))
    except Exception as exc:  # noqa: BLE001
        log.warning("标的池顺序回填失败: %s", exc)


def migrate_legacy_demo_naming() -> None:
    """把旧版的 paper(纸面) 命名迁移为 demo(模拟盘), 幂等.

    只做数据搬家, 不改表结构; 新进程启动时会自动跑一次.
    """
    import json

    from .models import Setting

    statements = (
        "UPDATE orders SET mode='demo' WHERE mode='paper'",
        "UPDATE positions SET mode='demo' WHERE mode='paper'",
        "UPDATE trades SET mode='demo' WHERE mode='paper'",
        "UPDATE equity_points SET mode='demo' WHERE mode='paper'",
        "UPDATE engine_state SET value='demo' WHERE key='mode' AND value='paper'",
    )
    for sql in statements:
        try:
            # 每条独立事务: 旧库可能缺表, 单条失败不影响其余迁移
            with engine.begin() as conn:
                conn.exec_driver_sql(sql)
        except Exception as exc:  # noqa: BLE001
            log.warning("旧数据迁移跳过 (%s): %s", sql.split(" SET ")[0], exc)

    try:
        with session_scope() as s:
            row = s.get(Setting, "engine")
            if row is None:
                return
            try:
                data = json.loads(row.value or "{}")
            except json.JSONDecodeError:
                return
            if not isinstance(data, dict):
                return
            changed = False
            if data.get("mode") == "paper":
                data["mode"] = "demo"
                changed = True
            if "paper_start_balance" in data:
                data.setdefault("demo_start_balance", data.pop("paper_start_balance"))
                changed = True
            if changed:
                row.value = json.dumps(data, ensure_ascii=False)
                log.info("已将 engine 配置中的 paper 命名迁移为 demo")
    except Exception as exc:  # noqa: BLE001
        log.warning("engine 配置迁移失败: %s", exc)


@contextlib.contextmanager
def session_scope() -> Iterator[Session]:
    """事务作用域: 异常回滚."""
    session = SessionLocal()
    try:
        yield session
        session.commit()
    except Exception:
        session.rollback()
        raise
    finally:
        session.close()


def get_session() -> Iterator[Session]:
    """FastAPI 依赖注入."""
    session = SessionLocal()
    try:
        yield session
    finally:
        session.close()
