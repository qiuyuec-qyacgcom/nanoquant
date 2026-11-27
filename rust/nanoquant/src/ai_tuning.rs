use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

const BOOTSTRAP_TIMEFRAME: &str = "1h";
const CURRENT_SNAPSHOT_EVIDENCE: &str =
    "{\"kind\":\"manual\",\"reason\":\"non_bootstrap_timeframe\",\"snapshot\":\"full\"}";
const INCIDENT_EVIDENCE: &str = "{\"kind\":\"manual\",\"historical\":true,\"snapshot\":\"partial\",\"incident_date\":\"2026-11-23\",\"known_keys\":[\"atr_pct_min\",\"atr_pct_max\",\"volume_ratio_min\",\"entry_threshold\"]}";

/// A 期迁移种子化：只建AI 调参记录，绝不改写 `app_settings.strategy`（C 期才物化）。
/// 幂等：`ai_tuning` 已有任何数据即跳过；`strategy` 缺失或损坏时直接返回。
/// 全部写入放在一个事务里：中途失败整体回滚，避免留下半套数据导致下次跳过。
pub(crate) fn seed_bootstrap(conn: &Connection) -> Result<(), String> {
    let exists: i64 = conn
        .query_row("SELECT COUNT(*) FROM ai_tuning_sets", [], |r| r.get(0))
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
        .map_err(|e| format!("开启AI 调参种子化事务失败: {e}"))?;
    if let Err(error) = seed_in_transaction(&tx, &current, now) {
        let _ = tx.rollback();
        return Err(error);
    }
    tx.commit()
        .map_err(|e| format!("提交AI 调参种子化事务失败: {e}"))
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
        "INSERT INTO ai_tuning_sets(name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at) \
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
    .map_err(|e| format!("写入AI 调参 {name} 失败: {e}"))?;
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
        "INSERT INTO ai_tuning_activations(set_id,regime,previous_set_id,ts,trigger) \
         VALUES(?1,?2,?3,?4,?5)",
        params![set_id, regime, previous_set_id, now, trigger],
    )
    .map_err(|e| format!("写入AI 调参激活记录失败: {e}"))?;
    Ok(())
}

const MAX_CANDIDATE_KEYS: usize = 3;
const MAX_RELATIVE_CHANGE: f64 = 0.25;
const SET_COLUMNS: &str = "id,name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AiTuningSet {
    pub id: i64,
    pub name: String,
    pub regime: String,
    pub timeframe: String,
    pub version: i64,
    pub params: Value,
    pub status: String,
    pub source: String,
    pub evidence: Value,
    pub parent_id: Option<i64>,
    pub created_at: i64,
    pub activated_at: Option<i64>,
}

/// `params` / `evidence` 是历史 JSON 快照；解析失败时降级为 Null，读取不中断。
pub(crate) fn row_to_set(row: &rusqlite::Row<'_>) -> rusqlite::Result<AiTuningSet> {
    let params_raw: String = row.get(5)?;
    let evidence_raw: String = row.get(8)?;
    Ok(AiTuningSet {
        id: row.get(0)?,
        name: row.get(1)?,
        regime: row.get(2)?,
        timeframe: row.get(3)?,
        version: row.get(4)?,
        params: serde_json::from_str(&params_raw).unwrap_or(Value::Null),
        status: row.get(6)?,
        source: row.get(7)?,
        evidence: serde_json::from_str(&evidence_raw).unwrap_or(Value::Null),
        parent_id: row.get(9)?,
        created_at: row.get(10)?,
        activated_at: row.get(11)?,
    })
}

/// 读取某 regime 当前唯一的 active AI 调参。
pub(crate) fn active_for_regime(
    conn: &Connection,
    regime: &str,
) -> Result<Option<AiTuningSet>, String> {
    let query = format!(
        "SELECT {SET_COLUMNS} FROM ai_tuning_sets \
         WHERE regime=?1 AND status='active' ORDER BY id DESC LIMIT 1"
    );
    match conn.query_row(&query, [regime], row_to_set) {
        Ok(set) => Ok(Some(set)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(format!("读取 {regime} active AI 调参失败: {error}")),
    }
}

/// 按 id 读取AI 调参，候选父集校验也复用它。
pub(crate) fn load_set(conn: &Connection, id: i64) -> Result<AiTuningSet, String> {
    conn.query_row(
        &format!("SELECT {SET_COLUMNS} FROM ai_tuning_sets WHERE id=?1"),
        [id],
        row_to_set,
    )
    .map_err(|e| format!("读取AI 调参 {id} 失败: {e}"))
}

const ACTIVATABLE_STATUSES: &[&str] = &["approved", "active", "bootstrap", "manual"];

/// 激活指定AI 调参：切换 active 状态、物化 strategy、记录逐键变更与激活审计。
///
/// 使用 SAVEPOINT 包裹，以便既能独立调用，也能嵌入 runtime 周期事务。
pub(crate) fn activate(
    conn: &Connection,
    set_id: i64,
    trigger: &str,
    mode: &str,
) -> Result<(), String> {
    let set = load_set(conn, set_id)?;
    conn.execute_batch("SAVEPOINT ai_tuning_activate")
        .map_err(|e| format!("开启AI 调参激活事务失败: {e}"))?;
    match activate_in_savepoint(conn, &set, trigger, mode) {
        Ok(()) => conn
            .execute_batch("RELEASE ai_tuning_activate")
            .map_err(|e| format!("提交AI 调参激活事务失败: {e}")),
        Err(error) => {
            let _ =
                conn.execute_batch("ROLLBACK TO ai_tuning_activate; RELEASE ai_tuning_activate");
            Err(error)
        }
    }
}

fn activate_in_savepoint(
    conn: &Connection,
    set: &AiTuningSet,
    trigger: &str,
    mode: &str,
) -> Result<(), String> {
    if !ACTIVATABLE_STATUSES.contains(&set.status.as_str()) {
        return Err(format!("AI 调参 {} 状态 {} 不允许激活", set.id, set.status));
    }
    if !set.params.is_object() {
        return Err(format!("AI 调参 {} params 必须是对象快照", set.id));
    }
    let before = crate::settings_api::group_value_public(conn, "strategy");
    let current_timeframe = before
        .get("timeframe")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if set.timeframe != current_timeframe {
        return Err(format!(
            "AI 调参 {} timeframe {} 与当前策略 {current_timeframe} 不一致",
            set.id, set.timeframe
        ));
    }
    let after = crate::settings_api::normalized_group_value("strategy", &set.params);
    crate::settings_api::validate_group("strategy", &after)?;
    crate::settings_api::validate_indicator_windows(
        &after,
        &crate::settings_api::group_value_public(conn, "data"),
    )?;

    if set.status == "active" {
        if after == before {
            // 已 active 且 strategy 与该集 params 一致：真 no-op，不写任何审计。
            return Ok(());
        }
        // 已 active 但尚未物化（如 bootstrap 首次启用）：只物化，不退役自身、
        // 不改 status，避免违反每 regime 唯一 active 约束。
        return materialize(conn, set, &before, &after, None, trigger, mode);
    }

    let previous_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM ai_tuning_sets \
             WHERE regime=?1 AND status='active' AND id<>?2 ORDER BY id DESC LIMIT 1",
            params![set.regime, set.id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| format!("读取 {} active AI 调参失败: {e}", set.regime))?;

    conn.execute(
        "UPDATE ai_tuning_sets SET status='retired' \
         WHERE regime=?1 AND status='active' AND id<>?2",
        params![set.regime, set.id],
    )
    .map_err(|e| format!("退役旧AI 调参失败: {e}"))?;

    materialize(conn, set, &before, &after, previous_id, trigger, mode)
}

#[allow(clippy::too_many_arguments)]
fn materialize(
    conn: &Connection,
    set: &AiTuningSet,
    before: &Value,
    after: &Value,
    previous_id: Option<i64>,
    trigger: &str,
    mode: &str,
) -> Result<(), String> {
    let now = crate::now();
    conn.execute(
        "UPDATE ai_tuning_sets SET status='active', activated_at=?1 WHERE id=?2",
        params![now, set.id],
    )
    .map_err(|e| format!("激活AI 调参 {} 失败: {e}", set.id))?;
    conn.execute(
        "INSERT INTO app_settings(key,value,updated_at) VALUES('strategy',?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![
            serde_json::to_string(after).map_err(|e| e.to_string())?,
            now
        ],
    )
    .map_err(|e| format!("物化 strategy 失败: {e}"))?;
    crate::settings_api::record_set_changes(
        conn,
        before,
        after,
        &set.regime,
        set.id,
        mode,
        "ai_tuning",
    )?;
    insert_activation(conn, set.id, &set.regime, previous_id, trigger, now)?;
    record_event(
        conn,
        "info",
        &format!("AI 调参 {} 已激活（{}）", set.name, trigger),
        json!({
            "set_id": set.id,
            "regime": set.regime,
            "mode": mode,
            "trigger": trigger,
            "previous_set_id": previous_id,
        }),
        now,
    )
}

