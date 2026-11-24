use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::AppState;

#[derive(Deserialize)]
struct ProtectionQuery {
    symbol: Option<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/engine/protection", get(protection_orders))
        .route(
            "/engine/protection/{algo_id}",
            axum::routing::delete(cancel_protection),
        )
}

async fn protection_orders(
    State(state): State<AppState>,
    Query(query): Query<ProtectionQuery>,
) -> Response {
    let params = query
        .symbol
        .map(|symbol| vec![("symbol".to_string(), symbol)])
        .unwrap_or_default();
    match state
        .engine_control
        .protection_request("GET", "/fapi/v1/openAlgoOrders", &params)
        .await
    {
        Ok(result) if result.degraded => (
            StatusCode::OK,
            Json(json!({"items": [], "degraded": true, "reason": "交易所限频冷却中"})),
        )
            .into_response(),
        Ok(result) if (200..300).contains(&result.status) => {
            let rows = parse_rows(&result.body);
            let mode = state
                .db
                .as_ref()
                .map(current_mode)
                .unwrap_or_else(|| "demo".to_string());
            let mut marks = state
                .db
                .as_ref()
                .map(|db| local_marks(db, &mode))
                .unwrap_or_default();
            // 本地没有持仓的标的（外部仓/手工仓）直接向交易所补标记价，
            // 否则保护单列表会出现 mark_price=0、距离=0。
            let missing: std::collections::BTreeSet<String> = rows
                .iter()
                .filter_map(|row| row.get("symbol").and_then(Value::as_str).map(str::to_string))
                .filter(|symbol| !marks.contains_key(symbol))
                .collect();
            for symbol in missing {
                let params = vec![("symbol".to_string(), symbol.clone())];
                let Ok(reply) = state
                    .engine_control
                    .protection_request("GET", "/fapi/v1/premiumIndex", &params)
                    .await
                else {
                    continue;
                };
                if !(200..300).contains(&reply.status) {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&reply.body) else {
                    continue;
                };
                if let Some(price) = value
                    .get("markPrice")
                    .and_then(Value::as_str)
                    .and_then(|text| text.parse::<f64>().ok())
                    .filter(|price| price.is_finite() && *price > 0.0)
                {
                    marks.insert(symbol, price);
                }
            }
            let items: Vec<Value> = rows
                .into_iter()
                .filter_map(|row| protection_item(row, &marks))
                .collect();
            (
                StatusCode::OK,
                Json(json!({"items": items, "count": items.len(), "mode": mode})),
            )
                .into_response()
        }
        Ok(result) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"detail": format!("查询保护单失败: {}", result.body)})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"detail": error})),
        )
            .into_response(),
    }
}

async fn cancel_protection(State(state): State<AppState>, Path(algo_id): Path<String>) -> Response {
    if algo_id.trim().is_empty()
        || !algo_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "无效的 algo_id"})),
        )
            .into_response();
    }
    let check = state
        .engine_control
        .protection_request("GET", "/fapi/v1/openAlgoOrders", &[])
        .await;
    let rows = match check {
        Ok(result) if result.degraded => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": "交易所限频冷却中"})),
            )
                .into_response()
        }
        Ok(result) if (200..300).contains(&result.status) => parse_rows(&result.body),
        Ok(result) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": result.body})),
            )
                .into_response()
        }
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": error})),
            )
                .into_response()
        }
    };
    if !rows
        .iter()
        .any(|row| algo_id_of(row).as_deref() == Some(algo_id.as_str()))
    {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": "保护单不存在或已失效"})),
        )
            .into_response();
    }
    match state
        .engine_control
        .protection_request(
            "DELETE",
            "/fapi/v1/algoOrder",
            &[("algoId".to_string(), algo_id.clone())],
        )
        .await
    {
        Ok(result) if (200..300).contains(&result.status) => (
            StatusCode::OK,
            Json(json!({"ok": true, "algo_id": algo_id})),
        )
            .into_response(),
        Ok(result) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": format!("撤单失败: {}", result.body)})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"detail": error})),
        )
            .into_response(),
    }
}

fn parse_rows(body: &str) -> Vec<Value> {
    let data = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    data.get("orders")
        .or_else(|| data.get("data"))
        .unwrap_or(&data)
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn algo_id_of(row: &Value) -> Option<String> {
    row.get("algoId").map(|value| {
        value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string())
    })
}

fn number(value: Option<&Value>) -> f64 {
    value
        .and_then(|value| value.as_f64().or_else(|| value.as_str()?.parse().ok()))
        .unwrap_or(0.0)
}

/// 本地持仓的标记价：交易所 Algo 接口不返回 markPrice，用它补距离。
fn local_marks(db: &crate::db::Db, mode: &str) -> std::collections::HashMap<String, f64> {
    let mut marks = std::collections::HashMap::new();
    let Ok(conn) = db.conn.lock() else {
        return marks;
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT symbol, mark_price FROM positions \
         WHERE mode=?1 AND status='open' AND mark_price>0",
    ) else {
        return marks;
    };
    if let Ok(rows) = stmt.query_map([mode], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
    }) {
        for (symbol, mark) in rows.flatten() {
            marks.insert(symbol, mark);
        }
    }
    marks
}

