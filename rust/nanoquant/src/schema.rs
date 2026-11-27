use std::collections::HashSet;

use rusqlite::{params, Connection};
use serde::Deserialize;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub created_tables: Vec<String>,
    pub added_columns: Vec<String>,
    pub added_indexes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Snapshot {
    tables: Vec<TableSpec>,
}

#[derive(Debug, Deserialize)]
struct TableSpec {
    name: String,
    sql: String,
    columns: Vec<ColumnSpec>,
    indexes: Vec<IndexSpec>,
}

#[derive(Debug, Deserialize)]
struct ColumnSpec {
    name: String,
    #[serde(rename = "type")]
    column_type: String,
    // 建表/补列只用 name+type；这三列只有基线比对测试要读，正式构建不编译。
    #[cfg(test)]
    #[serde(default)]
    notnull: bool,
    #[cfg(test)]
    #[serde(default, rename = "default")]
    default_value: Option<String>,
    #[cfg(test)]
    #[serde(default)]
    pk: bool,
}

#[derive(Debug, Deserialize)]
struct IndexSpec {
    name: String,
    #[serde(default)]
    unique: bool,
    #[serde(default)]
    origin: String,
    #[serde(default)]
    columns: Vec<IndexColumn>,
}

#[derive(Debug, Deserialize)]
struct IndexColumn {
    name: Option<String>,
}

const SNAPSHOT_JSON: &str = include_str!("schema_baseline.json");

pub fn migrate(conn: &Connection) -> Result<MigrationReport, String> {
    let snapshot: Snapshot =
        serde_json::from_str(SNAPSHOT_JSON).map_err(|e| format!("schema 快照无法解析: {e}"))?;
    let mut report = MigrationReport::default();
    conn.execute_batch("PRAGMA foreign_keys=OFF;")
        .map_err(|e| format!("关闭外键检查失败: {e}"))?;
    let outcome = (|| -> Result<(), String> {
        for table in &snapshot.tables {
            if table_exists(conn, &table.name)? {
                continue;
            }
            let sql = table
                .sql
                .replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1);
            conn.execute_batch(&sql)
                .map_err(|e| format!("建表 {} 失败: {e}", table.name))?;
            report.created_tables.push(table.name.clone());
        }
        for table in &snapshot.tables {
            if !table_exists(conn, &table.name)? {
                continue;
            }
            let existing = column_names(conn, &table.name)?;
            for column in &table.columns {
                if existing.contains(&column.name) {
                    continue;
                }
                let sql = format!(
                    "ALTER TABLE \"{}\" ADD COLUMN \"{}\" {}",
                    table.name, column.name, column.column_type
                );
                conn.execute_batch(&sql)
                    .map_err(|e| format!("补列 {}.{} 失败: {e}", table.name, column.name))?;
                report
                    .added_columns
                    .push(format!("{}.{}", table.name, column.name));
            }
        }
        for table in &snapshot.tables {
            for index in &table.indexes {
                if index.name.starts_with("sqlite_autoindex_")
                    || index.origin == "u"
                    || index.origin == "pk"
                {
                    continue;
                }
                if index.columns.is_empty()
                    || index.columns.iter().any(|column| column.name.is_none())
                {
                    continue;
                }
                if index_exists(conn, &index.name)? {
                    continue;
                }
                let columns = index
                    .columns
                    .iter()
                    .map(|column| format!("\"{}\"", column.name.as_ref().unwrap()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "CREATE {}INDEX IF NOT EXISTS \"{}\" ON \"{}\" ({})",
                    if index.unique { "UNIQUE " } else { "" },
                    index.name,
                    table.name,
                    columns
                );
                conn.execute_batch(&sql)
                    .map_err(|e| format!("建索引 {} 失败: {e}", index.name))?;
                report.added_indexes.push(index.name.clone());
            }
        }
        backfill_sort_order(conn)?;

        ensure_parameter_changes(conn)?;
        ensure_ai_tuning(conn)?;
        normalize_ai_tuning_interval(conn)?;
        drop_legacy_default_leverage(conn)?;
        drop_legacy_removed_params(conn)?;
        backfill_opened_signals(conn)?;
        Ok(())
    })();
    let _ = conn.execute_batch("PRAGMA foreign_keys=ON;");
    outcome?;
    Ok(report)
}