/// 按当前 regime 查 active 集；trend 无集时回退 range，其余 regime 缺失返回 None。
pub(crate) fn active_set_for_regime(
    conn: &Connection,
    regime: &str,
) -> Result<Option<AiTuningSet>, String> {
    match active_for_regime(conn, regime)? {
        Some(set) => Ok(Some(set)),
        None if regime == "trend" => active_for_regime(conn, "range"),
        None => Ok(None),
    }
}

/// 按当前 regime 查 active 集并激活；trend 无集时回退 range，其余缺失直接报错。
pub(crate) fn activate_for_regime(
    conn: &Connection,
    regime: &str,
    trigger: &str,
    mode: &str,
) -> Result<(), String> {
    let Some(set) = active_set_for_regime(conn, regime)? else {
        return Err(format!("{regime} 没有可用的 active AI 调参"));
    };
    activate(conn, set.id, trigger, mode)
}

const DEFAULT_NO_CANDIDATE_ROLLBACK_SEC: i64 = 1_800;
const GUARD_EVENT_THROTTLE_SEC: i64 = 3_600;

/// 每次 regime/manual 激活后初始化守卫；`last_candidate_at=started_at=now`。
///
/// 新激活必须清除 `rollback_done`，否则新集第一轮零候选就会永久停摆。
pub(crate) fn set_guard(
    conn: &Connection,
    regime: &str,
    set_id: i64,
    now: i64,
) -> Result<(), String> {
    write_guard(conn, regime, &guard_value(set_id, now, false))
}

fn guard_value(set_id: i64, now: i64, rollback_done: bool) -> Value {
    json!({
        "set_id": set_id,
        "last_candidate_at": now,
        "started_at": now,
        "rollback_done": rollback_done,
    })
}

/// 每轮实跑后推进守卫；触发无候选回滚时返回 true。
///
/// `skipped` 轮次只暂停计时；`has_candidate` 重置计时并清除回滚标记。
/// `rollback_done=true` 表示该 active 集已自动回滚过一次：再零候选只告警不动作，
/// 避免每 30 分钟在两套AI 调参之间来回抖动。
pub(crate) fn update_guard(
    conn: &Connection,
    regime: &str,
    set_id: i64,
    has_candidate: bool,
    skipped: bool,
    now: i64,
) -> Result<bool, String> {
    let key = format!("ai_tuning.guard.{regime}");
    let mut guard = engine_state(conn, &key)
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(|value| value.get("set_id").and_then(Value::as_i64) == Some(set_id))
        .unwrap_or_else(|| guard_value(set_id, now, false));
    let rollback_done = guard
        .get("rollback_done")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if skipped {
        guard["last_candidate_at"] = json!(now);
        write_guard(conn, regime, &guard)?;
        record_guard_event(
            conn,
            "warn",
            &format!("ai_tuning.guard_paused_ts.{regime}"),
            &format!("ai_tuning.guard_paused: {regime} 本轮未实跑，暂停无候选计时"),
            json!({
                "regime": regime,
                "set_id": set_id,
                "mode": engine_mode(conn),
                "reason": "skipped",
            }),
            now,
        )?;
        return Ok(false);
    }
    if has_candidate {
        guard["last_candidate_at"] = json!(now);
        guard["rollback_done"] = json!(false);
        write_guard(conn, regime, &guard)?;
        return Ok(false);
    }
    if rollback_done {
        record_guard_event(
            conn,
            "warn",
            &format!("ai_tuning.guard_rollback_hold_ts.{regime}"),
            &format!("ai_tuning.guard_rollback_hold: {regime} 已回滚过，暂停无候选回滚"),
            json!({
                "regime": regime,
                "set_id": set_id,
                "mode": engine_mode(conn),
                "reason": "no_candidate",
            }),
            now,
        )?;
        return Ok(false);
    }
    let last_candidate_at = guard["last_candidate_at"].as_i64().unwrap_or(now);
    let threshold = crate::settings_api::group_value_public(conn, "ai_tuning")
        .get("no_candidate_rollback_sec")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_NO_CANDIDATE_ROLLBACK_SEC)
        .max(0);
    if now - last_candidate_at < threshold {
        write_guard(conn, regime, &guard)?;
        return Ok(false);
    }
    let mode = engine_mode(conn);
    match rollback_with_trigger(conn, set_id, &mode, "no_candidate") {
        Ok(()) => {
            // 回滚成功后守卫指向回滚后的 active 集，并标记该集已回滚过。
            // trend 回退时 active 集在 range，必须用含回退的查询，不能指向已退役集。
            match active_set_for_regime(conn, regime)? {
                Some(set) => write_guard(conn, regime, &guard_value(set.id, now, true))?,
                None => {
                    guard["rollback_done"] = json!(true);
                    write_guard(conn, regime, &guard)?;
                }
            }
            record_guard_event(
                conn,
                "warn",
                &format!("ai_tuning.no_candidate_rollback_ts.{regime}"),
                &format!(
                    "ai_tuning.no_candidate_rollback: {regime} 长时间无候选，已回滚AI 调参 {set_id}"
                ),
                json!({
                    "regime": regime,
                    "set_id": set_id,
                    "regime_after": active_set_for_regime(conn, regime)?
                        .map(|set| set.id),
                    "mode": mode,
                    "trigger": "no_candidate",
                    "elapsed_sec": now - last_candidate_at,
                }),
                now,
            )?;
            Ok(true)
        }
        Err(error) => {
            // 首次 bootstrap 等没有上一套：只告警，不能每轮重复轰炸。
            guard["last_candidate_at"] = json!(now);
            write_guard(conn, regime, &guard)?;
            record_guard_event(
                conn,
                "warn",
                &format!("ai_tuning.no_candidate_rollback_ts.{regime}"),
                &format!("ai_tuning.no_candidate_rollback: {regime} 无可回滚的上一套AI 调参"),
                json!({
                    "regime": regime,
                    "set_id": set_id,
                    "mode": mode,
                    "trigger": "no_candidate",
                    "error": error,
                }),
                now,
            )?;
            Ok(false)
        }
    }
}

fn write_guard(conn: &Connection, regime: &str, guard: &Value) -> Result<(), String> {
    upsert_engine_state(
        conn,
        &format!("ai_tuning.guard.{regime}"),
        &guard.to_string(),
    )
}

fn record_guard_event(
    conn: &Connection,
    level: &str,
    throttle_key: &str,
    message: &str,
    payload: Value,
    now: i64,
) -> Result<(), String> {
    let last = engine_state(conn, throttle_key).and_then(|raw| raw.trim().parse::<i64>().ok());
    if last.is_some_and(|last| now - last < GUARD_EVENT_THROTTLE_SEC) {
        return Ok(());
    }
    record_event(conn, level, message, payload, now)?;
    upsert_engine_state(conn, throttle_key, &now.to_string())
}

fn engine_state(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM engine_state WHERE key=?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn upsert_engine_state(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO engine_state(key,value,updated_at) VALUES(?1,?2,?3) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![key, value, crate::now()],
    )
    .map(|_| ())
    .map_err(|error| format!("写入 engine_state.{key} 失败: {error}"))
}

fn engine_mode(conn: &Connection) -> String {
    engine_state(conn, "mode").unwrap_or_else(|| "demo".to_string())
}