fn protection_item(row: Value, marks: &std::collections::HashMap<String, f64>) -> Option<Value> {
    let object = row.as_object()?;
    let symbol = object.get("symbol")?.as_str()?;
    let trigger_price = number(object.get("triggerPrice").or_else(|| object.get("stopPrice")));
    let mark_price = marks.get(symbol).copied().unwrap_or(0.0);
    let distance_pct = if mark_price > 0.0 && trigger_price > 0.0 {
        ((trigger_price - mark_price) / mark_price * 10_000.0).round() / 100.0
    } else {
        0.0
    };
    Some(json!({
        "algo_id": algo_id_of(&row).unwrap_or_default(),
        "client_algo_id": object.get("clientAlgoId").and_then(Value::as_str).unwrap_or_default(),
        "symbol": symbol,
        "side": object.get("side").and_then(Value::as_str).unwrap_or_default(),
        "type": object.get("orderType").or_else(|| object.get("type")).and_then(Value::as_str).unwrap_or_default(),
        "trigger_price": trigger_price,
        "mark_price": (mark_price * 1e8).round() / 1e8,
        "distance_pct": distance_pct,
        "status": object.get("algoStatus").or_else(|| object.get("status")).and_then(Value::as_str).unwrap_or_default(),
        "close_position": object.get("closePosition").and_then(Value::as_bool).unwrap_or(false),
    }))
}

fn current_mode(db: &crate::db::Db) -> String {
    db.conn
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT value FROM engine_state WHERE key='mode'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .unwrap_or_else(|| "demo".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_protection_order_fields_and_string_prices() {
        let row = parse_rows(r#"[{"algoId":42,"symbol":"BTCUSDT","side":"SELL","orderType":"STOP_MARKET","triggerPrice":"65000.5","algoStatus":"NEW","closePosition":true}]"#).remove(0);
        let mut marks = std::collections::HashMap::new();
        marks.insert("BTCUSDT".to_string(), 70000.0);
        let item = protection_item(row, &marks).unwrap();
        assert_eq!(item["algo_id"], "42");
        assert_eq!(item["symbol"], "BTCUSDT");
        assert_eq!(item["trigger_price"], 65000.5);
        assert_eq!(item["mark_price"], 70000.0);
        assert!(
            (item["distance_pct"].as_f64().unwrap() + 7.14).abs() < 0.01,
            "距离必须按触发价相对标记价计算: {}",
            item["distance_pct"]
        );
        assert_eq!(item["close_position"], true);
    }

    #[test]
    fn missing_algo_id_is_not_cancelable() {
        assert_eq!(algo_id_of(&json!({"orderId": 42})), None);
    }

    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    const KEY: &str = "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=";

    struct FakeGateway {
        replies: std::sync::Mutex<std::collections::VecDeque<(u16, String, bool)>>,
    }

    impl FakeGateway {
        fn new(replies: Vec<(u16, String, bool)>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies.into()),
            }
        }
    }

    impl crate::live::Gateway for FakeGateway {
        fn send<'a>(
            &'a self,
            _request: &'a crate::submit::PreparedRequest,
        ) -> crate::live::GatewayFuture<'a> {
            Box::pin(async move {
                let (status, body, transport_failed) = self
                    .replies
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((500, "unexpected request".to_string(), false));
                crate::live::GatewayReply {
                    status,
                    body,
                    transport_failed,
                }
            })
        }
    }

    fn db_with_creds() -> crate::db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
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
        conn.execute(
            "INSERT INTO engine_state(key,value,updated_at) VALUES('mode','demo',0)",
            [],
        )
        .unwrap();
        crate::db::Db {
            path: String::new(),
            conn: Arc::new(std::sync::Mutex::new(conn)),
        }
    }

    fn state(replies: Vec<(u16, String, bool)>) -> AppState {
        let db = db_with_creds();
        let gateway = Arc::new(FakeGateway::new(replies));
        let mut state = crate::test_state(Some(db.clone()));
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

    async fn call(state: AppState, method: &str, uri: &str) -> (StatusCode, Value) {
        let response = routes()
            .with_state(state)
            .oneshot(
                Request::builder()
                    .method(method)
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
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn protection_list_returns_items_on_success() {
        let (status, body) = call(
            state(vec![
                (
                    200,
                    r#"{"orders":[{"algoId":42,"symbol":"BTCUSDT","side":"SELL","orderType":"STOP_MARKET","triggerPrice":"95","algoStatus":"NEW","closePosition":true}]}"#.to_string(),
                    false,
                ),
                (
                    200,
                    r#"{"symbol":"BTCUSDT","markPrice":"100"}"#.to_string(),
                    false,
                ),
            ]),
            "GET",
            "/engine/protection",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["count"], 1);
        assert_eq!(body["items"][0]["algo_id"], "42");
        assert_eq!(body["items"][0]["mark_price"], 100.0);
        assert_eq!(body["items"][0]["distance_pct"], -5.0);
    }

    #[tokio::test]
    async fn protection_list_returns_503_on_exchange_failure() {
        let (status, body) = call(
            state(vec![(500, "boom".to_string(), false)]),
            "GET",
            "/engine/protection",
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    }

    #[tokio::test]
    async fn protection_list_returns_503_without_credentials() {
        let (status, body) = call(crate::test_state(None), "GET", "/engine/protection").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    }

    #[tokio::test]
    async fn cancel_protection_checks_then_deletes() {
        let (status, body) = call(
            state(vec![
                (200, r#"{"orders":[{"algoId":42}]}"#.to_string(), false),
                (200, "{}".to_string(), false),
            ]),
            "DELETE",
            "/engine/protection/42",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["algo_id"], "42");
    }

    #[tokio::test]
    async fn cancel_protection_returns_404_when_algo_is_missing() {
        let (status, body) = call(
            state(vec![(200, r#"{"orders":[]}"#.to_string(), false)]),
            "DELETE",
            "/engine/protection/42",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    #[tokio::test]
    async fn cancel_protection_rejects_invalid_algo_id() {
        let (status, body) = call(state(vec![]), "DELETE", "/engine/protection/bad%21id").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
}
