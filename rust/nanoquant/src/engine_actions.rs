use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::app_state::AppState;
use crate::runtime::{CloseError, ProtectError, TradeModeError};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/engine/cycle-now", post(cycle_now))
        .route("/engine/flatten", post(flatten))
        .route(
            "/engine/positions/{position_id}/close",
            post(close_position),
        )
        .route(
            "/engine/trade-mode",
            get(trade_mode).post(switch_trade_mode),
        )
        .route(
            "/engine/positions/{position_id}/protect",
            post(protect_position),
        )
}

#[derive(Deserialize)]
struct TradeModeIn {
    #[serde(default = "default_true")]
    hedge: bool,
    #[serde(default = "default_true")]
    isolated: bool,
}

#[derive(Deserialize)]
struct FlattenIn {
    confirm: String,
    #[serde(default = "default_true")]
    stop_engine: bool,
}

fn default_true() -> bool {
    true
}

async fn trade_mode(State(state): State<AppState>) -> impl IntoResponse {
    match state.engine_control.trade_mode_status().await {
        Ok(value) => (StatusCode::OK, Json(value)).into_response(),
        Err(error) => trade_mode_error(error).into_response(),
    }
}

async fn switch_trade_mode(
    State(state): State<AppState>,
    Json(body): Json<TradeModeIn>,
) -> impl IntoResponse {
    match state
        .engine_control
        .switch_trade_mode(body.hedge, body.isolated)
        .await
    {
        Ok(value) => {
            state
                .events
                .record("info", "engine", "切换交易模式", value.clone());
            (StatusCode::OK, Json(value)).into_response()
        }
        Err(error) => {
            let (status, body) = trade_mode_error(error);
            state
                .events
                .record("warn", "engine", "切换交易模式失败", body.0.clone());
            (status, body).into_response()
        }
    }
}

fn trade_mode_error(error: TradeModeError) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        TradeModeError::Credentials(detail) => {
            (StatusCode::BAD_REQUEST, Json(json!({"detail": detail})))
        }
        TradeModeError::Positions(symbols) => (
            StatusCode::CONFLICT,
            Json(json!({"detail": "账户仍有持仓，无法切换交易模式", "positions": symbols})),
        ),
        TradeModeError::Exchange(detail) => {
            (StatusCode::BAD_GATEWAY, Json(json!({"detail": detail})))
        }
    }
}

async fn protect_position(
    State(state): State<AppState>,
    Path(position_id): Path<i64>,
) -> impl IntoResponse {
    match state.engine_control.protect_position(position_id).await {
        Ok(value) => {
            state
                .events
                .record("info", "trade", "手动补挂保护单", value.clone());
            (StatusCode::OK, Json(value)).into_response()
        }
        Err(error) => {
            let (status, body) = protect_error(error);
            state
                .events
                .record("warn", "trade", "补挂保护单失败", body.0.clone());
            (status, body).into_response()
        }
    }
}

fn protect_error(error: ProtectError) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        ProtectError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": "仓位不存在或已关闭"})),
        ),
        ProtectError::Credentials(detail) => {
            (StatusCode::BAD_REQUEST, Json(json!({"detail": detail})))
        }
        ProtectError::Exchange(detail) => {
            (StatusCode::BAD_GATEWAY, Json(json!({"detail": detail})))
        }
        ProtectError::Failed(value) => (StatusCode::BAD_REQUEST, Json(value)),
    }
}

async fn flatten(State(state): State<AppState>, Json(body): Json<FlattenIn>) -> impl IntoResponse {
    if body.confirm != "FLATTEN" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "需要 confirm=FLATTEN"})),
        )
            .into_response();
    }
    match state.engine_control.flatten(body.stop_engine).await {
        Ok(value) => {
            state
                .events
                .record("warn", "engine", "紧急全平", value.clone());
            (StatusCode::OK, Json(value)).into_response()
        }
        Err(error) => {
            state
                .events
                .record("error", "engine", "紧急全平失败", json!({"detail": error}));
            (StatusCode::BAD_GATEWAY, Json(json!({"detail": error}))).into_response()
        }
    }
}