/// 手动回滚：写 `trigger='rollback'` 的 activation 与事件。
/// C4 手动 API 才会调用，C3 自动守卫走 `rollback_with_trigger(...,\"no_candidate\")`。
pub(crate) fn rollback(conn: &Connection, current_id: i64, mode: &str) -> Result<(), String> {
    rollback_with_trigger(conn, current_id, mode, "rollback")
}

/// 回滚到该AI 调参最近一次激活前的上一套；`trigger` 决定审计来源
/// （手动 `rollback`、自动守卫 `no_candidate`）。
pub(crate) fn rollback_with_trigger(
    conn: &Connection,
    current_id: i64,
    mode: &str,
    trigger: &str,
) -> Result<(), String> {
    let current = load_set(conn, current_id)?;
    if current.status != "active" {
        return Err(format!(
            "只能回滚当前 active AI 调参（{} 当前状态 {}）",
            current_id, current.status
        ));
    }
    let previous_id: Option<i64> = conn
        .query_row(
            "SELECT previous_set_id FROM ai_tuning_activations \
             WHERE set_id=?1 AND previous_set_id IS NOT NULL ORDER BY id DESC LIMIT 1",
            [current_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|e| format!("读取AI 调参 {current_id} 激活历史失败: {e}"))?;
    let Some(previous_id) = previous_id else {
        return Err(format!("AI 调参 {} 没有上一套可回滚", current_id));
    };
    let previous = load_set(conn, previous_id)?;
    if previous.regime != current.regime {
        return Err(format!(
            "上一套AI 调参 regime {} 与当前 {} 不一致，拒绝回滚",
            previous.regime, current.regime
        ));
    }
    conn.execute_batch("SAVEPOINT ai_tuning_rollback")
        .map_err(|e| format!("开启AI 调参回滚事务失败: {e}"))?;
    match rollback_in_savepoint(conn, &current, &previous, mode, trigger) {
        Ok(()) => conn
            .execute_batch("RELEASE ai_tuning_rollback")
            .map_err(|e| format!("提交AI 调参回滚事务失败: {e}")),
        Err(error) => {
            let _ =
                conn.execute_batch("ROLLBACK TO ai_tuning_rollback; RELEASE ai_tuning_rollback");
            Err(error)
        }
    }
}

fn rollback_in_savepoint(
    conn: &Connection,
    current: &AiTuningSet,
    previous: &AiTuningSet,
    mode: &str,
    trigger: &str,
) -> Result<(), String> {
    if !previous.params.is_object() {
        return Err(format!("AI 调参 {} params 必须是对象快照", previous.id));
    }
    let before = crate::settings_api::group_value_public(conn, "strategy");
    let after = crate::settings_api::normalized_group_value("strategy", &previous.params);
    crate::settings_api::validate_group("strategy", &after)?;
    crate::settings_api::validate_indicator_windows(
        &after,
        &crate::settings_api::group_value_public(conn, "data"),
    )?;

    let now = crate::now();
    conn.execute(
        "UPDATE ai_tuning_sets SET status='retired' \
         WHERE regime=?1 AND status='active' AND id<>?2",
        params![previous.regime, previous.id],
    )
    .map_err(|e| format!("退役当前AI 调参失败: {e}"))?;
    conn.execute(
        "UPDATE ai_tuning_sets SET status='active', activated_at=?1 WHERE id=?2",
        params![now, previous.id],
    )
    .map_err(|e| format!("恢复上一套AI 调参 {} 失败: {e}", previous.id))?;

    conn.execute(
        "INSERT INTO app_settings(key,value,updated_at) VALUES('strategy',?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![
            serde_json::to_string(&after).map_err(|e| e.to_string())?,
            now
        ],
    )
    .map_err(|e| format!("恢复 strategy 失败: {e}"))?;
    crate::settings_api::record_set_changes(
        conn,
        &before,
        &after,
        &previous.regime,
        previous.id,
        mode,
        "ai_tuning_rollback",
    )?;
    insert_activation(
        conn,
        previous.id,
        &previous.regime,
        Some(current.id),
        trigger,
        now,
    )?;
    record_event(
        conn,
        "info",
        &format!("AI 调参 {} 回滚到 {}", current.name, previous.name),
        json!({
            "set_id": previous.id,
            "previous_set_id": current.id,
            "regime": previous.regime,
            "mode": mode,
            "trigger": trigger,
        }),
        now,
    )
}

fn record_event(
    conn: &Connection,
    level: &str,
    message: &str,
    payload: Value,
    now: i64,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO engine_events(ts,level,category,message,payload) VALUES(?1,?2,'ai_tuning',?3,?4)",
        params![now, level, message, payload.to_string()],
    )
    .map_err(|e| format!("写入AI 调参事件失败: {e}"))?;
    Ok(())
}

/// 只写候选记录：不激活、不写配置；激活必须由人工触发。
pub(crate) fn insert_candidate(
    conn: &Connection,
    name: &str,
    regime: &str,
    timeframe: &str,
    params_value: &Value,
    parent_id: Option<i64>,
) -> Result<i64, String> {
    insert_candidate_with_source(conn, name, regime, timeframe, params_value, "ai", parent_id)
}

/// GA 离线搜索结果同样只写候选：`source='ga'`，证据字段留空，待证据门评估。
/// 证据门只落 approved/rejected，任何候选都必须人工激活。
pub(crate) fn insert_ga_candidate(
    conn: &Connection,
    name: &str,
    regime: &str,
    timeframe: &str,
    params_value: &Value,
    parent_id: Option<i64>,
) -> Result<i64, String> {
    insert_candidate_with_source(conn, name, regime, timeframe, params_value, "ga", parent_id)
}

fn insert_candidate_with_source(
    conn: &Connection,
    name: &str,
    regime: &str,
    timeframe: &str,
    params_value: &Value,
    source: &str,
    parent_id: Option<i64>,
) -> Result<i64, String> {
    if !params_value.is_object() {
        return Err("候选参数必须是对象快照".to_string());
    }
    if params_value.get("timeframe").and_then(Value::as_str) != Some(timeframe) {
        return Err(format!("候选快照 timeframe 必须等于 {timeframe}"));
    }
    if let Some(parent_id) = parent_id {
        let parent = load_set(conn, parent_id)?;
        if parent.regime != regime {
            return Err(format!(
                "父集 regime {} 与候选 regime {regime} 不一致",
                parent.regime
            ));
        }
    }
    insert_set(
        conn,
        name,
        regime,
        timeframe,
        params_value,
        "candidate",
        source,
        "{}",
        parent_id,
        crate::now(),
    )
}

/// 证据门结果落库：只允许改写待评估的 AI/GA 候选。
/// `active`/`strategy` 与人工AI 调参一律不动；没有匹配行时返回 Err，避免静默丢结果。
pub(crate) fn mark_evidence(
    conn: &Connection,
    id: i64,
    pass: bool,
    evidence: &Value,
) -> Result<(), String> {
    let status = if pass { "approved" } else { "rejected" };
    let updated = conn
        .execute(
            "UPDATE ai_tuning_sets SET status=?1, evidence=?2 \
             WHERE id=?3 AND source IN ('ai','ga') AND status='candidate'",
            params![status, evidence.to_string(), id],
        )
        .map_err(|error| format!("写入AI 调参 {id} 证据失败: {error}"))?;
    if updated == 0 {
        return Err(format!("AI 调参 {id} 不是待评估的 AI/GA 候选，未写入证据"));
    }
    Ok(())
}

