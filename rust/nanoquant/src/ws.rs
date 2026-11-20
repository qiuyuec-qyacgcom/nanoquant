//! WebSocket 推送：`/ws` 事件流与 `/ws/klines` K 线流。
//!
//! 鉴权约定与 `api/routes/ws.py` 一致：token 不进 URL，连接后第一帧发
//! `{"type":"auth","token":"..."}`；首帧不合法一律按未认证处理。
//!
//! 推送契约（前端 `ShellLayout.vue` / `KlineBoard.vue` 依赖）：
//! - 事件流：`{"type":"event","data":{seq,level,category,message,payload,ts}}`；
//! - K 线流：`{"type":"kline","data":{symbol,interval,t,o,h,l,c,v,closed,...}}`；
//! - 空闲时分别发 `{"type":"ping","data":{}}` 与 `{"type":"ping"}` 保持连接。

use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::AppState;

/// 与 `exchange/kline_stream.py` 的 `ALLOWED_INTERVALS` 对齐。
pub const ALLOWED_INTERVALS: &[&str] = &["1m", "3m", "5m", "15m", "30m", "1h", "4h", "1d"];
/// 事件流空闲心跳间隔，与 legacy 内核（已退役） 的 25 秒一致。
const EVENT_IDLE_SEC: u64 = 25;
/// K 线流空闲心跳间隔，与 legacy 内核（已退役） 的 20 秒一致。
const KLINE_IDLE_SEC: u64 = 20;
/// 单帧发送超时：客户端太慢就断开，不能让一个卡住的连接占住推送任务。
const SEND_TIMEOUT_SEC: u64 = 5;
/// 超过订阅规模上限时的关闭码。
const CLOSE_TOO_MANY_SUBSCRIPTIONS: u16 = 4429;

#[derive(Debug, PartialEq)]
pub enum Gate {
    Allow,
    Close(u16),
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/ws", get(events))
        .route("/ws/klines", get(klines))
}

/// K 线连接的放行判定。顺序与 legacy 内核（已退役） 一致：参数 → 匿名且站点关闭 → 标的池。
pub fn kline_gate(
    symbol: &str,
    interval: &str,
    is_admin: bool,
    site_enabled: bool,
    symbol_known: bool,
) -> Gate {
    if symbol.is_empty() || !ALLOWED_INTERVALS.contains(&interval) {
        return Gate::Close(4400);
    }
    if !is_admin && !site_enabled {
        return Gate::Close(4403);
    }
    if !symbol_known {
        return Gate::Close(4404);
    }
    Gate::Allow
}

/// 事件流连接：要求登录时首帧鉴权失败即 4401。
pub fn events_gate(require_login: bool, authed: bool) -> Gate {
    if require_login && !authed {
        Gate::Close(4401)
    } else {
        Gate::Allow
    }
}

#[derive(Deserialize)]
pub struct KlineQuery {
    #[serde(default)]
    symbol: String,
    #[serde(default = "default_interval")]
    interval: String,
}

fn default_interval() -> String {
    "1m".into()
}

async fn events(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| serve_events(socket, state))
}

async fn klines(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(q): Query<KlineQuery>,
) -> Response {
    ws.on_upgrade(move |socket| serve_klines(socket, state, q))
}

async fn serve_events(mut socket: WebSocket, state: AppState) {
    let require_login = require_login(&state);
    let authed = if require_login {
        read_auth_frame(&mut socket, Duration::from_secs(5), &state).await
    } else {
        true
    };
    if events_gate(require_login, authed) != Gate::Allow {
        close_with(&mut socket, 4401, "unauthorized").await;
        return;
    }
    let mut events = state.events.subscribe();
    if !send_json(&mut socket, r#"{"type":"hello","data":{"ok":true}}"#).await {
        return;
    }
    loop {
        let outgoing = tokio::select! {
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) => break,
                Some(Ok(Message::Close(_))) => break,
                // 客户端只发心跳，其它内容一律忽略。
                Some(Ok(_)) => None,
            },
            result = events.recv() => match result {
                Ok(payload) => Some(payload.to_string()),
                Err(RecvError::Lagged(skipped)) => {
                    // 慢客户端丢帧即可，不能把事件生产方堵住。
                    tracing::warn!(skipped, "事件订阅者跟不上，已丢弃部分事件");
                    None
                }
                Err(RecvError::Closed) => break,
            },
            _ = tokio::time::sleep(Duration::from_secs(EVENT_IDLE_SEC)) => {
                Some(r#"{"type":"ping","data":{}}"#.to_string())
            }
        };
        if let Some(text) = outgoing {
            if !send_json(&mut socket, &text).await {
                break;
            }
        }
    }
}

