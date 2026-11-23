use rusqlite::{params, Connection};
use serde_json::{json, Value};

const BOOTSTRAP_TIMEFRAME: &str = "1h";
const CURRENT_SNAPSHOT_EVIDENCE: &str =
    "{\"kind\":\"manual\",\"historical\":true,\"snapshot\":\"partial\"}";
const INCIDENT_EVIDENCE: &str = "{\"kind\":\"manual\",\"historical\":true,\"snapshot\":\"partial\",\"incident_date\":\"2026-11-23\",\"known_keys\":[\"atr_pct_min\",\"atr_pct_max\",\"volume_ratio_min\",\"entry_threshold\"]}";
const AUTO_SWITCH_DISABLED: &str = "strategy.timeframe 不在自动切换白名单，已禁用自动切换";

/// A 期迁移种子化：只建参数集记录，绝不改写 `app_settings.strategy`（C 期才物化）。
/// 幂等：`parameter_sets` 已有任何数据即跳过；`strategy` 缺失或损坏时直接返回。
/// 全部写入放在一个事务里：中途失败整体回滚，避免留下半套数据导致下次跳过。
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
    let Ok(current) = serde_json::from_str::<Value>(&raw) else {
        // 配置损坏时跳过种子化：审计种子不值得阻断整次启动。
        return Ok(());
    };
    let now = crate::now();
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("开启参数集种子化事务失败: {e}"))?;
    if let Err(error) = seed_in_transaction(&tx, &current, now) {
        let _ = tx.rollback();
        return Err(error);
    }
    tx.commit()
        .map_err(|e| format!("提交参数集种子化事务失败: {e}"))
}

fn seed_in_transaction(conn: &Connection, current: &Value, now: i64) -> Result<(), String> {
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
            CURRENT_SNAPSHOT_EVIDENCE,
            None,
            now,
        )?;
        disable_auto_switch(conn, now)?;
        return Ok(());
    }
    let range = with_overrides(
        current,
        &[
            ("atr_pct_min", json!(0.6)),
            ("atr_pct_max", json!(10.0)),
            ("volume_ratio_min", json!(0.8)),
            ("entry_threshold", json!(0.45)),
        ],
    );
    let chaos = with_overrides(
        current,
        &[
            ("atr_pct_min", json!(1.0)),
            ("atr_pct_max", json!(8.0)),
            ("volume_ratio_min", json!(1.0)),
            ("entry_threshold", json!(0.5)),
        ],
    );
    // 四个已知事故值覆盖；其余键来自部署快照，审计标记 partial。
    let live = with_overrides(
        current,
        &[
            ("atr_pct_min", json!(1.5)),
            ("atr_pct_max", json!(8.0)),
            ("volume_ratio_min", json!(1.1)),
            ("entry_threshold", json!(0.55)),
        ],
    );
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
        now,
    )?;
    insert_activation(conn, range_id, "range", None, "bootstrap", now)?;
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
        now,
    )?;
    insert_activation(conn, chaos_id, "chaos", None, "bootstrap", now)?;
    insert_set(
        conn,
        "chaos-live-2026-11-23",
        "chaos",
        timeframe,
        &live,
        "retired",
        "manual",
        INCIDENT_EVIDENCE,
        None,
        now,
    )?;
    Ok(())
}

