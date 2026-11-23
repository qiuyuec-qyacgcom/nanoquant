//! 参数集只读接口。B 期仅暴露列表、当前视图与完整证据；
//! 启用/回滚属于 C 期，这里不改 `strategy` 也不写任何激活记录。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::Db;
use crate::parameter_sets::{active_for_regime, row_to_set, ParameterSet};

const SET_COLUMNS: &str = "id,name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at";
const LIST_LIMIT: i64 = 100;
const ACTIVE_REGIMES: [&str; 3] = ["trend", "range", "chaos"];

pub fn routes() -> Router<Db> {
    Router::new()
        .route("/parameter-sets", get(list_sets))
        .route("/parameter-sets/current", get(current_view))
        .route("/parameter-sets/{id}/evidence", get(set_evidence))
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

fn lock(db: &Db) -> Result<std::sync::MutexGuard<'_, Connection>, ApiError> {
    db.conn
        .lock()
        .map_err(|_| server_error("数据库锁不可用"))
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    regime: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// 列表只返回摘要字段，完整证据走单独的 evidence 接口。
fn summary(set: &ParameterSet) -> Value {
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
        "SELECT {SET_COLUMNS} FROM parameter_sets \
         WHERE (?1 IS NULL OR regime=?1) AND (?2 IS NULL OR status=?2) \
         ORDER BY id DESC LIMIT {LIST_LIMIT}"
    );
    let mut stmt = conn.prepare(&sql).map_err(server_error)?;
    let rows = stmt
        .query_map(params![regime.as_deref(), status.as_deref()], row_to_set)
        .map_err(server_error)?;
    let sets: rusqlite::Result<Vec<ParameterSet>> = rows.collect();
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

fn active_entry(regime: &str, set: Option<ParameterSet>) -> Value {
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
    })
    .into())
}

async fn set_evidence(
    State(db): State<Db>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let conn = lock(&db)?;
    let raw = conn.query_row(
        "SELECT evidence FROM parameter_sets WHERE id=?1",
        [id],
        |row| row.get::<_, String>(0),
    );
    match raw {
        Ok(raw) => {
            let evidence = serde_json::from_str(&raw).unwrap_or(Value::Null);
            Ok(json!({ "id": id, "evidence": evidence }).into())
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Err(not_found(format!("参数集 {id} 不存在"))),
        Err(error) => Err(server_error(format!("读取参数集 {id} 证据失败: {error}"))),
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
        let conn = db.conn.lock().unwrap();
        let activated_at = if status == "active" { Some(1000i64) } else { None };
        conn.execute(
            "INSERT INTO parameter_sets(name,regime,timeframe,version,params,status,source,evidence,parent_id,created_at,activated_at) \
             VALUES(?1,?2,'1h',1,'{\"timeframe\":\"1h\"}',?3,'bootstrap',?4,NULL,1000,?5)",
            params![name, regime, status, evidence, activated_at],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn set_engine_state(db: &Db, key: &str, value: &str) {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO engine_state(key,value,updated_at) VALUES(?1,?2,0) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )
        .unwrap();
    }

    async fn get(app: Router, uri: &str) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
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
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn app(db: Db) -> Router {
        Router::new().nest("/api/v1", routes()).with_state(db)
    }

    #[tokio::test]
    async fn list_returns_items_newest_first_with_evidence_json() {
        let db = memory_db();
        let range_id = insert_set(&db, "range-bootstrap", "range", "active", "{\"kind\":\"bootstrap\"}");
        let chaos_id = insert_set(&db, "chaos-bootstrap", "chaos", "active", "{\"kind\":\"bootstrap\"}");
        let candidate_id = insert_set(
            &db,
            "range-ai-1",
            "range",
            "candidate",
            "{\"kind\":\"ai\",\"gate\":{\"pass\":true}}",
        );

        let (status, body) = get(app(db), "/api/v1/parameter-sets").await;
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
        insert_set(&db, "range-bootstrap", "range", "active", "{\"kind\":\"bootstrap\"}");
        let chaos_id = insert_set(&db, "chaos-bootstrap", "chaos", "active", "{\"kind\":\"bootstrap\"}");
        insert_set(
            &db,
            "range-ai-1",
            "range",
            "candidate",
            "{\"kind\":\"ai\"}",
        );

        let (status, body) = get(app(db), "/api/v1/parameter-sets?regime=chaos&status=active").await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], chaos_id);
        assert_eq!(items[0]["regime"], "chaos");
    }

    #[tokio::test]
    async fn current_reads_market_regime_and_nulls_missing_trend() {
        let db = memory_db();
        let range_id = insert_set(&db, "range-bootstrap", "range", "active", "{\"kind\":\"bootstrap\"}");
        let chaos_id = insert_set(&db, "chaos-bootstrap", "chaos", "active", "{\"kind\":\"bootstrap\"}");
        set_engine_state(&db, "mode", "demo");
        set_engine_state(&db, "market_regime.demo", "range");

        let (status, body) = get(app(db), "/api/v1/parameter-sets/current").await;
        assert_eq!(status, StatusCode::OK, "当前视图应成功: {body}");
        assert_eq!(body["market_regime"], "range");
        let active = body["active"].as_array().unwrap();
        assert_eq!(active.len(), 3, "trend/range/chaos 各一项");

        let trend = active.iter().find(|item| item["regime"] == "trend").unwrap();
        assert!(trend["set_id"].is_null(), "trend 无 active 应为 null");
        assert!(trend["name"].is_null());
        assert!(trend["timeframe"].is_null());
        assert!(trend["params"].is_null());

        let range = active.iter().find(|item| item["regime"] == "range").unwrap();
        assert_eq!(range["set_id"], range_id);
        assert_eq!(range["name"], "range-bootstrap");
        assert_eq!(range["timeframe"], "1h");
        assert_eq!(range["params"]["timeframe"], "1h");

        let chaos = active.iter().find(|item| item["regime"] == "chaos").unwrap();
        assert_eq!(chaos["set_id"], chaos_id);
    }

    #[tokio::test]
    async fn current_without_market_regime_returns_null() {
        let db = memory_db();
        insert_set(&db, "range-bootstrap", "range", "active", "{\"kind\":\"bootstrap\"}");
        set_engine_state(&db, "mode", "demo");

        let (status, body) = get(app(db), "/api/v1/parameter-sets/current").await;
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

        let (status, body) = get(app(db.clone()), &format!("/api/v1/parameter-sets/{id}/evidence")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], id);
        assert_eq!(
            body["evidence"],
            json!({ "kind": "ai", "gate": { "pass": true, "reasons": [] } })
        );

        let (missing_status, missing_body) =
            get(app(db), "/api/v1/parameter-sets/999999/evidence").await;
        assert_eq!(missing_status, StatusCode::NOT_FOUND);
        assert_eq!(missing_body["detail"], "参数集 999999 不存在");
    }
}