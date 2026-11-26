//! AI 调参接口。B 期只读列表、当前视图与完整证据；C 期追加手动启用/回滚，
//! 复用 `ai_tuning` 的 SAVEPOINT 事务与审计，不在 API 层另写 `strategy`。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ai_tuning::{activate, active_for_regime, load_set, rollback, row_to_set, AiTuningSet};
use crate::db::Db;

const SET_COLUMNS: &str = "id,name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at";
const LIST_LIMIT: i64 = 100;
const ACTIVE_REGIMES: [&str; 3] = ["trend", "range", "chaos"];

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/ai-tuning", get(list_sets))
        .route("/ai-tuning/current", get(current_view))
        .route("/ai-tuning/{id}/evidence", get(set_evidence))
        .route("/ai-tuning/{id}/activate", post(activate_set))
        .route("/ai-tuning/{id}/rollback", post(rollback_set))
}

type ApiError = (StatusCode, Json<Value>);

fn server_error(message: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "detail": message.to_string() }).into(),
    )
}

fn not_found(message: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        json!({ "detail": message.to_string() }).into(),
    )
}

fn bad_request(message: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        json!({ "detail": message.to_string() }).into(),
    )
}

/// 与 runtime 一致：`engine_state.mode` 缺失时回落 `demo`。
fn current_mode(conn: &Connection) -> String {
    read_engine_state(conn, "mode")
        .ok()
        .flatten()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "demo".to_string())
}

/// 当前 regime 缺失时沿用 C 期 `market_regime` 的口径回退 `range`。
fn current_regime(conn: &Connection, mode: &str) -> String {
    read_engine_state(conn, &format!("market_regime.{mode}"))
        .ok()
        .flatten()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "range".to_string())
}

fn ensure_set_exists(conn: &Connection, id: i64) -> Result<(), ApiError> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM ai_tuning_sets WHERE id=?1)",
            [id],
            |row| row.get(0),
        )
        .map_err(server_error)?;
    if exists {
        Ok(())
    } else {
        Err(not_found(format!("AI 调参 {id} 不存在")))
    }
}

fn lock(db: &Db) -> Result<std::sync::MutexGuard<'_, Connection>, ApiError> {
    db.conn.lock().map_err(|_| server_error("数据库锁不可用"))
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    regime: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// 列表只返回摘要字段，完整证据走单独的 evidence 接口。
fn summary(set: &AiTuningSet) -> Value {
    json!({
        "id": set.id,
        "name": set.name,
        "regime": set.regime,
        "timeframe": set.timeframe,
        "version": set.version,
        "status": set.status,
        "source": set.source,
        "evidence": set.evidence,
        "parent_id": set.parent_id,
        "created_at": set.created_at,
        "activated_at": set.activated_at,
    })
}

async fn list_sets(
    State(db): State<Db>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let regime = query.regime.filter(|value| !value.is_empty());
    let status = query.status.filter(|value| !value.is_empty());
    let conn = lock(&db)?;
    let sql = format!(
        "SELECT {SET_COLUMNS} FROM ai_tuning_sets \
         WHERE (?1 IS NULL OR regime=?1) AND (?2 IS NULL OR status=?2) \
         ORDER BY id DESC LIMIT {LIST_LIMIT}"
    );
    let mut stmt = conn.prepare(&sql).map_err(server_error)?;
    let rows = stmt
        .query_map(params![regime.as_deref(), status.as_deref()], row_to_set)
        .map_err(server_error)?;
    let sets: rusqlite::Result<Vec<AiTuningSet>> = rows.collect();
    let items: Vec<Value> = sets.map_err(server_error)?.iter().map(summary).collect();
    Ok(json!({ "items": items }).into())
}