pub(crate) fn ensure_parameter_changes(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS parameter_changes(\
         id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, group_name TEXT NOT NULL, key TEXT NOT NULL, \
         old_value TEXT, new_value TEXT NOT NULL, reason TEXT NOT NULL, \
         decision_id INTEGER NOT NULL DEFAULT 0, mode TEXT NOT NULL, source TEXT NOT NULL); \
         CREATE INDEX IF NOT EXISTS ix_parameter_changes_ts ON parameter_changes(ts);",
    )
    .map_err(|e| format!("建表 parameter_changes 失败: {e}"))
}

pub(crate) fn ensure_ai_tuning(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ai_tuning_sets(\
         id INTEGER PRIMARY KEY, name TEXT NOT NULL, regime TEXT NOT NULL, timeframe TEXT NOT NULL, \
         version INTEGER NOT NULL, params TEXT NOT NULL, status TEXT NOT NULL, source TEXT NOT NULL, \
         evidence TEXT NOT NULL DEFAULT '{}', parent_id INTEGER, created_at INTEGER NOT NULL, activated_at INTEGER); \
         CREATE UNIQUE INDEX IF NOT EXISTS ix_ai_tuning_sets_active_regime ON ai_tuning_sets(regime) WHERE status='active'; \
         CREATE INDEX IF NOT EXISTS ix_ai_tuning_sets_regime_status ON ai_tuning_sets(regime,status); \
         CREATE TABLE IF NOT EXISTS ai_tuning_activations(\
         id INTEGER PRIMARY KEY, set_id INTEGER NOT NULL, regime TEXT NOT NULL, previous_set_id INTEGER, \
         ts INTEGER NOT NULL, trigger TEXT NOT NULL); \
         CREATE INDEX IF NOT EXISTS ix_ai_tuning_activations_ts ON ai_tuning_activations(ts); \
         CREATE INDEX IF NOT EXISTS ix_ai_tuning_activations_set_id ON ai_tuning_activations(set_id);",
    )
    .map_err(|e| format!("建表 ai_tuning_sets 失败: {e}"))?;
    if !column_exists(conn, "parameter_changes", "set_id")? {
        conn.execute_batch("ALTER TABLE parameter_changes ADD COLUMN set_id INTEGER;")
            .map_err(|e| format!("补列 parameter_changes.set_id 失败: {e}"))?;
    }
    if !column_exists(conn, "parameter_changes", "regime")? {
        conn.execute_batch("ALTER TABLE parameter_changes ADD COLUMN regime TEXT;")
            .map_err(|e| format!("补列 parameter_changes.regime 失败: {e}"))?;
    }
    if !column_exists(conn, "trades", "ai_tuning_set_id")? {
        conn.execute_batch("ALTER TABLE trades ADD COLUMN ai_tuning_set_id INTEGER;")
            .map_err(|e| format!("补列 trades.ai_tuning_set_id 失败: {e}"))?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS ix_parameter_changes_set_id ON parameter_changes(set_id); \
         CREATE INDEX IF NOT EXISTS ix_trades_ai_tuning_set_id ON trades(ai_tuning_set_id);",
    )
    .map_err(|e| format!("建索引 ai_tuning 关联列失败: {e}"))?;
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, String> {
    Ok(column_names(conn, table)?.contains(column))
}

/// 存量库的调参间隔 0 表示「每轮复核都可调参」，A 期解耦后语义变为独立任务间隔，
/// 必须归一为 86400；显式配置过的非 0 值保持不动。
pub(crate) fn normalize_ai_tuning_interval(conn: &Connection) -> Result<(), String> {
    conn.execute(
        "UPDATE app_settings SET value = json_set(value, '$.param_tuning_min_interval_sec', 86400) \
         WHERE key='ai' AND json_valid(value) \
         AND COALESCE(json_extract(value, '$.param_tuning_min_interval_sec'), 0) = 0",
        [],
    )
    .map_err(|e| format!("迁移 ai.param_tuning_min_interval_sec 失败: {e}"))?;
    Ok(())
}