async fn serve_klines(mut socket: WebSocket, state: AppState, q: KlineQuery) {
    let symbol = q.symbol.to_uppercase();
    let require_login = require_login(&state);
    let is_admin = if require_login {
        read_auth_frame(&mut socket, Duration::from_millis(600), &state).await
    } else {
        true
    };
    let symbol_known = symbol_in_pool(&state, &symbol);
    match kline_gate(
        &symbol,
        &q.interval,
        is_admin,
        site_enabled(&state),
        symbol_known,
    ) {
        Gate::Close(code) => {
            close_with(&mut socket, code, "rejected").await;
            return;
        }
        Gate::Allow => {}
    }
    let mut bars = match state.klines.subscribe(&symbol, &q.interval) {
        Ok(bars) => bars,
        Err(error) => {
            close_with(
                &mut socket,
                CLOSE_TOO_MANY_SUBSCRIPTIONS,
                &error.to_string(),
            )
            .await;
            return;
        }
    };
    let hello = json!({"type": "hello", "data": {"symbol": symbol, "interval": q.interval}});
    if !send_json(&mut socket, &hello.to_string()).await {
        state.klines.unsubscribe(&symbol, &q.interval);
        return;
    }
    loop {
        let outgoing = tokio::select! {
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) => break,
                Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => None,
            },
            result = bars.recv() => match result {
                Ok(bar) => Some(json!({"type": "kline", "data": bar}).to_string()),
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, symbol = %symbol, "K 线订阅者跟不上，已丢弃部分帧");
                    None
                }
                Err(RecvError::Closed) => break,
            },
            _ = tokio::time::sleep(Duration::from_secs(KLINE_IDLE_SEC)) => {
                Some(json!({"type": "ping"}).to_string())
            }
        };
        if let Some(text) = outgoing {
            if !send_json(&mut socket, &text).await {
                break;
            }
        }
    }
    // 连接结束必须退订，否则上游会一直给这条流推数据。
    state.klines.unsubscribe(&symbol, &q.interval);
}

/// 发送一帧。返回 false 表示连接已不可用（对端断开或发送超时）。
async fn send_json(socket: &mut WebSocket, text: &str) -> bool {
    matches!(
        tokio::time::timeout(
            Duration::from_secs(SEND_TIMEOUT_SEC),
            socket.send(Message::Text(text.into())),
        )
        .await,
        Ok(Ok(()))
    )
}