fn read_engine_state(conn: &Connection, key: &str) -> Result<Option<String>, String> {
    match conn.query_row(
        "SELECT value FROM engine_state WHERE key=?1",
        [key],
        |row| row.get::<_, String>(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(format!("读取 engine_state.{key} 失败: {error}")),
    }
}

fn active_entry(regime: &str, set: Option<AiTuningSet>) -> Value {
    match set {
        Some(set) => json!({
            "regime": regime,
            "set_id": set.id,
            "name": set.name,
            "timeframe": set.timeframe,
            "params": set.params,
        }),
        None => json!({
            "regime": regime,
            "set_id": Value::Null,
            "name": Value::Null,
            "timeframe": Value::Null,
            "params": Value::Null,
        }),
    }
}

/// 守卫状态按 regime 存 JSON；解析失败视为缺失，不阻断当前视图。
fn guard_view(conn: &Connection) -> Result<Value, ApiError> {
    let mut guard = serde_json::Map::new();
    for regime in ACTIVE_REGIMES {
        let key = format!("ai_tuning.guard.{regime}");
        if let Some(raw) = read_engine_state(conn, &key).map_err(server_error)? {
            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                guard.insert(regime.to_string(), parsed);
            }
        }
    }
    Ok(Value::Object(guard))
}

fn auto_apply_since(conn: &Connection) -> Result<i64, ApiError> {
    Ok(read_engine_state(conn, "ai_tuning.auto_apply_since")
        .map_err(server_error)?
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .unwrap_or(0))
}

/// 当前视图：`market_regime` 只读诊断键，不触发任何切换；active 缺失按 null 呈现。
async fn current_view(State(db): State<Db>) -> Result<Json<Value>, ApiError> {
    let conn = lock(&db)?;
    let mode = read_engine_state(&conn, "mode")
        .map_err(server_error)?
        .filter(|value| !value.is_empty());
    let market_regime = match mode {
        Some(mode) => read_engine_state(&conn, &format!("market_regime.{mode}"))
            .map_err(server_error)?
            .filter(|value| !value.is_empty())
            .map(Value::String)
            .unwrap_or(Value::Null),
        None => Value::Null,
    };
    let mut active = Vec::with_capacity(ACTIVE_REGIMES.len());
    for regime in ACTIVE_REGIMES {
        let set = active_for_regime(&conn, regime).map_err(server_error)?;
        active.push(active_entry(regime, set));
    }
    Ok(json!({
        "market_regime": market_regime,
        "active": active,
        "guard": guard_view(&conn)?,
        "auto_apply_since": auto_apply_since(&conn)?,
    })
    .into())
}

/// 手动启用只允许当前 regime：跨 regime 切换交给自动流程，避免人工绕过 regime 状态机。
async fn activate_set(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    let conn = lock(&db)?;
    ensure_set_exists(&conn, id)?;
    let mode = current_mode(&conn);
    let regime = current_regime(&conn, &mode);
    let set = load_set(&conn, id).map_err(server_error)?;
    if set.regime != regime {
        return Err(bad_request(format!(
            "只能启用当前 regime 的AI 调参；非当前 regime 由自动切换处理（AI 调参 {} 属于 {}，当前为 {}）",
            id, set.regime, regime
        )));
    }
    activate(&conn, id, "manual", &mode).map_err(bad_request)?;
    Ok(json!({ "ok": true, "id": id }).into())
}

/// 手动回滚当前 regime 的 active 集，审计来源由 `rollback` 固定为 `parameter_set_rollback`。
/// 跨 regime 回滚会绕过 regime 状态机，因此在写入任何状态前直接拒绝。
async fn rollback_set(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    let conn = lock(&db)?;
    ensure_set_exists(&conn, id)?;
    let mode = current_mode(&conn);
    let regime = current_regime(&conn, &mode);
    let set = load_set(&conn, id).map_err(server_error)?;
    if set.regime != regime {
        return Err(bad_request(format!(
            "只能回滚当前 regime 的AI 调参；跨 regime 回滚由自动切换处理（AI 调参 {} 属于 {}，当前为 {}）",
            id, set.regime, regime
        )));
    }
    rollback(&conn, id, &mode).map_err(bad_request)?;
    Ok(json!({ "ok": true, "id": id }).into())
}