/// 杠杆改由「AI 在全局上下限内给值」，老库里的 default_leverage 必须清掉：
/// 留着会被 group_value 合并回读路径，页面继续显示一个已不生效的旧值。
fn drop_legacy_default_leverage(conn: &Connection) -> Result<(), String> {
    conn.execute(
        "UPDATE app_settings SET value = json_remove(value, '$.default_leverage') \
         WHERE key = 'risk' AND json_valid(value) AND json_type(value, '$.default_leverage') IS NOT NULL",
        [],
    )
    .map_err(|e| format!("清理 default_leverage 失败: {e}"))?;
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .map_err(|e| format!("查询表 {name} 失败: {e}"))
}

fn index_exists(conn: &Connection, name: &str) -> Result<bool, String> {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .map_err(|e| format!("查询索引 {name} 失败: {e}"))
}

fn column_names(conn: &Connection, table: &str) -> Result<HashSet<String>, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .map_err(|e| format!("读取 {table} 列失败: {e}"))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("读取 {table} 列失败: {e}"))?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(|e| format!("读取 {table} 列失败: {e}"))?;
    Ok(names)
}

fn backfill_sort_order(conn: &Connection) -> Result<(), String> {
    if !table_exists(conn, "trading_symbols")? {
        return Ok(());
    }
    let existing = column_names(conn, "trading_symbols")?;
    if !existing.contains("sort_order") || !existing.contains("created_at") {
        return Ok(());
    }
    let rows: Vec<(String, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT symbol, COALESCE(sort_order, 0) FROM trading_symbols ORDER BY created_at ASC, symbol ASC",
            )
            .map_err(|e| format!("读取标的顺序失败: {e}"))?;
        let mapped = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| format!("读取标的顺序失败: {e}"))?;
        let collected = mapped
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("读取标的顺序失败: {e}"))?;
        collected
    };
    let mut next = rows.iter().map(|(_, order)| *order).max().unwrap_or(0);
    for (symbol, order) in rows {
        if order != 0 {
            continue;
        }
        next += 1;
        conn.execute(
            "UPDATE trading_symbols SET sort_order=?1 WHERE symbol=?2",
            params![next, symbol],
        )
        .map_err(|e| format!("回填 {symbol} 顺序失败: {e}"))?;
    }
    Ok(())
}

/// 已退役的参数必须从存量库清掉：否则参数页会把它们落到「其他参数」继续展示。
fn drop_legacy_removed_params(conn: &Connection) -> Result<(), String> {
    conn.execute(
        "UPDATE app_settings SET value = json_remove(value, '$.sl_atr_mult', '$.tp_atr_mult', \
         '$.min_risk_reward', '$.atr_stop_mult') \
         WHERE key='strategy' AND json_valid(value)",
        [],
    )
    .map_err(|e| format!("清理已退役 strategy 参数失败: {e}"))?;
    conn.execute(
        "UPDATE app_settings SET value = json_remove(value, '$.risk_per_trade_pct') \
         WHERE key='risk' AND json_valid(value)",
        [],
    )
    .map_err(|e| format!("清理已退役 risk 参数失败: {e}"))?;
    Ok(())
}