async fn close_with(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

async fn read_auth_frame(socket: &mut WebSocket, timeout: Duration, state: &AppState) -> bool {
    let raw = match tokio::time::timeout(timeout, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => text,
        _ => return false,
    };
    let (version, key, salt) = {
        let guard = state.auth.security.lock().expect("auth");
        (
            guard.token_version,
            guard.master_key.clone(),
            guard.salt.clone(),
        )
    };
    crate::wsauth::first_frame_ok(&raw, version, &key, &salt, crate::now())
}

fn require_login(state: &AppState) -> bool {
    state
        .db
        .as_ref()
        .and_then(|db| db.setting("security").ok().flatten())
        .and_then(|g| g.get("require_login").and_then(|v| v.as_bool()))
        .unwrap_or(true)
}

fn site_enabled(state: &AppState) -> bool {
    state
        .db
        .as_ref()
        .and_then(|db| db.setting("public").ok().flatten())
        .and_then(|g| g.get("site_enabled").and_then(|v| v.as_bool()))
        .unwrap_or(true)
}

fn symbol_in_pool(state: &AppState, symbol: &str) -> bool {
    let Some(db) = state.db.as_ref() else {
        return false;
    };
    let conn = db.conn.lock().expect("db");
    conn.query_row(
        "SELECT COUNT(*) FROM trading_symbols WHERE symbol = ?1 AND enabled != 0",
        [symbol],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBus;
    use crate::kline_stream::{KlineHub, KlineSource};
    use std::sync::Arc;

    #[test]
    fn rejects_bad_params_before_anything_else() {
        assert_eq!(kline_gate("", "1m", true, true, true), Gate::Close(4400));
        assert_eq!(
            kline_gate("BTCUSDT", "2m", true, true, true),
            Gate::Close(4400),
            "2m 不在白名单"
        );
    }

    #[test]
    fn anonymous_visitor_on_closed_site_is_forbidden() {
        assert_eq!(
            kline_gate("BTCUSDT", "1m", false, false, true),
            Gate::Close(4403)
        );
    }

    #[test]
    fn admin_reads_klines_even_when_site_is_closed() {
        assert_eq!(kline_gate("BTCUSDT", "1m", true, false, true), Gate::Allow);
    }

    #[test]
    fn symbol_outside_enabled_pool_is_not_found() {
        assert_eq!(
            kline_gate("DOGEUSDT", "1m", true, true, false),
            Gate::Close(4404)
        );
    }

    #[test]
    fn events_stream_closes_401_only_when_login_required() {
        assert_eq!(events_gate(true, false), Gate::Close(4401));
        assert_eq!(events_gate(true, true), Gate::Allow);
        assert_eq!(
            events_gate(false, false),
            Gate::Allow,
            "免登录模式下匿名也要能收事件"
        );
    }

    struct DeadSource;

    impl KlineSource for DeadSource {
        fn connect(
            &self,
            _url: &str,
        ) -> crate::kline_stream::BoxFuture<
            'static,
            Result<Box<dyn crate::kline_stream::Upstream>, String>,
        > {
            Box::pin(async { Err("测试环境不连上游".to_string()) })
        }
    }

    fn test_hub() -> KlineHub {
        KlineHub::new(Arc::new(DeadSource), Arc::new(|| "demo".to_string()))
    }

    #[tokio::test]
    async fn unsubscribing_removes_the_stream_from_the_status() {
        let hub = test_hub();
        let _rx = hub.subscribe("BTCUSDT", "1m").unwrap();
        assert_eq!(hub.status()["subscriptions"].as_array().unwrap().len(), 1);
        assert_eq!(hub.status()["clients"], json!(1));
        hub.unsubscribe("BTCUSDT", "1m");
        assert!(hub.status()["subscriptions"].as_array().unwrap().is_empty());
        assert_eq!(hub.status()["clients"], json!(0));
        hub.stop();
    }

    #[tokio::test]
    async fn the_event_bus_is_wired_into_the_application_state() {
        let bus = EventBus::new(None);
        let mut rx = bus.subscribe();
        bus.publish("account", json!({"equity": 1234.5}));
        let payload = rx.recv().await.unwrap();
        assert_eq!(payload["type"], "account");
        assert_eq!(payload["data"]["equity"], 1234.5);
    }

    /// 起一个真实的 HTTP 服务，返回 WebSocket 基址。
    async fn spawn_server(state: AppState) -> String {
        let app = crate::router(state, None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("ws://{addr}")
    }

    fn test_state_with_auth() -> AppState {
        let auth = crate::auth::AuthState {
            security: std::sync::Arc::new(std::sync::Mutex::new(crate::auth::Security {
                username: "admin".into(),
                password_hash: String::new(),
                password_is_default: false,
                token_version: 0,
                session_ttl_hours: 12,
                master_key: vec![17; 32],
                salt: "nanoquant-static-salt-v1".into(),
            })),
            throttle: std::sync::Arc::new(std::sync::Mutex::new(crate::auth::LoginThrottle::new(
                8, 600,
            ))),
            db: None,
        };
        AppState {
            auth,
            ..crate::test_state(None)
        }
    }

    #[tokio::test]
    async fn an_authenticated_client_receives_events_over_the_socket() {
        use futures_util::{SinkExt, StreamExt};
        let state = test_state_with_auth();
        let bus = state.events.clone();
        let (token, _) = crate::auth::issue_token(
            "admin",
            12,
            0,
            &[17; 32],
            "nanoquant-static-salt-v1",
            crate::now(),
        );
        let addr = spawn_server(state).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("{addr}/api/v1/ws"))
            .await
            .expect("应能连上");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                format!(r#"{{"type":"auth","token":"{token}"}}"#).into(),
            ))
            .await
            .unwrap();
        let hello = socket.next().await.unwrap().unwrap();
        let hello = hello.to_text().unwrap().to_string();
        assert!(hello.contains("hello"), "首帧应是 hello: {hello}");
        bus.record("info", "trade", "BTCUSDT 已成交", json!({}));
        let frame = tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .expect("事件应及时送达")
            .unwrap()
            .unwrap();
        let text = frame.to_text().unwrap().to_string();
        assert!(text.contains("\"type\":\"event\""), "应收到事件: {text}");
        assert!(text.contains("BTCUSDT 已成交"), "应带上事件内容: {text}");
    }

    #[tokio::test]
    async fn an_unauthenticated_client_is_closed_with_4401() {
        use futures_util::StreamExt;
        let state = test_state_with_auth();
        let addr = spawn_server(state).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("{addr}/api/v1/ws"))
            .await
            .expect("应能连上");
        // 不发鉴权帧，等服务端按未认证处理（首帧宽限 5 秒，与 legacy 内核（已退役） 一致）。
        let frame = tokio::time::timeout(Duration::from_secs(8), socket.next())
            .await
            .expect("服务端应主动关闭")
            .unwrap()
            .unwrap();
        match frame {
            tokio_tungstenite::tungstenite::Message::Close(Some(close)) => {
                assert_eq!(u16::from(close.code), 4401, "未鉴权应回 4401");
            }
            other => panic!("应收到关闭帧，实际: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_kline_client_for_an_unknown_symbol_is_closed_with_4404() {
        use futures_util::StreamExt;
        let state = test_state_with_auth();
        let addr = spawn_server(state).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "{addr}/api/v1/ws/klines?symbol=DOGEUSDT&interval=1m"
        ))
        .await
        .expect("应能连上");
        let frame = tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .expect("服务端应主动关闭")
            .unwrap()
            .unwrap();
        match frame {
            tokio_tungstenite::tungstenite::Message::Close(Some(close)) => {
                assert_eq!(u16::from(close.code), 4404, "不在标的池应回 4404");
            }
            other => panic!("应收到关闭帧，实际: {other:?}"),
        }
    }

    fn ws_pool_db(path: &std::path::Path, site_enabled: bool) -> crate::db::Db {
        let _ = std::fs::remove_file(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE trading_symbols(symbol TEXT, enabled INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             INSERT INTO app_settings VALUES('security','{{\"require_login\":true}}',0); \
             INSERT INTO app_settings VALUES('public','{{\"site_enabled\":{site_enabled}}}',0); \
             INSERT INTO trading_symbols VALUES('NEWUSDT',1);"
        ))
        .unwrap();
        drop(conn);
        crate::db::Db::open(path.to_str().unwrap()).unwrap()
    }

    #[test]
    fn heartbeat_and_close_codes_match_the_contract() {
        assert_eq!(EVENT_IDLE_SEC, 25, "事件流空闲心跳 25s");
        assert_eq!(KLINE_IDLE_SEC, 20, "K 线流空闲心跳 20s");
        assert_eq!(SEND_TIMEOUT_SEC, 5, "单帧发送超时 5s");
        assert_eq!(CLOSE_TOO_MANY_SUBSCRIPTIONS, 4429, "订阅超限关闭码 4429");
    }

    #[tokio::test]
    async fn too_many_upstream_streams_is_closed_with_4429() {
        use futures_util::StreamExt;
        let path = std::env::temp_dir().join("nq-ws-4429.db");
        let db = ws_pool_db(&path, true);
        let hub = test_hub();
        for index in 0..crate::kline_stream::MAX_STREAMS {
            hub.subscribe(&format!("SYM{index}USDT"), "1m").unwrap();
        }
        let mut state = crate::test_state(Some(db));
        state.klines = hub.clone();
        let addr = spawn_server(state).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "{addr}/api/v1/ws/klines?symbol=NEWUSDT&interval=1m"
        ))
        .await
        .expect("应能连上");
        let frame = tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .expect("服务端应主动关闭")
            .unwrap()
            .unwrap();
        match frame {
            tokio_tungstenite::tungstenite::Message::Close(Some(close)) => {
                assert_eq!(u16::from(close.code), 4429, "订阅超限必须回 4429");
            }
            other => panic!("应收到关闭帧，实际: {other:?}"),
        }
        hub.stop();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
    }

    #[tokio::test]
    async fn an_unauthenticated_kline_client_is_closed_after_the_600ms_grace() {
        use futures_util::StreamExt;
        let path = std::env::temp_dir().join("nq-ws-grace.db");
        let db = ws_pool_db(&path, false);
        let mut state = crate::test_state(Some(db));
        state.klines = test_hub();
        let addr = spawn_server(state).await;
        let started = std::time::Instant::now();
        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "{addr}/api/v1/ws/klines?symbol=NEWUSDT&interval=1m"
        ))
        .await
        .expect("应能连上");
        let frame = tokio::time::timeout(Duration::from_secs(3), socket.next())
            .await
            .expect("服务端应在宽限后关闭")
            .unwrap()
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(550),
            "必须先给 600ms 首帧宽限"
        );
        match frame {
            tokio_tungstenite::tungstenite::Message::Close(Some(close)) => {
                assert_eq!(u16::from(close.code), 4403, "站点关闭且匿名应回 4403");
            }
            other => panic!("应收到关闭帧，实际: {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