/// 非白名单 timeframe 下按规格关闭自动切换，并留下一条告警事件。
fn disable_auto_switch(conn: &Connection, now: i64) -> Result<(), String> {
    conn.execute(
        "UPDATE app_settings \
         SET value = json_patch(value, '{\"auto_apply\":false,\"auto_apply_timeframes\":\"\"}') \
         WHERE key='parameter_set'",
        [],
    )
    .map_err(|e| format!("禁用参数集自动切换失败: {e}"))?;
    conn.execute(
        "INSERT INTO engine_events(ts,level,category,message,payload) \
         VALUES(?1,'warn','parameter_set',?2,?3)",
        params![now, AUTO_SWITCH_DISABLED, "{}"],
    )
    .map_err(|e| format!("写入参数集告警事件失败: {e}"))?;
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
    now: i64,
) -> Result<i64, String> {
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
    now: i64,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO parameter_set_activations(set_id,regime,previous_set_id,ts,trigger) \
         VALUES(?1,?2,?3,?4,?5)",
        params![set_id, regime, previous_set_id, now, trigger],
    )
    .map_err(|e| format!("写入参数集激活记录失败: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    struct SetRow {
        name: String,
        regime: String,
        timeframe: String,
        status: String,
        source: String,
        evidence: Value,
        parent_id: Option<i64>,
        params: Value,
    }

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
        // 非 1h 分支需要既有 parameter_set 配置来验证白名单被清空。
        conn.execute(
            "INSERT INTO app_settings VALUES('parameter_set',?1,0)",
            [serde_json::json!({
                "enabled": true,
                "auto_apply": true,
                "auto_apply_timeframes": "1h"
            })
            .to_string()],
        )
        .unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    fn all_sets(conn: &Connection) -> Vec<SetRow> {
        conn.prepare(
            "SELECT name,regime,timeframe,status,source,evidence,parent_id,params \
             FROM parameter_sets ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            let evidence: String = row.get(5)?;
            let params: String = row.get(7)?;
            Ok(SetRow {
                name: row.get(0)?,
                regime: row.get(1)?,
                timeframe: row.get(2)?,
                status: row.get(3)?,
                source: row.get(4)?,
                evidence: serde_json::from_str(&evidence).unwrap(),
                parent_id: row.get(6)?,
                params: serde_json::from_str(&params).unwrap(),
            })
        })
        .unwrap()
        .flatten()
        .collect()
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

        let sets = all_sets(&conn);
        assert_eq!(sets.len(), 3);

        let range = &sets[0];
        assert_eq!(range.name, "range-bootstrap");
        assert_eq!(range.regime, "range");
        assert_eq!(range.timeframe, "1h");
        assert_eq!(range.params["timeframe"], range.timeframe);
        assert_eq!(range.status, "active");
        assert_eq!(range.source, "bootstrap");
        assert_eq!(range.evidence, json!({ "kind": "bootstrap" }));
        assert!(range.parent_id.is_none(), "种子集不应有父集");
        assert_eq!(range.params["atr_pct_min"], 0.6);
        assert_eq!(range.params["atr_pct_max"], 10.0);
        assert_eq!(range.params["volume_ratio_min"], 0.8);
        assert_eq!(range.params["entry_threshold"], 0.45);

        let chaos = &sets[1];
        assert_eq!(chaos.name, "chaos-bootstrap");
        assert_eq!(chaos.regime, "chaos");
        assert_eq!(chaos.timeframe, "1h");
        assert_eq!(chaos.params["timeframe"], chaos.timeframe);
        assert_eq!(chaos.status, "active");
        assert_eq!(chaos.source, "bootstrap");
        assert_eq!(chaos.evidence, json!({ "kind": "bootstrap" }));
        assert!(chaos.parent_id.is_none(), "种子集不应有父集");
        assert_eq!(chaos.params["atr_pct_min"], 1.0);
        assert_eq!(chaos.params["atr_pct_max"], 8.0);
        assert_eq!(chaos.params["volume_ratio_min"], 1.0);
        assert_eq!(chaos.params["entry_threshold"], 0.5);

        let live = &sets[2];
        assert_eq!(live.name, "chaos-live-2026-11-23");
        assert_eq!(live.regime, "chaos");
        assert_eq!(live.timeframe, "1h");
        assert_eq!(live.params["timeframe"], live.timeframe);
        assert_eq!(live.status, "retired");
        assert_eq!(live.source, "manual");
        assert_eq!(live.evidence["kind"], "manual");
        assert_eq!(live.evidence["historical"], true);
        assert_eq!(live.evidence["snapshot"], "partial");
        assert_eq!(live.evidence["incident_date"], "2026-11-23");
        assert_eq!(
            live.evidence["known_keys"],
            json!([
                "atr_pct_min",
                "atr_pct_max",
                "volume_ratio_min",
                "entry_threshold"
            ])
        );
        assert!(live.parent_id.is_none());
        assert_eq!(live.params["atr_pct_min"], 1.5);
        assert_eq!(live.params["atr_pct_max"], 8.0);
        assert_eq!(live.params["volume_ratio_min"], 1.1);
        assert_eq!(live.params["entry_threshold"], 0.55);

        let activations: Vec<(i64, String, Option<i64>, String, String)> = conn
            .prepare(
                "SELECT a.set_id, a.regime, a.previous_set_id, p.regime, p.status \
                 FROM parameter_set_activations a JOIN parameter_sets p ON p.id = a.set_id \
                 WHERE a.trigger='bootstrap' ORDER BY a.id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(activations.len(), 2);
        for (set_id, regime, previous_set_id, set_regime, status) in &activations {
            assert!(*set_id > 0);
            assert!(previous_set_id.is_none(), "首次种子化不应有上一套参数集");
            assert_eq!(regime, set_regime, "激活记录 regime 必须与参数集一致");
            assert_eq!(status, "active");
        }
        assert_eq!(
            activations
                .iter()
                .map(|(_, regime, _, _, _)| regime.as_str())
                .collect::<Vec<_>>(),
            vec!["range", "chaos"]
        );
        let total_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_set_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(total_activations, 2, "retired 审计集不应产生激活记录");
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
        let activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_set_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(activations, 2);
    }

    #[test]
    fn seed_bootstrap_rolls_back_all_writes_on_failure() {
        let conn = conn_with_strategy("1h");
        conn.execute_batch(
            "CREATE TRIGGER seed_boom BEFORE INSERT ON parameter_sets \
             WHEN NEW.name='chaos-bootstrap' BEGIN SELECT RAISE(ABORT,'boom'); END;",
        )
        .unwrap();
        assert!(seed_bootstrap(&conn).is_err());
        let sets: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sets, 0, "种子化失败必须整体回滚");
        let activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_set_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(activations, 0, "激活记录也必须回滚");
    }

    #[test]
    fn non_1h_seeds_only_retired_snapshot_and_disables_auto_switch() {
        let conn = conn_with_strategy("4h");
        seed_bootstrap(&conn).unwrap();
        let sets = all_sets(&conn);
        assert_eq!(sets.len(), 1);
        let snapshot = &sets[0];
        assert_eq!(snapshot.name, "current-snapshot");
        assert_eq!(snapshot.status, "retired");
        assert_eq!(snapshot.source, "manual");
        assert_eq!(snapshot.evidence["kind"], "manual");
        assert_eq!(snapshot.evidence["historical"], true);
        assert_eq!(snapshot.evidence["snapshot"], "partial");
        assert!(snapshot.parent_id.is_none());
        assert_eq!(snapshot.params["timeframe"], snapshot.timeframe);

        let raw: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='parameter_set'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let config: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(config["auto_apply"], false);
        assert_eq!(config["auto_apply_timeframes"], "");

        let activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_set_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(activations, 0);

        let warnings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events \
                 WHERE level='warn' AND category='parameter_set' AND message=?1",
                [AUTO_SWITCH_DISABLED],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(warnings, 1);
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

    #[test]
    fn corrupt_strategy_json_is_skipped_without_failing_bootstrap() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings VALUES('strategy','{not valid json',0)",
            [],
        )
        .unwrap();
        crate::schema::migrate(&conn).unwrap();
        assert!(seed_bootstrap(&conn).is_ok());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