async fn close_position(
    State(state): State<AppState>,
    Path(position_id): Path<i64>,
) -> impl IntoResponse {
    match state
        .engine_control
        .close_position(position_id, "manual")
        .await
    {
        Ok(value) => {
            state
                .events
                .record("warn", "trade", "手动平仓", value.clone());
            (StatusCode::OK, Json(value)).into_response()
        }
        Err(error) => {
            let (status, body) = close_error(error);
            state
                .events
                .record("warn", "trade", "手动平仓失败", body.0.clone());
            (status, body).into_response()
        }
    }
}

fn close_error(error: CloseError) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        CloseError::NotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": "仓位不存在或已关闭"})),
        ),
        CloseError::Credentials(detail) => {
            (StatusCode::BAD_REQUEST, Json(json!({"detail": detail})))
        }
        CloseError::Exchange(detail) => (StatusCode::BAD_GATEWAY, Json(json!({"detail": detail}))),
        CloseError::Rejected(detail) => (StatusCode::CONFLICT, Json(json!({"detail": detail}))),
        CloseError::Unknown(detail) => (StatusCode::BAD_GATEWAY, Json(json!({"detail": detail}))),
    }
}

async fn cycle_now(State(state): State<AppState>) -> impl IntoResponse {
    match state.engine_control.trigger_cycle() {
        Ok(requested_at) => (
            StatusCode::ACCEPTED,
            Json(json!({"ok": true, "status": "queued", "requested_at": requested_at})),
        )
            .into_response(),
        Err(error) => {
            let status = if error.contains("凭证") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::CONFLICT
            };
            (status, Json(json!({"detail": error}))).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tokio::sync::Notify;
    use tower::ServiceExt;

    fn state(active: bool, busy: bool) -> AppState {
        let db = crate::db::Db {
            path: String::new(),
            conn: Arc::new(std::sync::Mutex::new(
                rusqlite::Connection::open_in_memory().unwrap(),
            )),
        };
        let mut state = crate::test_state(None);
        state.engine_control = crate::runtime::EngineControl::with_cycle(
            db,
            vec![17; 32],
            Some(Arc::new(Notify::new())),
            Arc::new(AtomicBool::new(busy)),
            Arc::new(AtomicBool::new(active)),
        );
        state
    }

    async fn post_cycle(state: AppState) -> (StatusCode, Value) {
        let response = routes()
            .with_state(state)
            .oneshot(
                Request::post("/engine/cycle-now")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn cycle_now_refuses_when_the_runtime_is_not_active() {
        let (status, body) = post_cycle(crate::test_state(None)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body["detail"].as_str().unwrap().contains("引擎未运行"));
    }

    #[tokio::test]
    async fn cycle_now_queues_when_the_runtime_is_active() {
        let (status, body) = post_cycle(state(true, false)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["status"], "queued");
    }

    #[tokio::test]
    async fn cycle_now_refuses_while_a_cycle_is_already_running() {
        let (status, body) = post_cycle(state(true, true)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body["detail"].as_str().unwrap().contains("周期正在进行"));
    }

    const KEY: &str = "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=";

    struct FakeGateway {
        replies: std::sync::Mutex<std::collections::VecDeque<(u16, String)>>,
        sent: std::sync::Mutex<Vec<String>>,
    }

    impl FakeGateway {
        fn new(replies: Vec<(u16, String)>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies.into()),
                sent: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl crate::live::Gateway for FakeGateway {
        fn send<'a>(
            &'a self,
            request: &'a crate::submit::PreparedRequest,
        ) -> crate::live::GatewayFuture<'a> {
            Box::pin(async move {
                self.sent.lock().unwrap().push(request.url.clone());
                let (status, body) = self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((500, "unexpected request".to_string()));
                crate::live::GatewayReply {
                    status,
                    body,
                    transport_failed: false,
                }
            })
        }
    }

    fn trade_db(open_positions: bool) -> crate::db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE positions(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, qty REAL, mark_price REAL, sl_price REAL, tp_price REAL, status TEXT, mode TEXT); \
             CREATE TABLE trading_symbols(symbol TEXT PRIMARY KEY, enabled INTEGER, sort_order INTEGER); \
             CREATE TABLE engine_events(id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER, level TEXT, category TEXT, message TEXT, payload TEXT); \
             INSERT INTO trading_symbols VALUES('BTCUSDT',1,1);",
        )
        .unwrap();
        let fernet = fernet::Fernet::new(KEY).unwrap();
        let data = serde_json::json!({
            "demo_api_key_enc": fernet.encrypt(b"api-key"),
            "demo_api_secret_enc": fernet.encrypt(b"secret"),
        });
        conn.execute(
            "INSERT INTO app_settings VALUES('data', ?1, 0)",
            [data.to_string()],
        )
        .unwrap();
        if open_positions {
            conn.execute(
                "INSERT INTO positions(symbol,status) VALUES('BTCUSDT','open')",
                [],
            )
            .unwrap();
        }
        crate::db::Db {
            path: String::new(),
            conn: Arc::new(std::sync::Mutex::new(conn)),
        }
    }

    fn trade_state(db: crate::db::Db, gateway: Arc<dyn crate::live::Gateway>) -> AppState {
        let mut state = crate::test_state(None);
        state.engine_control = crate::runtime::EngineControl::with_cycle(
            db,
            KEY.as_bytes().to_vec(),
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
        .with_gateway(gateway);
        state
    }

    async fn trade_request(
        state: AppState,
        method: &str,
        body: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri("/engine/trade-mode");
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let request = request
            .body(
                body.map(|text| Body::from(text.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap();
        let response = routes().with_state(state).oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn trade_mode_status_reports_hedge_and_can_switch() {
        let gateway = Arc::new(FakeGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#.to_string()),
            (200, "[]".to_string()),
        ]));
        let (status, body) =
            trade_request(trade_state(trade_db(false), gateway.clone()), "GET", None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["hedge_mode"], false);
        assert_eq!(body["need_hedge"], true);
        assert_eq!(body["can_switch"], true);
        assert_eq!(gateway.sent.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn switch_trade_mode_refuses_when_positions_are_open() {
        let gateway = Arc::new(FakeGateway::new(vec![]));
        let (status, body) = trade_request(
            trade_state(trade_db(true), gateway.clone()),
            "POST",
            Some(r#"{"hedge":true,"isolated":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body["positions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == "BTCUSDT"));
        assert_eq!(gateway.sent.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn switch_trade_mode_switches_hedge_and_isolated() {
        let gateway = Arc::new(FakeGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#.to_string()),
            (200, "[]".to_string()),
            (200, "{}".to_string()),
            (200, "{}".to_string()),
        ]));
        let (status, body) = trade_request(
            trade_state(trade_db(false), gateway.clone()),
            "POST",
            Some(r#"{"hedge":true,"isolated":true}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["results"].as_array().unwrap().len(), 2);
        assert_eq!(gateway.sent.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn trade_mode_status_returns_502_when_exchange_fails() {
        let gateway = Arc::new(FakeGateway::new(vec![(500, "boom".to_string())]));
        let (status, body) =
            trade_request(trade_state(trade_db(false), gateway), "GET", None).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(body["detail"].as_str().unwrap().contains("500"));
    }

    async fn protect_request(state: AppState, position_id: i64) -> (StatusCode, Value) {
        let response = routes()
            .with_state(state)
            .oneshot(
                Request::post(format!("/engine/positions/{position_id}/protect"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    fn seed_protect_position(db: &crate::db::Db) {
        db.lock()
            .execute(
                "INSERT INTO positions(id,symbol,side,qty,mark_price,sl_price,tp_price,status,mode) \
                 VALUES(1,'BTCUSDT','long',1,100,95,110,'open','demo')",
                [],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn protect_position_places_both_legs() {
        let db = trade_db(false);
        seed_protect_position(&db);
        let gateway = Arc::new(FakeGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#.to_string()),
            (200, r#"{"algoId":1}"#.to_string()),
            (200, r#"{"algoId":2}"#.to_string()),
        ]));
        let (status, body) = protect_request(trade_state(db, gateway.clone()), 1).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["results"].as_array().unwrap().len(), 2);
        assert_eq!(gateway.sent.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn protect_position_reports_unknown_without_retry() {
        let db = trade_db(false);
        seed_protect_position(&db);
        let gateway = Arc::new(FakeGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#.to_string()),
            (500, "gateway timeout".to_string()),
            (500, "gateway timeout".to_string()),
        ]));
        let (status, body) = protect_request(trade_state(db, gateway.clone()), 1).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["ok"], false);
        let results = body["results"].as_array().unwrap();
        assert!(results.iter().all(|item| item["unknown"] == true));
        assert_eq!(gateway.sent.lock().unwrap().len(), 3, "未知结果不能重发");
    }

    #[tokio::test]
    async fn protect_position_returns_404_for_missing_position() {
        let gateway = Arc::new(FakeGateway::new(vec![]));
        let (status, body) =
            protect_request(trade_state(trade_db(false), gateway.clone()), 9).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(gateway.sent.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn protect_position_reports_explicit_rejections_as_failures() {
        let db = trade_db(false);
        seed_protect_position(&db);
        let gateway = Arc::new(FakeGateway::new(vec![
            (200, r#"{"dualSidePosition":false}"#.to_string()),
            (
                400,
                r#"{"code":-2019,"msg":"Margin is insufficient."}"#.to_string(),
            ),
            (
                400,
                r#"{"code":-2019,"msg":"Margin is insufficient."}"#.to_string(),
            ),
        ]));
        let (status, body) = protect_request(trade_state(db, gateway), 1).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["ok"], false);
        let results = body["results"].as_array().unwrap();
        assert!(results.iter().all(|item| item["ok"] == false));
        assert!(results.iter().all(|item| item["unknown"] == false));
    }

    fn full_db() -> crate::db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn.execute_batch(
            "DROP TABLE trades; \
             CREATE TABLE trades(symbol TEXT, side TEXT, qty REAL, entry_price REAL, exit_price REAL, pnl REAL, opened_at INTEGER, closed_at INTEGER, hold_seconds INTEGER, exit_reason TEXT, mode TEXT); \
             INSERT INTO trading_symbols(symbol,name,tier,sl_atr_mult,enabled,max_leverage,precision,tick_sz,min_sz,min_notional,ct_val,min_score,note,sort_order,created_at,updated_at) \
             VALUES('BTCUSDT','BTCUSDT','C',1.0,1,5,2,'0.01','0.001',5.0,0.0,0.0,'',1,0,0); \
             INSERT INTO trading_symbols(symbol,name,tier,sl_atr_mult,enabled,max_leverage,precision,tick_sz,min_sz,min_notional,ct_val,min_score,note,sort_order,created_at,updated_at) \
             VALUES('ETHUSDT','ETHUSDT','C',1.0,1,5,2,'0.01','0.001',5.0,0.0,0.0,'',2,0,0);",
        )
        .unwrap();
        let fernet = fernet::Fernet::new(KEY).unwrap();
        let data = serde_json::json!({
            "demo_api_key_enc": fernet.encrypt(b"api-key"),
            "demo_api_secret_enc": fernet.encrypt(b"secret"),
            "live_api_key_enc": fernet.encrypt(b"live-key"),
            "live_api_secret_enc": fernet.encrypt(b"live-secret"),
        });
        conn.execute(
            "INSERT INTO app_settings VALUES('data', ?1, 0)",
            [data.to_string()],
        )
        .unwrap();
        crate::db::Db {
            path: String::new(),
            conn: Arc::new(std::sync::Mutex::new(conn)),
        }
    }

    fn insert_position(db: &crate::db::Db, id: i64, symbol: &str, mode: &str) {
        db.lock()
            .execute(
                "INSERT INTO positions(id,symbol,side,qty,entry_price,mark_price,leverage,margin,sl_price,tp_price,peak_price,initial_sl_price,signal_score,opened_at,closed_at,exit_price,exit_reason,realized_pnl,fees,status,mode,venue_unrealized,venue_ts,partial_taken_at,ai_confidence,ai_reason,entry_slippage_bps,trade_id) \
                 VALUES(?1,?2,'long',1,100,100,5,20,95,110,100,95,0.5,0,0,0,'',0,0,'open',?3,0,0,0,0,'',0,0)",
                rusqlite::params![id, symbol, mode],
            )
            .unwrap();
    }

    async fn close_request(state: AppState, id: i64) -> (StatusCode, Value) {
        let response = routes()
            .with_state(state)
            .oneshot(
                Request::post(format!("/engine/positions/{id}/close"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn flatten_request(state: AppState, body: &str) -> (StatusCode, Value) {
        let response = routes()
            .with_state(state)
            .oneshot(
                Request::post("/engine/flatten")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn close_position_cancels_orders_then_closes_demo_position() {
        let db = full_db();
        insert_position(&db, 1, "BTCUSDT", "demo");
        let gateway = Arc::new(FakeGateway::new(vec![
            (200, r#"[{"orderId":11}]"#.to_string()),
            (200, "{}".to_string()),
            (200, r#"{"orders":[{"algoId":22}]}"#.to_string()),
            (200, "{}".to_string()),
        ]));
        let (status, body) = close_request(trade_state(db.clone(), gateway), 1).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["cancelled"], 2);
        let closed: String = db
            .lock()
            .query_row("SELECT status FROM positions WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(closed, "closed");
    }

    #[tokio::test]
    async fn flatten_closes_two_demo_positions_and_stops_engine() {
        let db = full_db();
        insert_position(&db, 1, "BTCUSDT", "demo");
        insert_position(&db, 2, "ETHUSDT", "demo");
        let gateway = Arc::new(FakeGateway::new(vec![
            (200, "[]".to_string()),
            (200, "[]".to_string()),
            (200, "[]".to_string()),
            (200, "[]".to_string()),
        ]));
        let (status, body) = flatten_request(
            trade_state(db.clone(), gateway),
            r#"{"confirm":"FLATTEN","stop_engine":true}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["closed"].as_array().unwrap().len(), 2);
        let open: i64 = db
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM positions WHERE status='open'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(open, 0);
        let enabled: String = db
            .lock()
            .query_row(
                "SELECT value FROM engine_state WHERE key='enabled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(enabled, "false");
    }

    fn open_positions(db: &crate::db::Db, mode: &str) -> i64 {
        db.lock()
            .query_row(
                "SELECT COUNT(*) FROM positions WHERE mode=?1 AND status='open'",
                [mode],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn flatten_without_confirm_phrase_keeps_positions_open() {
        let db = full_db();
        insert_position(&db, 1, "BTCUSDT", "demo");
        let gateway = Arc::new(FakeGateway::new(vec![]));
        let (status, body) = flatten_request(
            trade_state(db.clone(), gateway.clone()),
            r#"{"confirm":"no"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["detail"].as_str().unwrap().contains("FLATTEN"));
        assert_eq!(open_positions(&db, "demo"), 1);
        assert_eq!(gateway.sent.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn flatten_closes_only_the_current_mode() {
        let db = full_db();
        insert_position(&db, 1, "BTCUSDT", "demo");
        insert_position(&db, 2, "ETHUSDT", "live");
        let gateway = Arc::new(FakeGateway::new(vec![
            (200, "[]".to_string()),
            (200, "[]".to_string()),
        ]));
        let (status, body) = flatten_request(
            trade_state(db.clone(), gateway),
            r#"{"confirm":"FLATTEN","stop_engine":false}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["closed"].as_array().unwrap().len(), 1);
        assert_eq!(open_positions(&db, "demo"), 0);
        assert_eq!(open_positions(&db, "live"), 1, "不能误平另一个环境");
    }

    #[tokio::test]
    async fn close_missing_position_returns_404() {
        let db = full_db();
        let gateway = Arc::new(FakeGateway::new(vec![]));
        let (status, body) = close_request(trade_state(db, gateway.clone()), 9).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(gateway.sent.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn close_position_leaves_other_rows_open() {
        let db = full_db();
        insert_position(&db, 1, "BTCUSDT", "demo");
        insert_position(&db, 2, "ETHUSDT", "demo");
        let gateway = Arc::new(FakeGateway::new(vec![
            (200, "[]".to_string()),
            (200, "[]".to_string()),
        ]));
        let (status, body) = close_request(trade_state(db.clone(), gateway), 1).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["symbol"], "BTCUSDT");
        assert_eq!(open_positions(&db, "demo"), 1);
        let other: String = db
            .lock()
            .query_row("SELECT status FROM positions WHERE id=2", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(other, "open");
    }
}