/// 把 AI 参数增量合并到 active 快照上并校验，返回完整 strategy 快照。
/// 任何一条不合规都整体拒绝，避免部分写入。
pub(crate) fn validate_candidate(
    active: &Value,
    patch: &[crate::ai_client::ParamUpdate],
) -> Result<Value, String> {
    let Some(mut next) = active.as_object().cloned() else {
        return Err("active AI 调参必须是对象".to_string());
    };
    let unique_keys: std::collections::HashSet<(&str, &str)> = patch
        .iter()
        .map(|update| (update.group.as_str(), update.key.as_str()))
        .collect();
    if unique_keys.len() > MAX_CANDIDATE_KEYS {
        return Err(format!(
            "候选最多允许 {MAX_CANDIDATE_KEYS} 个键，实际 {}",
            unique_keys.len()
        ));
    }
    for update in patch {
        if update.group != "strategy" {
            return Err(format!(
                "候选只允许 strategy 组，拒绝 {}.{}",
                update.group, update.key
            ));
        }
        if update.key == "timeframe" {
            return Err("候选不允许修改 timeframe".to_string());
        }
        let Some(meta) = crate::param_registry::find("strategy", &update.key) else {
            return Err(format!("候选键 strategy.{} 未注册", update.key));
        };
        if !meta.ai_writable {
            return Err(format!("候选键 strategy.{} 不允许 AI 写入", update.key));
        }
        if meta.kind != "number" {
            return Err(format!(
                "候选键 strategy.{} 类型为 {}，仅允许数字",
                update.key, meta.kind
            ));
        }
        // 基线始终取自不可变 active：重复键不得相对中间 next 复算。
        let old = active
            .get(update.key.as_str())
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("候选键 strategy.{} 缺少基线值，拒绝调参", update.key))?;
        let new = update
            .value
            .as_f64()
            .ok_or_else(|| format!("候选键 strategy.{} 必须是数字", update.key))?;
        if old != 0.0 && ((new - old).abs() / old.abs()) > MAX_RELATIVE_CHANGE {
            return Err(format!(
                "候选键 strategy.{} 相对变化超过 {}%",
                update.key,
                (MAX_RELATIVE_CHANGE * 100.0) as i64
            ));
        }
        if let Some((low, high)) = crate::param_registry::ai_bounds("strategy", &update.key) {
            if !(low..=high).contains(&new) {
                return Err(format!(
                    "候选键 strategy.{} 超出 AI 边界 [{low},{high}]",
                    update.key
                ));
            }
        }
        let coerced = crate::param_registry::coerce(&meta, &update.value)?;
        // coerce 按 RANGES 收敛；若未来 RANGES 与 ai_bounds 区间漂移，这里再兜底一次。
        if let Some((low, high)) = crate::param_registry::ai_bounds("strategy", &update.key) {
            let value = coerced
                .as_f64()
                .ok_or_else(|| format!("候选键 strategy.{} 必须是数字", update.key))?;
            if !(low..=high).contains(&value) {
                return Err(format!(
                    "候选键 strategy.{} 超出 AI 边界 [{low},{high}]",
                    update.key
                ));
            }
        }
        next.insert(update.key.clone(), coerced);
    }
    let Some(active_timeframe) = active.get("timeframe").and_then(Value::as_str) else {
        return Err("active AI 调参缺少 timeframe，无法校验候选".to_string());
    };
    if next.get("timeframe").and_then(Value::as_str) != Some(active_timeframe) {
        return Err("候选AI 调参 timeframe 必须与 active 一致".to_string());
    }
    let snapshot = Value::Object(next);
    crate::settings_api::validate_group("strategy", &snapshot)?;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_client::ParamUpdate;
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
        conn.execute(
            "INSERT INTO app_settings VALUES('ai_tuning',?1,0)",
            [serde_json::json!({"enabled": true}).to_string()],
        )
        .unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    fn all_sets(conn: &Connection) -> Vec<SetRow> {
        conn.prepare(
            "SELECT name,regime,timeframe,status,source,evidence,parent_id,params \
             FROM ai_tuning_sets ORDER BY id",
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

    fn param_update(key: &str, value: Value) -> ParamUpdate {
        ParamUpdate {
            group: "strategy".to_string(),
            key: key.to_string(),
            value,
            reason: String::new(),
        }
    }

    fn seeded_snapshot(conn: &Connection, regime: &str) -> Value {
        active_for_regime(conn, regime)
            .unwrap()
            .unwrap_or_else(|| panic!("{regime} 应有 active AI 调参"))
            .params
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
                 FROM ai_tuning_activations a JOIN ai_tuning_sets p ON p.id = a.set_id \
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
            assert!(previous_set_id.is_none(), "首次种子化不应有上一套AI 调参");
            assert_eq!(regime, set_regime, "激活记录 regime 必须与AI 调参一致");
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
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
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
            .query_row("SELECT COUNT(*) FROM ai_tuning_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
        let activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(activations, 2);
    }

    #[test]
    fn seed_bootstrap_rolls_back_all_writes_on_failure() {
        let conn = conn_with_strategy("1h");
        conn.execute_batch(
            "CREATE TRIGGER seed_boom BEFORE INSERT ON ai_tuning_sets \
             WHEN NEW.name='chaos-bootstrap' BEGIN SELECT RAISE(ABORT,'boom'); END;",
        )
        .unwrap();
        assert!(seed_bootstrap(&conn).is_err());
        let sets: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sets, 0, "种子化失败必须整体回滚");
        let activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(activations, 0, "激活记录也必须回滚");
    }

    #[test]
    fn non_1h_seeds_only_retired_snapshot() {
        let conn = conn_with_strategy("4h");
        seed_bootstrap(&conn).unwrap();
        let sets = all_sets(&conn);
        assert_eq!(sets.len(), 1);
        let snapshot = &sets[0];
        assert_eq!(snapshot.name, "current-snapshot");
        assert_eq!(snapshot.status, "retired");
        assert_eq!(snapshot.source, "manual");
        assert_eq!(snapshot.evidence["kind"], "manual");
        assert_eq!(snapshot.evidence["snapshot"], "full");
        assert_eq!(snapshot.evidence["reason"], "non_bootstrap_timeframe");
        assert!(
            snapshot.evidence.get("historical").is_none(),
            "非 1h 快照不是历史审计，不应带 historical"
        );
        assert!(snapshot.parent_id.is_none());
        assert_eq!(snapshot.params["timeframe"], snapshot.timeframe);

        let activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(activations, 0);
    }

    #[test]
    fn non_1h_with_corrupt_ai_tuning_config_keeps_going() {
        let conn = conn_with_strategy("4h");
        conn.execute(
            "UPDATE app_settings SET value='not-json' WHERE key='ai_tuning'",
            [],
        )
        .unwrap();
        assert!(seed_bootstrap(&conn).is_ok(), "脏配置不能阻断种子化");

        let sets = all_sets(&conn);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].name, "current-snapshot");
        assert_eq!(sets[0].status, "retired");
        let activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(activations, 0);
        let value: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='ai_tuning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value, "not-json", "脏配置保持原样，不做半更新");
    }

    #[test]
    fn ai_tuning_has_no_auto_apply_path() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let raw: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='ai_tuning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let config: Value = serde_json::from_str(&raw).unwrap();
        assert!(
            config.get("auto_apply").is_none(),
            "不得再存在 auto_apply 键"
        );
        assert!(
            config.get("auto_apply_timeframes").is_none(),
            "不得再存在 auto_apply_timeframes 键"
        );
        assert!(
            config.get("max_auto_apply_per_24h").is_none(),
            "不得再存在 max_auto_apply_per_24h 键"
        );
        let since: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_state WHERE key='ai_tuning.auto_apply_since'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(since, 0, "不得再写入 ai_tuning.auto_apply_since");
        assert!(
            crate::param_registry::find("ai_tuning", "auto_apply").is_none(),
            "auto_apply 不得再注册为配置键"
        );
        assert!(
            crate::param_registry::find("ai_tuning", "auto_apply_timeframes").is_none(),
            "auto_apply_timeframes 不得再注册为配置键"
        );
        assert!(
            crate::param_registry::find("ai_tuning", "max_auto_apply_per_24h").is_none(),
            "max_auto_apply_per_24h 不得再注册为配置键"
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
            .query_row("SELECT COUNT(*) FROM ai_tuning_sets", [], |r| r.get(0))
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
            .query_row("SELECT COUNT(*) FROM ai_tuning_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn candidate_validation_rejects_risk_and_three_key_limit() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let active = seeded_snapshot(&conn, "range");

        let risk = ParamUpdate {
            group: "risk".to_string(),
            key: "max_leverage".to_string(),
            value: json!(5),
            reason: String::new(),
        };
        assert!(
            validate_candidate(&active, &[risk]).is_err(),
            "risk 组必须拒绝"
        );

        let keys = [
            "entry_threshold",
            "exit_threshold",
            "roi_min_r",
            "trailing_pct",
        ];
        let patch: Vec<ParamUpdate> = keys
            .iter()
            .map(|key| param_update(key, active[*key].clone()))
            .collect();
        assert_eq!(patch.len(), 4);
        assert!(
            validate_candidate(&active, &patch).is_err(),
            "4 个键必须拒绝"
        );
    }

    #[test]
    fn candidate_validation_rejects_timeframe_change() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let active = seeded_snapshot(&conn, "range");

        let changed = param_update("timeframe", json!("4h"));
        assert!(
            validate_candidate(&active, &[changed]).is_err(),
            "不允许直接改 timeframe"
        );

        let mut missing = active.clone();
        missing.as_object_mut().unwrap().remove("timeframe");
        let patch = [param_update("entry_threshold", json!(0.45))];
        assert!(
            validate_candidate(&missing, &patch).is_err(),
            "无法确认 timeframe 一致时必须拒绝"
        );
    }

    #[test]
    fn candidate_step_over_25_percent_is_rejected() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let active = seeded_snapshot(&conn, "range");
        assert_eq!(active["entry_threshold"], 0.45);

        let patch = [param_update("entry_threshold", json!(0.60))];
        assert!(
            validate_candidate(&active, &patch).is_err(),
            "0.45→0.60 超过 25%"
        );
    }

    #[test]
    fn chaos_parent_can_lower_atr_min_within_hard_bound() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let active = seeded_snapshot(&conn, "chaos");
        assert_eq!(active["atr_pct_min"], 1.0);

        let ok = validate_candidate(&active, &[param_update("atr_pct_min", json!(0.75))]).unwrap();
        assert_eq!(ok["atr_pct_min"], 0.75);
        assert_eq!(ok["timeframe"], active["timeframe"]);

        assert!(
            validate_candidate(&active, &[param_update("atr_pct_min", json!(0.5))]).is_err(),
            "1.0→0.5 超过 25%"
        );
        assert!(
            validate_candidate(&active, &[param_update("atr_pct_min", json!(1.05))]).is_err(),
            "1.05 超出 ai_bounds 上界 1.0"
        );
    }

    #[test]
    fn insert_candidate_persists_complete_snapshot() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let parent = active_for_regime(&conn, "range").unwrap().unwrap();
        let snapshot = validate_candidate(
            &parent.params,
            &[param_update("entry_threshold", json!(0.40))],
        )
        .unwrap();

        let id = insert_candidate(
            &conn,
            "ai-range-1",
            "range",
            "1h",
            &snapshot,
            Some(parent.id),
        )
        .unwrap();
        let set = load_set(&conn, id).unwrap();
        assert_eq!(set.name, "ai-range-1");
        assert_eq!(set.status, "candidate");
        assert_eq!(set.source, "ai");
        assert_eq!(set.timeframe, "1h");
        assert_eq!(set.params["timeframe"], "1h");
        assert_eq!(set.parent_id, Some(parent.id));
        assert_eq!(set.params, snapshot, "candidate 必须保存完整快照");
        assert_eq!(set.evidence, json!({}));
        assert_eq!(set.version, 1);
        assert!(set.activated_at.is_none(), "候选不应被激活");

        let active = active_for_regime(&conn, "range").unwrap().unwrap();
        assert_eq!(active.id, parent.id, "插入候选不得改变 active");
    }

    #[test]
    fn ga_candidate_requires_manual_activation() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let parent = active_for_regime(&conn, "range").unwrap().unwrap();
        let mut snapshot = parent.params.clone();
        snapshot["entry_threshold"] = json!(0.2);

        let id = insert_ga_candidate(&conn, "ga-range-1", "range", "1h", &snapshot, None).unwrap();
        let candidate = load_set(&conn, id).unwrap();
        assert_eq!(candidate.source, "ga");
        assert_eq!(candidate.status, "candidate");
        assert_eq!(candidate.evidence, json!({}), "证据字段留空待门评估");
        assert!(candidate.activated_at.is_none(), "GA 候选不得被激活");

        mark_evidence(&conn, id, true, &json!({"gate": {"pass": true}})).unwrap();

        let after = load_set(&conn, id).unwrap();
        assert_eq!(after.status, "approved");
        assert_eq!(after.source, "ga");
        assert!(after.activated_at.is_none(), "证据门通过后必须等待人工激活");
        let active = active_for_regime(&conn, "range").unwrap().unwrap();
        assert_eq!(active.id, parent.id, "GA 候选不得替换 active");

        activate(&conn, id, "manual", "demo").unwrap();
        assert_eq!(load_set(&conn, id).unwrap().status, "active");
        assert_eq!(load_set(&conn, parent.id).unwrap().status, "retired");
    }

    #[test]
    fn active_for_regime_returns_active_set() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();

        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().name,
            "range-bootstrap"
        );
        assert_eq!(
            active_for_regime(&conn, "chaos").unwrap().unwrap().name,
            "chaos-bootstrap"
        );
        assert!(active_for_regime(&conn, "trend").unwrap().is_none());

        let bare = Connection::open_in_memory().unwrap();
        assert!(
            active_for_regime(&bare, "range").is_err(),
            "缺表必须传播 DB 错误，不能当作无 active"
        );
    }

    #[test]
    fn candidate_validation_requires_numeric_baseline_but_allows_zero() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let active = seeded_snapshot(&conn, "range");

        let missing = [param_update("exit_threshold", json!(0.5))];
        assert!(
            validate_candidate(&active, &missing).is_err(),
            "active 缺少目标键时必须拒绝，不能跳过步长检查"
        );

        let mut zero = active;
        zero["trailing_pct"] = json!(0.0);
        let at_zero = [param_update("trailing_pct", json!(2.0))];
        let snapshot = validate_candidate(&zero, &at_zero).unwrap();
        assert_eq!(snapshot["trailing_pct"], 2.0, "old=0 时跳过比例检查");
    }

    #[test]
    fn duplicate_candidate_keys_still_use_immutable_active_baseline() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let mut active = seeded_snapshot(&conn, "range");
        active["trailing_pct"] = json!(2.0);

        let patch = [
            param_update("trailing_pct", json!(2.4)),
            param_update("trailing_pct", json!(2.9)),
        ];
        assert!(
            validate_candidate(&active, &patch).is_err(),
            "第二次必须相对 active 2.0 计算 45%，不能相对中间值 2.4 只算 20%"
        );
    }

    #[test]
    fn candidate_rejects_array_strategy_key() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let active = seeded_snapshot(&conn, "range");
        let patch = [param_update("roi_table", json!([[1, 2]]))];
        assert!(
            validate_candidate(&active, &patch).is_err(),
            "roi_table 是数组，不允许 AI 候选"
        );
    }

    #[test]
    fn insert_candidate_rejects_timeframe_and_parent_mismatch() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let parent = active_for_regime(&conn, "range").unwrap().unwrap();
        let snapshot = validate_candidate(
            &parent.params,
            &[param_update("entry_threshold", json!(0.40))],
        )
        .unwrap();

        assert!(
            insert_candidate(&conn, "bad-tf", "range", "4h", &snapshot, Some(parent.id)).is_err(),
            "timeframe 列与快照不一致必须拒绝"
        );
        assert!(
            insert_candidate(&conn, "no-parent", "range", "1h", &snapshot, Some(999_999)).is_err(),
            "父集不存在必须拒绝"
        );
        assert!(
            insert_candidate(
                &conn,
                "bad-regime",
                "chaos",
                "1h",
                &snapshot,
                Some(parent.id)
            )
            .is_err(),
            "父集 regime 与候选不一致必须拒绝"
        );
        assert!(
            insert_candidate(&conn, "not-object", "range", "1h", &json!([]), None).is_err(),
            "非对象快照必须拒绝"
        );

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_sets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3, "失败的候选不应写入");
    }

    fn pending_candidate(conn: &Connection, name: &str) -> (i64, i64) {
        let parent = active_for_regime(conn, "range").unwrap().unwrap();
        let snapshot = validate_candidate(
            &parent.params,
            &[param_update("entry_threshold", json!(0.40))],
        )
        .unwrap();
        let id = insert_candidate(conn, name, "range", "1h", &snapshot, Some(parent.id)).unwrap();
        (id, parent.id)
    }

    #[test]
    fn mark_evidence_approves_pending_candidate_without_touching_active() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let (candidate_id, parent_id) = pending_candidate(&conn, "range-ai-approved");
        let evidence = json!({"gate":{"pass":true,"reasons":[]}});

        mark_evidence(&conn, candidate_id, true, &evidence).unwrap();

        let set = load_set(&conn, candidate_id).unwrap();
        assert_eq!(set.status, "approved");
        assert_eq!(set.source, "ai");
        assert_eq!(set.evidence, evidence);
        assert!(set.activated_at.is_none(), "B 期证据门不得激活AI 调参");
        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().id,
            parent_id,
            "落证据不得改变 active"
        );

        assert!(
            mark_evidence(&conn, candidate_id, false, &json!({"gate":{"pass":false}})).is_err(),
            "已评审候选不能再写一次证据"
        );
        let set = load_set(&conn, candidate_id).unwrap();
        assert_eq!(set.status, "approved", "重复写入失败不得改状态");
        assert_eq!(set.evidence, evidence, "重复写入失败不得改证据");

        assert!(
            mark_evidence(&conn, parent_id, true, &json!({})).is_err(),
            "active 不是待评估 AI 候选，必须拒绝"
        );
        assert_eq!(
            load_set(&conn, parent_id).unwrap().status,
            "active",
            "拒绝写入不得改动其他状态"
        );
    }

    #[test]
    fn mark_evidence_rejects_failed_candidate() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let (candidate_id, _) = pending_candidate(&conn, "range-ai-rejected");
        let evidence = json!({"gate":{"pass":false,"reasons":["insufficient_oos_trades: 0 < 30"]}});

        mark_evidence(&conn, candidate_id, false, &evidence).unwrap();

        let set = load_set(&conn, candidate_id).unwrap();
        assert_eq!(set.status, "rejected");
        assert_eq!(set.source, "ai");
        assert_eq!(set.evidence, evidence);
    }

    fn set_id_by_name(conn: &Connection, name: &str) -> i64 {
        conn.query_row("SELECT id FROM ai_tuning_sets WHERE name=?1", [name], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn strategy_value(conn: &Connection) -> Value {
        let raw: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn activate_materializes_strategy_and_retires_previous() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let range_id = set_id_by_name(&conn, "range-bootstrap");
        let (candidate_id, parent_id) = pending_candidate(&conn, "range-ai-approved");
        assert_eq!(parent_id, range_id);
        mark_evidence(&conn, candidate_id, true, &json!({"gate":{"pass":true}})).unwrap();

        activate(&conn, candidate_id, "regime", "demo").unwrap();

        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().id,
            candidate_id
        );
        assert_eq!(
            load_set(&conn, range_id).unwrap().status,
            "retired",
            "同 regime 旧 active 必须退役"
        );
        assert!(
            load_set(&conn, candidate_id)
                .unwrap()
                .activated_at
                .is_some(),
            "激活后应写入 activated_at"
        );

        let strategy = strategy_value(&conn);
        assert_eq!(strategy["entry_threshold"], 0.40, "候选参数必须物化");
        assert_eq!(strategy["timeframe"], "1h");

        let changes: Vec<(String, String, String, String, i64, String)> = conn
            .prepare(
                "SELECT key, new_value, mode, source, set_id, regime FROM parameter_changes \
                 WHERE source='ai_tuning' ORDER BY key",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .unwrap()
            .flatten()
            .collect();
        assert!(
            changes
                .iter()
                .any(|(key, new_value, mode, source, set_id, regime)| {
                    key == "entry_threshold"
                        && new_value == "0.4"
                        && mode == "demo"
                        && source == "ai_tuning"
                        && *set_id == candidate_id
                        && regime == "range"
                }),
            "逐键变更必须带 mode/source/set_id/regime: {changes:?}"
        );
        let decision_ids: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM parameter_changes \
                 WHERE source='ai_tuning' AND decision_id=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(decision_ids as usize, changes.len());

        let (set_id, previous_id, trigger): (i64, Option<i64>, String) = conn
            .query_row(
                "SELECT set_id, previous_set_id, trigger FROM ai_tuning_activations \
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(set_id, candidate_id);
        assert_eq!(previous_id, Some(range_id));
        assert_eq!(trigger, "regime");

        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events \
                 WHERE level='info' AND category='ai_tuning' AND message LIKE '%已激活%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1);
    }

    #[test]
    fn activate_rejects_rejected_retired_and_timeframe_mismatch() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let (candidate_id, _) = pending_candidate(&conn, "range-ai-rejected");
        mark_evidence(&conn, candidate_id, false, &json!({"gate":{"pass":false}})).unwrap();
        assert!(
            activate(&conn, candidate_id, "regime", "demo").is_err(),
            "rejected 集不允许激活"
        );

        let retired_id = set_id_by_name(&conn, "chaos-live-2026-11-23");
        assert!(
            activate(&conn, retired_id, "regime", "demo").is_err(),
            "retired 审计集不允许激活"
        );

        let range = active_for_regime(&conn, "range").unwrap().unwrap();
        let mismatched = insert_set(
            &conn,
            "range-4h-approved",
            "range",
            "4h",
            &range.params,
            "approved",
            "manual",
            "{}",
            None,
            crate::now(),
        )
        .unwrap();
        assert!(
            activate(&conn, mismatched, "regime", "demo").is_err(),
            "timeframe 与当前策略不一致必须拒绝"
        );
        let active = active_for_regime(&conn, "range").unwrap().unwrap();
        assert_eq!(active.id, range.id, "失败的激活不得改变 active");
    }

    #[test]
    fn activate_rejects_indicator_window_beyond_snapshot() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let range = active_for_regime(&conn, "range").unwrap().unwrap();
        let mut params = range.params.clone();
        params["ema_slow"] = json!(250);
        let bad_id = insert_candidate(
            &conn,
            "range-bad-window",
            "range",
            "1h",
            &params,
            Some(range.id),
        )
        .unwrap();
        mark_evidence(&conn, bad_id, true, &json!({"gate":{"pass":true}})).unwrap();
        let before = strategy_value(&conn);

        let error = activate(&conn, bad_id, "regime", "demo").unwrap_err();

        assert!(error.contains("ema_slow"), "实际错误: {error}");
        assert_eq!(
            strategy_value(&conn),
            before,
            "非法指标窗口不得物化到 strategy"
        );
        assert_eq!(
            load_set(&conn, bad_id).unwrap().status,
            "approved",
            "失败的激活不得改状态"
        );
    }

    #[test]
    fn rollback_rejects_indicator_window_beyond_snapshot() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let current = active_for_regime(&conn, "range").unwrap().unwrap();
        let mut params = current.params.clone();
        params["ema_slow"] = json!(250);
        let bad_id = insert_candidate(
            &conn,
            "range-bad-window-rollback",
            "range",
            "1h",
            &params,
            Some(current.id),
        )
        .unwrap();
        let bad = load_set(&conn, bad_id).unwrap();
        let before = strategy_value(&conn);

        let error = rollback_in_savepoint(&conn, &current, &bad, "demo", "manual").unwrap_err();

        assert!(error.contains("ema_slow"), "实际错误: {error}");
        assert_eq!(strategy_value(&conn), before, "非法窗口不得回滚物化");
    }

    #[test]
    fn activate_for_regime_uses_active_set_and_falls_back_trend_to_range() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let range_id = set_id_by_name(&conn, "range-bootstrap");

        activate_for_regime(&conn, "range", "regime", "demo").unwrap();
        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().id,
            range_id
        );

        assert!(
            active_for_regime(&conn, "trend").unwrap().is_none(),
            "trend 没有自己的 active 集"
        );
        activate_for_regime(&conn, "trend", "regime", "demo").unwrap();
        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().id,
            range_id,
            "trend 应回退到 range active 集"
        );

        let fresh = conn_with_strategy("1h");
        assert!(
            activate_for_regime(&fresh, "chaos", "regime", "demo").is_err(),
            "无 active 集必须报错"
        );
    }

    #[test]
    fn rollback_restores_previous_strategy_and_records_source() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let range_id = set_id_by_name(&conn, "range-bootstrap");
        let (candidate_id, _) = pending_candidate(&conn, "range-ai-approved");
        mark_evidence(&conn, candidate_id, true, &json!({"gate":{"pass":true}})).unwrap();
        activate(&conn, candidate_id, "regime", "demo").unwrap();
        assert_eq!(strategy_value(&conn)["entry_threshold"], 0.40);

        rollback(&conn, candidate_id, "demo").unwrap();

        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().id,
            range_id,
            "回滚应恢复上一套 active"
        );
        assert_eq!(load_set(&conn, candidate_id).unwrap().status, "retired");
        let strategy = strategy_value(&conn);
        assert_eq!(strategy["entry_threshold"], 0.45, "应恢复 range 快照参数");
        assert_eq!(strategy["atr_pct_min"], 0.6);

        let rollback_changes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM parameter_changes \
                 WHERE source='ai_tuning_rollback' AND set_id=?1 AND regime='range' AND mode='demo'",
                [range_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(rollback_changes > 0, "回滚必须逐键记录变更");

        let (set_id, previous_id, trigger): (i64, Option<i64>, String) = conn
            .query_row(
                "SELECT set_id, previous_set_id, trigger FROM ai_tuning_activations \
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(set_id, range_id);
        assert_eq!(previous_id, Some(candidate_id));
        assert_eq!(trigger, "rollback");
        let payload: String = conn
            .query_row(
                "SELECT payload FROM engine_events WHERE message LIKE '%回滚到%' \
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&payload).unwrap()["trigger"],
            json!("rollback"),
            "手动回滚事件的 trigger 必须与 activation 一致"
        );
    }

    #[test]
    fn rollback_without_previous_activation_errors() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let range_id = set_id_by_name(&conn, "range-bootstrap");
        assert!(
            rollback(&conn, range_id, "demo").is_err(),
            "bootstrap 首次激活没有上一套，只能告警"
        );
        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().id,
            range_id
        );
    }

    #[test]
    fn rollback_rejects_non_active_set_without_writes() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let (candidate_id, _) = pending_candidate(&conn, "range-ai-rollback-guard");
        mark_evidence(&conn, candidate_id, true, &json!({"gate":{"pass":true}})).unwrap();
        assert_eq!(load_set(&conn, candidate_id).unwrap().status, "approved");

        let before_strategy = strategy_value(&conn);
        let before_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |row| {
                row.get(0)
            })
            .unwrap();
        let before_changes: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_changes", [], |row| {
                row.get(0)
            })
            .unwrap();

        let error = rollback(&conn, candidate_id, "demo").unwrap_err();
        assert!(
            error.contains("只能回滚当前 active"),
            "非 active 应明确拒绝: {error}"
        );
        assert_eq!(
            load_set(&conn, candidate_id).unwrap().status,
            "approved",
            "拒绝回滚不得改状态"
        );
        assert_eq!(
            strategy_value(&conn),
            before_strategy,
            "拒绝回滚不得改 strategy"
        );
        let activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(activations, before_activations, "拒绝回滚不得写激活记录");
        let changes: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_changes", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(changes, before_changes, "拒绝回滚不得写变更记录");
    }

    #[test]
    fn activate_active_set_is_noop() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let range_id = set_id_by_name(&conn, "range-bootstrap");
        // 先完成首次物化；之后同参数 active 再次激活才是 no-op。
        activate(&conn, range_id, "regime", "demo").unwrap();

        let before_strategy = strategy_value(&conn);
        let before_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        let before_changes: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_changes", [], |r| r.get(0))
            .unwrap();
        let before_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE category='ai_tuning'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        activate(&conn, range_id, "regime", "demo").unwrap();

        assert_eq!(
            strategy_value(&conn),
            before_strategy,
            "同参数 active 不得改写 strategy"
        );
        let after_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(after_activations, before_activations, "不得新增激活记录");
        let after_changes: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after_changes, before_changes, "不得新增参数变更");
        let after_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE category='ai_tuning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after_events, before_events, "不得新增激活事件");
        assert_eq!(load_set(&conn, range_id).unwrap().status, "active");
    }

    #[test]
    fn activate_active_set_with_stale_strategy_materializes() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let range_id = set_id_by_name(&conn, "range-bootstrap");
        assert_eq!(
            strategy_value(&conn)["entry_threshold"],
            0.55,
            "种子化不物化，策略仍是原快照"
        );
        let before_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();

        activate(&conn, range_id, "regime", "demo").unwrap();

        let strategy = strategy_value(&conn);
        assert_eq!(strategy["entry_threshold"], 0.45, "active 但未同步必须物化");
        assert_eq!(strategy["atr_pct_min"], 0.6);
        assert_eq!(strategy["atr_pct_max"], 10.0);
        assert_eq!(strategy["volume_ratio_min"], 0.8);
        assert_eq!(strategy["timeframe"], "1h");
        let set = load_set(&conn, range_id).unwrap();
        assert_eq!(set.status, "active", "物化不得改变自身 active 状态");
        assert!(set.activated_at.is_some(), "物化必须更新 activated_at");

        let changes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM parameter_changes \
                 WHERE source='ai_tuning' AND set_id=?1 AND regime='range' AND mode='demo' \
                 AND decision_id=0",
                [range_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(changes > 0, "物化必须逐键记录变更");

        let (set_id, previous_id, trigger): (i64, Option<i64>, String) = conn
            .query_row(
                "SELECT set_id, previous_set_id, trigger FROM ai_tuning_activations \
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(set_id, range_id);
        assert_eq!(previous_id, None, "物化自身不应产生上一套");
        assert_eq!(trigger, "regime");

        let after_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(after_activations, before_activations + 1);

        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events \
                 WHERE level='info' AND category='ai_tuning' AND message LIKE '%已激活%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1);
    }

    fn active_range_set_with_legacy_key(conn: &Connection, name: &str) -> i64 {
        let range = active_for_regime(conn, "range").unwrap().unwrap();
        conn.execute(
            "UPDATE ai_tuning_sets SET status='retired' WHERE regime='range'",
            [],
        )
        .unwrap();
        let mut params = range.params.clone();
        params["weight_trend"] = json!(0.123);
        insert_set(
            conn,
            name,
            "range",
            "1h",
            &params,
            "active",
            "manual",
            "{}",
            None,
            crate::now(),
        )
        .unwrap()
    }

    #[test]
    fn activate_active_set_with_legacy_key_is_noop_when_strategy_matches() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let set_id = active_range_set_with_legacy_key(&conn, "range-legacy");
        let params = load_set(&conn, set_id).unwrap().params;
        // strategy 已经是该集归一化后的值：legacy 键被 migrate 剔除，不应触发物化。
        let normalized = crate::settings_api::normalized_group_value("strategy", &params);
        conn.execute(
            "UPDATE app_settings SET value=?1 WHERE key='strategy'",
            [normalized.to_string()],
        )
        .unwrap();
        let before_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        let before_changes: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_changes", [], |r| r.get(0))
            .unwrap();
        let before_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE category='ai_tuning'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        activate(&conn, set_id, "regime", "demo").unwrap();

        assert_eq!(strategy_value(&conn), normalized, "strategy 不应变化");
        assert!(
            strategy_value(&conn).get("weight_trend").is_none(),
            "历史键不得进入 strategy"
        );
        let after_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(after_activations, before_activations, "不得新增激活记录");
        let after_changes: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after_changes, before_changes, "不得新增参数变更");
        let after_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE category='ai_tuning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after_events, before_events, "不得新增激活事件");
    }

    #[test]
    fn activate_active_set_strips_legacy_key_and_materializes_once() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let set_id = active_range_set_with_legacy_key(&conn, "range-legacy-stale");
        let params = load_set(&conn, set_id).unwrap().params;
        let normalized = crate::settings_api::normalized_group_value("strategy", &params);
        assert_ne!(
            strategy_value(&conn),
            normalized,
            "前置条件：strategy 尚未物化"
        );

        activate(&conn, set_id, "regime", "demo").unwrap();

        let strategy = strategy_value(&conn);
        assert_eq!(strategy, normalized, "应写入归一化后的完整策略");
        assert_eq!(strategy["entry_threshold"], 0.45);
        assert!(
            strategy.get("weight_trend").is_none(),
            "历史键不得进入 strategy"
        );

        let legacy_changes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM parameter_changes WHERE key='weight_trend'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy_changes, 0, "历史键不应产生参数变更");
        let effective_changes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM parameter_changes \
                 WHERE source='ai_tuning' AND set_id=?1 AND key<>'weight_trend'",
                [set_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(effective_changes > 0, "有效差异键必须记录");

        let first_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        let first_changes: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_changes", [], |r| r.get(0))
            .unwrap();
        let first_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE category='ai_tuning'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        activate(&conn, set_id, "regime", "demo").unwrap();

        assert_eq!(strategy_value(&conn), strategy, "归一化后再次激活应 no-op");
        let after_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(after_activations, first_activations);
        let after_changes: i64 = conn
            .query_row("SELECT COUNT(*) FROM parameter_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after_changes, first_changes);
        let after_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE category='ai_tuning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after_events, first_events);
    }

    #[test]
    fn activate_rolls_back_savepoint_when_audit_write_fails() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let range_id = set_id_by_name(&conn, "range-bootstrap");
        let (candidate_id, _) = pending_candidate(&conn, "range-ai-approved");
        mark_evidence(&conn, candidate_id, true, &json!({"gate":{"pass":true}})).unwrap();
        let before_strategy = strategy_value(&conn);
        let before_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_parameter_changes BEFORE INSERT ON parameter_changes \
             BEGIN SELECT RAISE(ABORT,'boom'); END;",
        )
        .unwrap();

        assert!(
            activate(&conn, candidate_id, "regime", "demo").is_err(),
            "审计写入失败必须让激活整体失败"
        );

        assert_eq!(
            load_set(&conn, candidate_id).unwrap().status,
            "approved",
            "目标集必须回到原状态"
        );
        assert!(
            load_set(&conn, candidate_id)
                .unwrap()
                .activated_at
                .is_none(),
            "目标集不得留下 activated_at"
        );
        assert_eq!(
            load_set(&conn, range_id).unwrap().status,
            "active",
            "旧 active 不得被 retired"
        );
        assert_eq!(
            strategy_value(&conn),
            before_strategy,
            "strategy 必须回滚到激活前"
        );
        let after_activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(after_activations, before_activations, "不得留下激活记录");
    }

    fn approved_candidate(conn: &Connection, name: &str, regime: &str) -> i64 {
        let parent = active_for_regime(conn, "range").unwrap().unwrap();
        let snapshot = validate_candidate(
            &parent.params,
            &[param_update("entry_threshold", json!(0.40))],
        )
        .unwrap();
        let id = insert_candidate(conn, name, regime, "1h", &snapshot, None).unwrap();
        mark_evidence(conn, id, true, &json!({"gate":{"pass":true}})).unwrap();
        id
    }

    fn state_value(conn: &Connection, key: &str) -> Option<String> {
        conn.query_row(
            "SELECT value FROM engine_state WHERE key=?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap()
    }

    fn guard_fixture() -> (Connection, i64, i64) {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let bootstrap_id = set_id_by_name(&conn, "range-bootstrap");
        let candidate_id = approved_candidate(&conn, "range-ai-guard", "range");
        activate(&conn, candidate_id, "regime", "demo").unwrap();
        (conn, bootstrap_id, candidate_id)
    }

    #[test]
    fn guard_resets_on_candidate_and_pauses_on_skipped() {
        let (conn, _, candidate_id) = guard_fixture();
        set_guard(&conn, "range", candidate_id, 1_000).unwrap();

        assert!(!update_guard(&conn, "range", candidate_id, false, true, 1_100).unwrap());
        let guard: Value =
            serde_json::from_str(&state_value(&conn, "ai_tuning.guard.range").unwrap()).unwrap();
        assert_eq!(guard["last_candidate_at"], json!(1_100), "熔断轮暂停计时");

        assert!(!update_guard(&conn, "range", candidate_id, true, false, 1_200).unwrap());
        let guard: Value =
            serde_json::from_str(&state_value(&conn, "ai_tuning.guard.range").unwrap()).unwrap();
        assert_eq!(
            guard["last_candidate_at"],
            json!(1_200),
            "有候选必须重置计时"
        );

        assert!(!update_guard(&conn, "range", candidate_id, false, false, 2_900).unwrap());
        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().id,
            candidate_id,
            "未满 30 分钟不得回滚"
        );
    }

    #[test]
    fn guard_rolls_back_after_no_candidate_timeout() {
        let (conn, bootstrap_id, candidate_id) = guard_fixture();
        set_guard(&conn, "range", candidate_id, 1_000).unwrap();

        assert!(!update_guard(&conn, "range", candidate_id, false, false, 1_600).unwrap());
        assert!(update_guard(&conn, "range", candidate_id, false, false, 2_800).unwrap());

        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().id,
            bootstrap_id,
            "超时后必须回滚到上一套 active"
        );
        let guard: Value =
            serde_json::from_str(&state_value(&conn, "ai_tuning.guard.range").unwrap()).unwrap();
        assert_eq!(
            guard["set_id"],
            json!(bootstrap_id),
            "守卫重置到回滚后的 active 集"
        );
        assert_eq!(
            guard["rollback_done"],
            json!(true),
            "回滚后必须打标记防止抖动"
        );
        let trigger: String = conn
            .query_row(
                "SELECT trigger FROM ai_tuning_activations WHERE set_id=?1 \
                 ORDER BY id DESC LIMIT 1",
                [bootstrap_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trigger, "no_candidate", "自动守卫回滚必须写 no_candidate");
        let payload: String = conn
            .query_row(
                "SELECT payload FROM engine_events WHERE message LIKE 'ai_tuning.no_candidate_rollback%' \
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&payload).unwrap()["trigger"],
            json!("no_candidate"),
            "事件 payload 必须与 activation trigger 一致"
        );
        let warnings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE level='warn' \
                 AND message LIKE 'ai_tuning.no_candidate_rollback%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(warnings, 1);
    }

    #[test]
    fn guard_rollback_does_not_repeat_until_candidate_returns() {
        let (conn, bootstrap_id, candidate_id) = guard_fixture();
        set_guard(&conn, "range", candidate_id, 1_000).unwrap();
        assert!(update_guard(&conn, "range", candidate_id, false, false, 2_800).unwrap());
        let activations: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |row| {
                row.get(0)
            })
            .unwrap();
        let guard: Value =
            serde_json::from_str(&state_value(&conn, "ai_tuning.guard.range").unwrap()).unwrap();
        assert_eq!(guard["rollback_done"], json!(true));

        // 再过 30 分钟仍零候选：只 hold 告警，不得产生反向回滚。
        assert!(!update_guard(&conn, "range", bootstrap_id, false, false, 4_600).unwrap());
        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().id,
            bootstrap_id,
            "回滚标记必须阻止抖动"
        );
        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM ai_tuning_activations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(after, activations, "不得产生反向 activation");
        let holds: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events \
                 WHERE message LIKE 'ai_tuning.guard_rollback_hold%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(holds, 1);

        // 候选恢复后清除标记，守卫重新开始计时。
        assert!(!update_guard(&conn, "range", bootstrap_id, true, false, 4_700).unwrap());
        let guard: Value =
            serde_json::from_str(&state_value(&conn, "ai_tuning.guard.range").unwrap()).unwrap();
        assert_eq!(guard["rollback_done"], json!(false));
        assert_eq!(guard["last_candidate_at"], json!(4_700));
    }

    #[test]
    fn guard_without_previous_only_warns() {
        let conn = conn_with_strategy("1h");
        seed_bootstrap(&conn).unwrap();
        let bootstrap_id = set_id_by_name(&conn, "range-bootstrap");
        set_guard(&conn, "range", bootstrap_id, 1_000).unwrap();

        assert!(
            !update_guard(&conn, "range", bootstrap_id, false, false, 2_800).unwrap(),
            "没有上一套时不得报告回滚成功"
        );
        assert_eq!(
            active_for_regime(&conn, "range").unwrap().unwrap().id,
            bootstrap_id
        );
        let warnings: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE level='warn' \
                 AND message LIKE 'ai_tuning.no_candidate_rollback%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(warnings, 1, "只写告警，不中断周期");
    }
}