async fn set_evidence(State(db): State<Db>, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    let conn = lock(&db)?;
    let raw = conn.query_row(
        "SELECT evidence FROM ai_tuning_sets WHERE id=?1",
        [id],
        |row| row.get::<_, String>(0),
    );
    match raw {
        Ok(raw) => {
            let evidence = serde_json::from_str(&raw).unwrap_or(Value::Null);
            Ok(json!({ "id": id, "evidence": evidence }).into())
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(not_found(format!("AI 调参 {id} 不存在"))),
        Err(error) => Err(server_error(format!("读取AI 调参 {id} 证据失败: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn memory_db() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        Db {
            path: ":memory:".to_string(),
            conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        }
    }

    fn insert_set(db: &Db, name: &str, regime: &str, status: &str, evidence: &str) -> i64 {
        let conn = db.lock();
        let activated_at = if status == "active" {
            Some(1000i64)
        } else {
            None
        };
        conn.execute(
            "INSERT INTO ai_tuning_sets(name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at) \
             VALUES(?1,?2,'1h',1,'{\"timeframe\":\"1h\"}',?3,'bootstrap',?4,NULL,1000,?5)",
            params![name, regime, status, evidence, activated_at],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn set_engine_state(db: &Db, key: &str, value: &str) {
        let conn = db.lock();
        conn.execute(
            "INSERT INTO engine_state(key,value,updated_at) VALUES(?1,?2,0) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )
        .unwrap();
    }

    fn seed_strategy(db: &Db, params: Value) {
        let conn = db.lock();
        conn.execute(
            "INSERT INTO app_settings(key,value,updated_at) VALUES('strategy',?1,0) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [params.to_string()],
        )
        .unwrap();
    }

    /// 插入指定 params 快照，供激活/回滚测试构造差异。
    fn insert_set_with_params(
        db: &Db,
        name: &str,
        regime: &str,
        status: &str,
        params: Value,
    ) -> i64 {
        let conn = db.lock();
        conn.execute(
            "INSERT INTO ai_tuning_sets(name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at) \
             VALUES(?1,?2,'1h',1,?3,?4,'manual','{\"kind\":\"manual\"}',NULL,1000,NULL)",
            params![name, regime, params.to_string(), status],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn strategy_value(db: &Db) -> Value {
        let conn = db.lock();
        let raw: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='strategy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    async fn post(app: Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn params_with_threshold(threshold: f64) -> Value {
        json!({
            "timeframe": "1h",
            "atr_pct_min": 1.2,
            "atr_pct_max": 8,
            "volume_ratio_min": 1.1,
            "entry_threshold": threshold,
        })
    }

    fn count_changes(db: &Db, source: &str, set_id: i64) -> i64 {
        let conn = db.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM parameter_changes WHERE source=?1 AND set_id=?2",
            params![source, set_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn count_activations(db: &Db, trigger: &str, set_id: i64) -> i64 {
        let conn = db.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM ai_tuning_activations WHERE trigger=?1 AND set_id=?2",
            params![trigger, set_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn app(db: Db) -> Router {
        Router::new().nest("/api/v1", routes()).with_state(db)
    }

    #[tokio::test]
    async fn list_returns_items_newest_first_with_evidence_json() {
        let db = memory_db();
        let range_id = insert_set(
            &db,
            "range-bootstrap",
            "range",
            "active",
            "{\"kind\":\"bootstrap\"}",
        );
        let chaos_id = insert_set(
            &db,
            "chaos-bootstrap",
            "chaos",
            "active",
            "{\"kind\":\"bootstrap\"}",
        );
        let candidate_id = insert_set(
            &db,
            "range-ai-1",
            "range",
            "candidate",
            "{\"kind\":\"ai\",\"gate\":{\"pass\":true}}",
        );

        let (status, body) = get(app(db), "/api/v1/ai-tuning").await;
        assert_eq!(status, StatusCode::OK, "列表应成功: {body}");
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["id"], candidate_id, "按 id DESC 排序");
        assert_eq!(items[1]["id"], chaos_id);
        assert_eq!(items[2]["id"], range_id);
        assert_eq!(items[2]["regime"], "range");
        assert_eq!(items[2]["timeframe"], "1h");
        assert_eq!(items[2]["status"], "active");
        assert_eq!(items[2]["source"], "bootstrap");
        assert_eq!(items[2]["evidence"], json!({ "kind": "bootstrap" }));
        assert_eq!(
            items[0]["evidence"],
            json!({ "kind": "ai", "gate": { "pass": true } })
        );
        assert!(items[0]["params"].is_null(), "列表不应回传完整参数快照");
    }

    #[tokio::test]
    async fn list_filters_by_regime_and_status() {
        let db = memory_db();
        insert_set(
            &db,
            "range-bootstrap",
            "range",
            "active",
            "{\"kind\":\"bootstrap\"}",
        );
        let chaos_id = insert_set(
            &db,
            "chaos-bootstrap",
            "chaos",
            "active",
            "{\"kind\":\"bootstrap\"}",
        );
        insert_set(&db, "range-ai-1", "range", "candidate", "{\"kind\":\"ai\"}");

        let (status, body) = get(app(db), "/api/v1/ai-tuning?regime=chaos&status=active").await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], chaos_id);
        assert_eq!(items[0]["regime"], "chaos");
    }

    #[tokio::test]
    async fn current_reads_market_regime_and_nulls_missing_trend() {
        let db = memory_db();
        let range_id = insert_set(
            &db,
            "range-bootstrap",
            "range",
            "active",
            "{\"kind\":\"bootstrap\"}",
        );
        let chaos_id = insert_set(
            &db,
            "chaos-bootstrap",
            "chaos",
            "active",
            "{\"kind\":\"bootstrap\"}",
        );
        set_engine_state(&db, "mode", "demo");
        set_engine_state(&db, "market_regime.demo", "range");

        let (status, body) = get(app(db), "/api/v1/ai-tuning/current").await;
        assert_eq!(status, StatusCode::OK, "当前视图应成功: {body}");
        assert_eq!(body["market_regime"], "range");
        let active = body["active"].as_array().unwrap();
        assert_eq!(active.len(), 3, "trend/range/chaos 各一项");

        let trend = active
            .iter()
            .find(|item| item["regime"] == "trend")
            .unwrap();
        assert!(trend["set_id"].is_null(), "trend 无 active 应为 null");
        assert!(trend["name"].is_null());
        assert!(trend["timeframe"].is_null());
        assert!(trend["params"].is_null());

        let range = active
            .iter()
            .find(|item| item["regime"] == "range")
            .unwrap();
        assert_eq!(range["set_id"], range_id);
        assert_eq!(range["name"], "range-bootstrap");
        assert_eq!(range["timeframe"], "1h");
        assert_eq!(range["params"]["timeframe"], "1h");

        let chaos = active
            .iter()
            .find(|item| item["regime"] == "chaos")
            .unwrap();
        assert_eq!(chaos["set_id"], chaos_id);
    }

    #[tokio::test]
    async fn current_without_market_regime_returns_null() {
        let db = memory_db();
        insert_set(
            &db,
            "range-bootstrap",
            "range",
            "active",
            "{\"kind\":\"bootstrap\"}",
        );
        set_engine_state(&db, "mode", "demo");

        let (status, body) = get(app(db), "/api/v1/ai-tuning/current").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["market_regime"].is_null(), "诊断键缺失应为 null");
    }

    #[tokio::test]
    async fn evidence_returns_full_json_and_missing_is_404() {
        let db = memory_db();
        let id = insert_set(
            &db,
            "range-ai-1",
            "range",
            "approved",
            "{\"kind\":\"ai\",\"gate\":{\"pass\":true,\"reasons\":[]}}",
        );

        let (status, body) =
            get(app(db.clone()), &format!("/api/v1/ai-tuning/{id}/evidence")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], id);
        assert_eq!(
            body["evidence"],
            json!({ "kind": "ai", "gate": { "pass": true, "reasons": [] } })
        );

        let (missing_status, missing_body) =
            get(app(db), "/api/v1/ai-tuning/999999/evidence").await;
        assert_eq!(missing_status, StatusCode::NOT_FOUND);
        assert_eq!(missing_body["detail"], "AI 调参 999999 不存在");
    }

    #[tokio::test]
    async fn current_includes_guard_and_auto_apply_since() {
        let db = memory_db();
        insert_set(
            &db,
            "range-bootstrap",
            "range",
            "active",
            "{\"kind\":\"bootstrap\"}",
        );
        set_engine_state(&db, "mode", "demo");
        set_engine_state(&db, "market_regime.demo", "range");
        set_engine_state(
            &db,
            "ai_tuning.guard.range",
            "{\"set_id\":1,\"last_candidate_at\":123}",
        );
        set_engine_state(&db, "ai_tuning.auto_apply_since", "1700000000");

        let (status, body) = get(app(db), "/api/v1/ai-tuning/current").await;
        assert_eq!(status, StatusCode::OK, "当前视图应成功: {body}");
        assert_eq!(body["guard"]["range"]["set_id"], 1);
        assert_eq!(body["guard"]["range"]["last_candidate_at"], 123);
        assert_eq!(body["auto_apply_since"], 1_700_000_000);
    }

    #[tokio::test]
    async fn activate_route_materializes_strategy_and_audits_manual() {
        let db = memory_db();
        seed_strategy(&db, params_with_threshold(0.55));
        set_engine_state(&db, "mode", "demo");
        let id = insert_set_with_params(
            &db,
            "range-manual",
            "range",
            "manual",
            params_with_threshold(0.50),
        );

        let (status, body) =
            post(app(db.clone()), &format!("/api/v1/ai-tuning/{id}/activate")).await;
        assert_eq!(status, StatusCode::OK, "手动启用应成功: {body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["id"], id);
        assert_eq!(strategy_value(&db)["entry_threshold"], 0.50);
        assert!(
            count_changes(&db, "ai_tuning", id) > 0,
            "应写AI 调参切换审计"
        );
        assert_eq!(
            count_activations(&db, "manual", id),
            1,
            "trigger 应为 manual"
        );
    }

    #[tokio::test]
    async fn rollback_route_restores_previous_and_audits_rollback() {
        let db = memory_db();
        seed_strategy(&db, params_with_threshold(0.55));
        set_engine_state(&db, "mode", "demo");
        set_engine_state(&db, "market_regime.demo", "range");
        let first = insert_set_with_params(
            &db,
            "range-first",
            "range",
            "manual",
            params_with_threshold(0.50),
        );
        let second = insert_set_with_params(
            &db,
            "range-second",
            "range",
            "manual",
            params_with_threshold(0.52),
        );

        let (activate_first, _) = post(
            app(db.clone()),
            &format!("/api/v1/ai-tuning/{first}/activate"),
        )
        .await;
        assert_eq!(activate_first, StatusCode::OK);
        let (activate_second, activate_body) = post(
            app(db.clone()),
            &format!("/api/v1/ai-tuning/{second}/activate"),
        )
        .await;
        assert_eq!(
            activate_second,
            StatusCode::OK,
            "第二套应可启用: {activate_body}"
        );
        assert_eq!(strategy_value(&db)["entry_threshold"], 0.52);

        let (status, body) = post(
            app(db.clone()),
            &format!("/api/v1/ai-tuning/{second}/rollback"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "手动回滚应成功: {body}");
        assert_eq!(
            strategy_value(&db)["entry_threshold"],
            0.50,
            "应恢复上一套参数"
        );
        assert!(
            count_changes(&db, "ai_tuning_rollback", first) > 0,
            "应写回滚审计"
        );
        assert_eq!(
            count_activations(&db, "rollback", first),
            1,
            "trigger 应为 rollback"
        );

        let conn = db.lock();
        let statuses: Vec<(i64, String)> = conn
            .prepare("SELECT id,status FROM ai_tuning_sets ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(
            statuses,
            vec![
                (first, "active".to_string()),
                (second, "retired".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn activate_and_rollback_return_404_or_400() {
        let db = memory_db();
        seed_strategy(&db, params_with_threshold(0.55));
        set_engine_state(&db, "mode", "demo");
        let pending = insert_set_with_params(
            &db,
            "range-candidate",
            "range",
            "candidate",
            params_with_threshold(0.5),
        );
        let no_history = insert_set_with_params(
            &db,
            "range-manual",
            "range",
            "active",
            params_with_threshold(0.5),
        );

        let (missing_activate, missing_activate_body) =
            post(app(db.clone()), "/api/v1/ai-tuning/999999/activate").await;
        assert_eq!(missing_activate, StatusCode::NOT_FOUND);
        assert_eq!(missing_activate_body["detail"], "AI 调参 999999 不存在");

        let (missing_rollback, _) =
            post(app(db.clone()), "/api/v1/ai-tuning/999999/rollback").await;
        assert_eq!(missing_rollback, StatusCode::NOT_FOUND);

        let (bad_status, bad_body) = post(
            app(db.clone()),
            &format!("/api/v1/ai-tuning/{pending}/activate"),
        )
        .await;
        assert_eq!(bad_status, StatusCode::BAD_REQUEST);
        assert!(
            bad_body["detail"]
                .as_str()
                .unwrap_or("")
                .contains("不允许激活"),
            "候选状态不可手动启用: {bad_body}"
        );

        let (no_previous, no_previous_body) =
            post(app(db), &format!("/api/v1/ai-tuning/{no_history}/rollback")).await;
        assert_eq!(no_previous, StatusCode::BAD_REQUEST);
        assert!(
            no_previous_body["detail"]
                .as_str()
                .unwrap_or("")
                .contains("没有上一套"),
            "无历史不可回滚: {no_previous_body}"
        );
    }

    #[tokio::test]
    async fn activate_rejects_non_current_regime_without_writes() {
        let db = memory_db();
        seed_strategy(&db, params_with_threshold(0.55));
        set_engine_state(&db, "mode", "demo");
        set_engine_state(&db, "market_regime.demo", "range");
        let trend_id = insert_set_with_params(
            &db,
            "trend-manual",
            "trend",
            "manual",
            params_with_threshold(0.50),
        );
        let before = strategy_value(&db);

        let (status, body) = post(
            app(db.clone()),
            &format!("/api/v1/ai-tuning/{trend_id}/activate"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "跨 regime 应拒绝: {body}");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or("")
                .contains("只能启用当前 regime 的AI 调参"),
            "文案应说明仅当前 regime 可启用: {body}"
        );
        assert_eq!(strategy_value(&db), before, "拒绝时不得物化 strategy");
        assert_eq!(
            count_changes(&db, "ai_tuning", trend_id),
            0,
            "不得写切换审计"
        );
        assert_eq!(
            count_activations(&db, "manual", trend_id),
            0,
            "不得写激活记录"
        );
    }

    #[tokio::test]
    async fn rollback_rejects_non_active_set_without_writes() {
        let db = memory_db();
        seed_strategy(&db, params_with_threshold(0.55));
        set_engine_state(&db, "mode", "demo");
        let retired = insert_set_with_params(
            &db,
            "range-retired",
            "range",
            "retired",
            params_with_threshold(0.50),
        );
        let approved = insert_set_with_params(
            &db,
            "range-approved",
            "range",
            "approved",
            params_with_threshold(0.50),
        );
        let before = strategy_value(&db);

        for id in [retired, approved] {
            let (status, body) =
                post(app(db.clone()), &format!("/api/v1/ai-tuning/{id}/rollback")).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "非 active 不可回滚: {body}"
            );
            assert!(
                body["detail"]
                    .as_str()
                    .unwrap_or("")
                    .contains("只能回滚当前 active"),
                "文案应说明只能回滚 active: {body}"
            );
            assert_eq!(count_changes(&db, "ai_tuning_rollback", id), 0);
            assert_eq!(count_activations(&db, "rollback", id), 0);
        }
        assert_eq!(strategy_value(&db), before, "拒绝时不得改动 strategy");
    }

    #[tokio::test]
    async fn rollback_rejects_non_current_regime_active_without_writes() {
        let db = memory_db();
        seed_strategy(&db, params_with_threshold(0.55));
        set_engine_state(&db, "mode", "demo");
        set_engine_state(&db, "market_regime.demo", "range");
        let chaos_active = insert_set_with_params(
            &db,
            "chaos-active",
            "chaos",
            "active",
            params_with_threshold(0.50),
        );
        let before = strategy_value(&db);

        let (status, body) = post(
            app(db.clone()),
            &format!("/api/v1/ai-tuning/{chaos_active}/rollback"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "跨 regime 回滚应拒绝: {body}"
        );
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or("")
                .contains("只能回滚当前 regime 的AI 调参"),
            "文案应说明仅当前 regime 可回滚: {body}"
        );
        assert_eq!(strategy_value(&db), before, "拒绝时不得物化 strategy");
        assert_eq!(
            count_changes(&db, "ai_tuning_rollback", chaos_active),
            0,
            "不得写回滚审计"
        );
        assert_eq!(
            count_activations(&db, "rollback", chaos_active),
            0,
            "不得写激活记录"
        );
    }
}
