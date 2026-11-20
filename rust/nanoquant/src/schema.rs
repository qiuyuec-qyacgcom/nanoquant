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
    #[serde(default)]
    notnull: bool,
    #[serde(default, rename = "default")]
    default_value: Option<String>,
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
        Ok(())
    })();
    let _ = conn.execute_batch("PRAGMA foreign_keys=ON;");
    outcome?;
    Ok(report)
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
        assert_eq!(report.created_tables.len(), 23);
        assert_eq!(table_count(&conn), 23);
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let second = migrate(&conn).unwrap();
        assert!(second.created_tables.is_empty());
        assert!(second.added_columns.is_empty());
        assert!(second.added_indexes.is_empty());
        assert_eq!(table_count(&conn), 23);
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