/// 历史修复：旧版本实盘成交后没有把信号审计从 pending 改成 opened。
///
/// 只回填能明确匹配到已成交开仓单的记录，真实挂单未成交的 pending 保持不动。
fn backfill_opened_signals(conn: &Connection) -> Result<(), String> {
    if !table_exists(conn, "signals")? || !table_exists(conn, "orders")? {
        return Ok(());
    }
    conn.execute(
        "UPDATE signals SET decision='opened' \
         WHERE decision='pending' AND mode='live' AND EXISTS ( \
           SELECT 1 FROM orders o \
           WHERE o.mode=signals.mode AND o.symbol=signals.symbol \
             AND o.purpose='entry' AND o.status='FILLED' \
             AND o.ts BETWEEN signals.ts - 60 AND signals.ts + 1800 \
         )",
        [],
    )
    .map_err(|e| format!("回填已开仓信号失败: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn migrate_on_empty_database_creates_all_tables() {
        let conn = Connection::open_in_memory().unwrap();
        let report = migrate(&conn).unwrap();
        assert_eq!(report.created_tables.len(), 24);
        assert_eq!(table_count(&conn), 27);
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let second = migrate(&conn).unwrap();
        assert!(second.created_tables.is_empty());
        assert!(second.added_columns.is_empty());
        assert!(second.added_indexes.is_empty());
        assert_eq!(table_count(&conn), 27);
    }

    #[test]
    fn migrate_adds_invalidation_price_to_positions() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE positions(id INTEGER PRIMARY KEY, symbol TEXT);")
            .unwrap();
        migrate(&conn).unwrap();
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info('positions')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(columns.contains(&"invalidation_price".to_string()));
    }

    #[test]
    fn normalize_ai_tuning_interval_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO app_settings(key,value,updated_at) \
             VALUES('ai','{\"allow_param_tuning\":true,\"param_tuning_min_interval_sec\":0}',0)",
            [],
        )
        .unwrap();
        normalize_ai_tuning_interval(&conn).unwrap();
        normalize_ai_tuning_interval(&conn).unwrap();
        let raw: String = conn
            .query_row("SELECT value FROM app_settings WHERE key='ai'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let ai: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(ai["param_tuning_min_interval_sec"], 86400);
    }

    #[test]
    fn migrate_normalize_ai_tuning_interval_keeps_nonzero_and_fills_missing() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let write_ai = |value: &str| {
            conn.execute(
                "INSERT OR REPLACE INTO app_settings(key,value,updated_at) VALUES('ai',?1,0)",
                [value],
            )
            .unwrap();
        };
        let read_ai = || -> serde_json::Value {
            let raw: String = conn
                .query_row("SELECT value FROM app_settings WHERE key='ai'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            serde_json::from_str(&raw).unwrap()
        };

        write_ai("{\"allow_param_tuning\":true,\"param_tuning_min_interval_sec\":0}");
        migrate(&conn).unwrap();
        let zero = read_ai();
        assert_eq!(zero["param_tuning_min_interval_sec"], 86400);
        assert_eq!(zero["allow_param_tuning"], true);

        write_ai("{\"allow_param_tuning\":true,\"param_tuning_min_interval_sec\":3600}");
        migrate(&conn).unwrap();
        assert_eq!(read_ai()["param_tuning_min_interval_sec"], 3600);

        write_ai("{\"allow_param_tuning\":false}");
        migrate(&conn).unwrap();
        let missing = read_ai();
        assert_eq!(missing["param_tuning_min_interval_sec"], 86400);
        assert_eq!(missing["allow_param_tuning"], false);
    }

    #[test]
    fn migrate_drops_legacy_default_leverage_but_keeps_the_bounds() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO app_settings(key,value,updated_at) \
             VALUES('risk','{\"min_leverage\":1,\"max_leverage\":100,\"default_leverage\":20}',0)",
            [],
        )
        .unwrap();
        migrate(&conn).unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='risk'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let risk: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            risk.get("default_leverage").is_none(),
            "旧默认杠杆必须被清掉"
        );
        assert_eq!(risk["min_leverage"], 1);
        assert_eq!(risk["max_leverage"], 100);
    }

    #[test]
    fn migrate_drops_retired_strategy_and_risk_params() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER); \
             INSERT INTO app_settings VALUES('strategy','{\"timeframe\":\"1h\",\"sl_atr_mult\":1.2,\"tp_atr_mult\":2.0,\"min_risk_reward\":2.0,\"atr_stop_mult\":1.3}',0); \
             INSERT INTO app_settings VALUES('risk','{\"margin_pct\":50,\"risk_per_trade_pct\":2.0}',0);",
        )
        .unwrap();
        drop_legacy_removed_params(&conn).unwrap();
        let strategy: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let risk: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='risk'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        for removed in [
            "sl_atr_mult",
            "tp_atr_mult",
            "min_risk_reward",
            "atr_stop_mult",
        ] {
            assert!(!strategy.contains(removed), "strategy 仍残留 {removed}");
        }
        assert!(
            !risk.contains("risk_per_trade_pct"),
            "risk 仍残留 risk_per_trade_pct"
        );
        assert!(strategy.contains("timeframe"));
        assert!(risk.contains("margin_pct"));
    }

    #[test]
    fn migrate_backfills_filled_live_entries_to_opened() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute_batch(
            "DELETE FROM signals; DELETE FROM orders; \
             INSERT INTO signals(ts,mode,symbol,direction,score,features,ai_action,ai_confidence,ai_reason,decision,reject_reason,repeats) \
             VALUES(1000,'live','BTCUSDT','long',0.5,'{}','open_long',0.6,'','pending','',1); \
             INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,client_order_id,exchange_order_id,reduce_only,mode,latency_ms) \
             VALUES(1005,'BTCUSDT','BUY','MARKET',1,100,'FILLED','entry','entry-1','',0,'live',0); \
             INSERT INTO signals(ts,mode,symbol,direction,score,features,ai_action,ai_confidence,ai_reason,decision,reject_reason,repeats) \
             VALUES(2000,'live','ETHUSDT','short',0.4,'{}','open_short',0.5,'','pending','',1); \
             INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,client_order_id,exchange_order_id,reduce_only,mode,latency_ms) \
             VALUES(2005,'ETHUSDT','SELL','MARKET',1,100,'NEW','entry','entry-2','',0,'live',0);",
        )
        .unwrap();
        backfill_opened_signals(&conn).unwrap();
        let btc: String = conn
            .query_row(
                "SELECT decision FROM signals WHERE symbol='BTCUSDT'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let eth: String = conn
            .query_row(
                "SELECT decision FROM signals WHERE symbol='ETHUSDT'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(btc, "opened");
        assert_eq!(eth, "pending", "未成交的挂单不能误标为已开仓");
    }

    #[test]
    fn migrate_backfills_legacy_trading_symbols_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE trading_symbols (symbol VARCHAR(32) NOT NULL PRIMARY KEY);",
        )
        .unwrap();
        let report = migrate(&conn).unwrap();
        let columns: Vec<String> = {
            let mut stmt = conn
                .prepare("PRAGMA table_info('trading_symbols')")
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert!(columns.contains(&"name".to_string()));
        assert!(columns.contains(&"sort_order".to_string()));
        assert!(report
            .added_columns
            .iter()
            .any(|item| item == "trading_symbols.name"));
        let index_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='ix_trading_symbols_sort_order'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_exists, 1);
    }

    #[test]
    fn schema_parity_with_baseline_snapshot() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let snapshot: Snapshot = serde_json::from_str(SNAPSHOT_JSON).unwrap();
        for table in &snapshot.tables {
            let actual: Vec<(String, String, bool, Option<String>, bool)> = {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA table_info(\"{}\")", table.name))
                    .unwrap();
                stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)? != 0,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, i64>(5)? != 0,
                    ))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
            };
            assert_eq!(
                actual.len(),
                table.columns.len(),
                "{} 列数不一致",
                table.name
            );
            for (column, spec) in actual.iter().zip(table.columns.iter()) {
                assert_eq!(column.0, spec.name, "{} 列名不一致", table.name);
                assert_eq!(
                    column.1, spec.column_type,
                    "{}.{} 类型不一致",
                    table.name, spec.name
                );
                assert_eq!(
                    column.2, spec.notnull,
                    "{}.{} NOT NULL 不一致",
                    table.name, spec.name
                );
                assert_eq!(
                    column.3, spec.default_value,
                    "{}.{} 默认值不一致",
                    table.name, spec.name
                );
                assert_eq!(column.4, spec.pk, "{}.{} 主键不一致", table.name, spec.name);
            }
            let expected: std::collections::HashSet<String> = table
                .indexes
                .iter()
                .filter(|index| {
                    !index.name.starts_with("sqlite_autoindex_")
                        && index.origin != "u"
                        && index.origin != "pk"
                })
                .map(|index| index.name.clone())
                .collect();
            let actual_indexes: std::collections::HashSet<String> = {
                let mut stmt = conn
                    .prepare(&format!("PRAGMA index_list(\"{}\")", table.name))
                    .unwrap();
                stmt.query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .collect::<Result<Vec<String>, _>>()
                    .unwrap()
            }
            .into_iter()
            .filter(|name| !name.starts_with("sqlite_autoindex_"))
            .collect();
            assert_eq!(actual_indexes, expected, "{} 索引集合不一致", table.name);
        }
    }
}
