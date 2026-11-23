use rusqlite::{params, Connection};
use serde_json::{json, Value};

const BOOTSTRAP_TIMEFRAME: &str = "1h";

/// A 期迁移种子化：只建参数集记录，绝不改写 `app_settings.strategy`（C 期才物化）。
/// 幂等：`parameter_sets` 已有任何数据即跳过；`strategy` 缺失时直接返回，允许单独调用。
pub(crate) fn seed_bootstrap(conn: &Connection) -> Result<(), String> {
    let exists: i64 = conn
        .query_row("SELECT COUNT(*) FROM parameter_sets", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if exists > 0 {
        return Ok(());
    }
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='strategy'",
            [],
            |r| r.get(0),
        )
        .ok();
    let Some(raw) = raw else {
        return Ok(());
    };
    let current: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let timeframe = current["timeframe"].as_str().unwrap_or("4h");
    if timeframe != BOOTSTRAP_TIMEFRAME {
        let retired = current.clone();
        insert_set(
            conn,
            "current-snapshot",
            "range",
            timeframe,
            &retired,
            "retired",
            "manual",
            "{\"kind\":\"historical\"}",
            None,
        )?;
        return Ok(());
    }
    let range = with_overrides(
        &current,
        &[
            ("atr_pct_min", json!(0.6)),
            ("atr_pct_max", json!(10.0)),
            ("volume_ratio_min", json!(0.8)),
            ("entry_threshold", json!(0.45)),
        ],
    );
    let chaos = with_overrides(
        &current,
        &[
            ("atr_pct_min", json!(1.0)),
            ("atr_pct_max", json!(8.0)),
            ("volume_ratio_min", json!(1.0)),
            ("entry_threshold", json!(0.5)),
        ],
    );
    let live = with_overrides(&current, &[]);
    let range_id = insert_set(
        conn,
        "range-bootstrap",
        "range",
        timeframe,
        &range,
        "active",
        "bootstrap",
        "{\"kind\":\"bootstrap\"}",
        None,
    )?;
    insert_activation(conn, range_id, "range", None, "bootstrap")?;
    let chaos_id = insert_set(
        conn,
        "chaos-bootstrap",
        "chaos",
        timeframe,
        &chaos,
        "active",
        "bootstrap",
        "{\"kind\":\"bootstrap\"}",
        None,
    )?;
    insert_activation(conn, chaos_id, "chaos", None, "bootstrap")?;
    insert_set(
        conn,
        "chaos-live-2026-11-23",
        "chaos",
        timeframe,
        &live,
        "retired",
        "manual",
        "{\"kind\":\"historical\"}",
        None,
    )?;
    Ok(())
}

fn with_overrides(base: &Value, overrides: &[(&str, Value)]) -> Value {
    let mut params = base.clone();
    if let Some(map) = params.as_object_mut() {
        for (key, value) in overrides {
            map.insert((*key).to_string(), value.clone());
        }
    }
    params
}

#[allow(clippy::too_many_arguments)]
fn insert_set(
    conn: &Connection,
    name: &str,
    regime: &str,
    timeframe: &str,
    params_value: &Value,
    status: &str,
    source: &str,
    evidence: &str,
    parent_id: Option<i64>,
) -> Result<i64, String> {
    let now = crate::now();
    let activated_at = if status == "active" { Some(now) } else { None };
    let raw = serde_json::to_string(params_value).map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO parameter_sets(name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at) \
         VALUES(?1,?2,?3,1,?4,?5,?6,?7,?8,?9,?10)",
        params![
            name,
            regime,
            timeframe,
            raw,
            status,
            source,
            evidence,
            parent_id,
            now,
            activated_at
        ],
    )
    .map_err(|e| format!("写入参数集 {name} 失败: {e}"))?;
    Ok(conn.last_insert_rowid())
}

fn insert_activation(
    conn: &Connection,
    set_id: i64,
    regime: &str,
    previous_set_id: Option<i64>,
    trigger: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO parameter_set_activations(set_id,regime,previous_set_id,ts,trigger) \
         VALUES(?1,?2,?3,?4,?5)",
        params![set_id, regime, previous_set_id, crate::now(), trigger],
    )
    .map_err(|e| format!("写入参数集激活记录失败: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn conn_with_strategy(timeframe: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('strategy',?1,0)",
            [serde_json::json!({
                "timeframe": timeframe,
                "atr_pct_min": 1.5,
                "atr_pct_max": 8,
                "volume_ratio_min": 1.1,
                "entry_threshold": 0.55
            })
            .to_string()],
        )
        .unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn bootstrap_seeds_range_and_chaos_only_for_1h_and_keeps_strategy() {
        let conn = conn_with_strategy("1h");
        let before: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        seed_bootstrap(&conn).unwrap();
        let after: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, after);
        let rows: Vec<(String, String, String, String)> = conn
            .prepare("SELECT name,regime,timeframe,status FROM parameter_sets ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .flatten()
            .collect();
        assert!(rows.contains(&(
            "range-bootstrap".into(),
            "range".into(),
            "1h".into(),
            "active".into()
        )));
        assert!(rows.contains(&(
            "chaos-bootstrap".into(),
            "chaos".into(),
            "1h".into(),
            "active".into()
        )));
        assert!(rows.contains(&(
            "chaos-live-2026-11-23".into(),
            "chaos".into(),
            "1h".into(),
            "retired".into()
        )));
        let activations: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM parameter_set_activations WHERE trigger='bootstrap'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(activations, 2);
    }

    #[test]
    fn bootstrap_is_idempotent() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        seed_bootstrap(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn non_1h_seeds_only_retired_snapshot() {
        let conn = conn_with_strategy("4h");
        seed_bootstrap(&conn).unwrap();
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT name,status FROM parameter_sets")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(
            rows,
            vec![("current-snapshot".to_string(), "retired".to_string())]
        );
    }

    #[test]
    fn migrate_on_empty_settings_does_not_seed_or_fail() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);",
        )
        .unwrap();
        crate::schema::migrate(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert!(seed_bootstrap(&conn).is_ok());
    }
}
