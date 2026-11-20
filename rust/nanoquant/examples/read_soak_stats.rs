//! 临时工具：读取 soak 实例的库统计，输出 JSON。
use rusqlite::Connection;
use serde_json::json;

fn scalar(conn: &Connection, sql: &str) -> serde_json::Value {
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .map(|n| json!(n))
        .unwrap_or(serde_json::Value::Null)
}

fn main() {
    let path = std::env::args().nth(1).expect("用法: read_soak_stats <db>");
    let conn = Connection::open(&path).expect("open db");
    let last_cycle_ts: Option<String> = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='last_cycle_ts'",
            [],
            |r| r.get(0),
        )
        .ok();
    let last_cycle_ms: Option<String> = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='last_cycle_ms'",
            [],
            |r| r.get(0),
        )
        .ok();
    println!(
        "{}",
        json!({
            "last_cycle_ts": last_cycle_ts,
            "last_cycle_ms": last_cycle_ms,
            "error_events": scalar(&conn, "SELECT COUNT(*) FROM engine_events WHERE level='error'"),
            "warn_events": scalar(&conn, "SELECT COUNT(*) FROM engine_events WHERE level='warn'"),
            "cycles_recorded": scalar(&conn, "SELECT COUNT(*) FROM engine_events WHERE category='cycle'"),
            "open_positions": scalar(&conn, "SELECT COUNT(*) FROM positions WHERE status='open'"),
            "orders": scalar(&conn, "SELECT COUNT(*) FROM orders"),
            "equity_points": scalar(&conn, "SELECT COUNT(*) FROM equity_points")
        })
    );
}
