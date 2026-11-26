//! 管理端鉴权：密码哈希、会话令牌、登录限流。
//!
//! 哈希与令牌格式对齐 `backend/nanoquant/security.py`，现有 SQLite 库里的
//! `pbkdf2_sha256$...` 密码无需迁移即可登录。

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use hmac::{Hmac, Mac};
use pbkdf2::pbkdf2_hmac;
use serde::Serialize;
use sha2::Sha256;

const PBKDF2_ITERS: u32 = 120_000;

type HmacSha256 = Hmac<Sha256>;

/// `pbkdf2_sha256$<iters>$<salt>$<hex>`，与 legacy 内核（已退役） `hash_password` 同格式。
pub fn hash_password(password: &str, salt: &str) -> String {
    let mut out = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt.as_bytes(), PBKDF2_ITERS, &mut out);
    format!("pbkdf2_sha256${PBKDF2_ITERS}${salt}${}", hex(&out))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.splitn(4, '$');
    let (Some(algo), Some(iters), Some(salt), Some(digest)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if algo != "pbkdf2_sha256" {
        return false;
    }
    let Ok(iters) = iters.parse::<u32>() else {
        return false;
    };
    let mut out = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt.as_bytes(), iters, &mut out);
    constant_eq(&hex(&out), digest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 按来源 IP 的登录失败滑动窗口。超限返回 true（应回 429）。
pub struct LoginThrottle {
    limit: usize,
    window_sec: i64,
    fails: HashMap<String, Vec<i64>>,
}

impl LoginThrottle {
    pub fn new(limit: usize, window_sec: i64) -> Self {
        Self {
            limit,
            window_sec,
            fails: HashMap::new(),
        }
    }

    pub fn is_blocked(&mut self, ip: &str, now: i64) -> bool {
        self.prune(ip, now);
        self.fails.get(ip).map(|v| v.len()).unwrap_or(0) >= self.limit
    }

    pub fn record_failure(&mut self, ip: &str, now: i64) {
        self.prune(ip, now);
        self.fails.entry(ip.to_string()).or_default().push(now);
    }

    pub fn clear(&mut self, ip: &str) {
        self.fails.remove(ip);
    }

    fn prune(&mut self, ip: &str, now: i64) {
        if let Some(stamps) = self.fails.get_mut(ip) {
            stamps.retain(|t| now - *t < self.window_sec);
        }
    }
}

/// 无状态会话令牌：`<base64url(json)>.<hmac-sha256 hex>`。
/// 签名密钥为 `sha256(master_key || salt)`，与 legacy 内核（已退役） `_sign` 一致。
pub fn issue_token(
    username: &str,
    ttl_hours: i64,
    version: i64,
    master_key: &[u8],
    salt: &str,
    now: i64,
) -> (String, i64) {
    let exp = now + ttl_hours.max(1) * 3600;
    let body = serde_json::json!({"u": username, "exp": exp, "v": version, "n": "fixed"});
    let raw = serde_json::to_vec(&body).expect("token body");
    let encoded = base64url(&raw);
    let sig = sign(encoded.as_bytes(), master_key, salt);
    (format!("{encoded}.{sig}"), exp)
}

pub fn verify_token(
    token: &str,
    version: Option<i64>,
    master_key: &[u8],
    salt: &str,
    now: i64,
) -> bool {
    if master_key.is_empty() {
        return false;
    }
    let Some((encoded, sig)) = token.rsplit_once('.') else {
        return false;
    };
    if !constant_eq(&sign(encoded.as_bytes(), master_key, salt), sig) {
        return false;
    }
    let Ok(bytes) = base64url_decode(encoded) else {
        return false;
    };
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    if body.get("exp").and_then(|v| v.as_i64()).unwrap_or(0) < now {
        return false;
    }
    if let Some(want) = version {
        if body.get("v").and_then(|v| v.as_i64()).unwrap_or(0) != want {
            return false;
        }
    }
    true
}

fn sign(payload: &[u8], master_key: &[u8], salt: &str) -> String {
    let mut material = master_key.to_vec();
    material.extend_from_slice(salt.as_bytes());
    let key = sha2::Sha256::digest(&material);
    let mut mac = HmacSha256::new_from_slice(&key).expect("hmac key");
    mac.update(payload);
    hex(&mac.finalize().into_bytes())
}

fn base64url(data: &[u8]) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, data)
}

fn base64url_decode(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, data)
}

/// 登录所需的最小账号状态。任务 3 接入 SQLite 前由内存提供。
#[derive(Clone)]
pub struct Security {
    pub username: String,
    pub password_hash: String,
    pub password_is_default: bool,
    pub token_version: i64,
    pub session_ttl_hours: i64,
    pub master_key: Vec<u8>,
    pub salt: String,
}

#[derive(Clone)]
pub struct AuthState {
    pub security: std::sync::Arc<Mutex<Security>>,
    pub throttle: std::sync::Arc<Mutex<LoginThrottle>>,
    pub db: Option<crate::db::Db>,
}

#[derive(Serialize)]
struct ErrorBody {
    detail: String,
    code: String,
}

const PASSWORD_CHANGE_ALLOWED: &[&str] = &[
    "/api/v1/auth/login",
    "/api/v1/auth/password",
    "/api/v1/health",
    "/api/v1/metrics",
];
const PASSWORD_GATE_ALLOWED: &[&str] = &["/api/v1/auth/me"];
/// WebSocket 升级不走 HTTP 中间件（legacy 内核（已退役） 侧同理），但 Rust 的中间件对
/// 全量请求生效，所以这里要显式放行：WS 自带首帧鉴权。
const WS_PATHS: &[&str] = &["/api/v1/ws", "/api/v1/ws/klines"];


#[derive(serde::Deserialize)]
pub struct LoginIn {
    pub username: String,
    pub password: String,
}

#[derive(serde::Deserialize)]
pub struct PasswordIn {
    pub old_password: String,
    pub new_password: String,
}

#[derive(Serialize)]
struct LoginOut {
    token: String,
    expires_at: i64,
    username: String,
    must_change_password: bool,
}

pub fn routes() -> Router<AuthState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/me", get(me))
        .route("/auth/password", post(change_password))
}

async fn login(State(state): State<AuthState>, Json(body): Json<LoginIn>) -> Response {
    let now = crate::now();
    {
        let mut throttle = state.throttle.lock().expect("throttle");
        if throttle.is_blocked("local", now) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(ErrorBody {
                    detail: "失败次数过多".into(),
                    code: "rate_limited".into(),
                }),
            )
                .into_response();
        }
    }
    let sec = state.security.lock().expect("security").clone();
    if body.username != sec.username || !verify_password(&body.password, &sec.password_hash) {
        state
            .throttle
            .lock()
            .expect("throttle")
            .record_failure("local", now);
        return (
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody {
                detail: "用户名或密码错误".into(),
                code: "bad_credentials".into(),
            }),
        )
            .into_response();
    }
    state.throttle.lock().expect("throttle").clear("local");
    let (token, exp) = issue_token(
        &sec.username,
        sec.session_ttl_hours,
        sec.token_version,
        &sec.master_key,
        &sec.salt,
        now,
    );
    Json(LoginOut {
        token,
        expires_at: exp,
        username: sec.username,
        must_change_password: sec.password_is_default,
    })
    .into_response()
}

async fn me(State(state): State<AuthState>) -> impl IntoResponse {
    let sec = state.security.lock().expect("security");
    Json(serde_json::json!({
        "username": sec.username,
        "must_change_password": sec.password_is_default,
        "session_ttl_hours": sec.session_ttl_hours,
    }))
}

async fn change_password(State(state): State<AuthState>, Json(body): Json<PasswordIn>) -> Response {
    let mut sec = state.security.lock().expect("security");
    if !verify_password(&body.old_password, &sec.password_hash) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                detail: "原密码不正确".into(),
                code: "bad_old_password".into(),
            }),
        )
            .into_response();
    }
    if body.new_password == body.old_password {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                detail: "新密码不能与原密码相同".into(),
                code: "same_password".into(),
            }),
        )
            .into_response();
    }
    let new_hash = hash_password(&body.new_password, "rotated");
    let version = sec.token_version + 1;
    if let Some(db) = &state.db {
        if let Err(error) = db.update_security_password(&new_hash, false, version) {
            tracing::error!(%error, "无法持久化密码变更");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    detail: "密码变更未能保存".into(),
                    code: "password_persistence_failed".into(),
                }),
            )
                .into_response();
        }
    }
    sec.token_version = version;
    sec.password_is_default = false;
    sec.password_hash = new_hash;
    let (token, exp) = issue_token(
        &sec.username,
        sec.session_ttl_hours,
        version,
        &sec.master_key,
        &sec.salt,
        crate::now(),
    );
    Json(serde_json::json!({"ok": true, "token": token, "expires_at": exp})).into_response()
}

pub fn bearer_is_valid(state: &AuthState, headers: &axum::http::HeaderMap) -> bool {
    let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
    else {
        return false;
    };
    let security = state.security.lock().expect("security");
    !security.master_key.is_empty()
        && verify_token(
            token,
            Some(security.token_version),
            &security.master_key,
            &security.salt,
            crate::now(),
        )
}

pub async fn require_token(State(state): State<AuthState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if (!path.starts_with("/api/v1/") && path != "/api/v1")
        || PASSWORD_CHANGE_ALLOWED.contains(&path)
        || path.starts_with("/api/v1/public/")
        || matches!(path, "/api/v1/ws" | "/api/v1/ws/klines")

    {
        return next.run(req).await;
    }
    if !bearer_is_valid(&state, req.headers()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody {
                detail: "未授权".into(),
                code: "unauthorized".into(),
            }),
        )
            .into_response();
    }
    next.run(req).await
}

pub async fn force_password_change(
    State(state): State<AuthState>,
    req: Request,
    next: Next,
) -> Response {
    let must = state.security.lock().expect("security").password_is_default;
    if !must {
        return next.run(req).await;
    }
    let path = req.uri().path();
    if PASSWORD_CHANGE_ALLOWED.contains(&path)
        || PASSWORD_GATE_ALLOWED.contains(&path)
        || WS_PATHS.contains(&path)
        || path.starts_with("/api/v1/public/")

    {
        return next.run(req).await;
    }
    if path.starts_with("/api/") {
        return (
            StatusCode::FORBIDDEN,
            Json(ErrorBody {
                detail: "当前仍是初始密码，请先修改密码后再操作".to_string(),
                code: "password_change_required".to_string(),
            }),
        )
            .into_response();
    }
    next.run(req).await
}

use sha2::Digest;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn verifies_legacy_pbkdf2_hash() {
        // 由 legacy 内核（已退役） hashlib.pbkdf2_hmac("sha256", b"s3cret", b"abc", 120000) 生成
        let stored = "pbkdf2_sha256$120000$abc$6a234869f6204dfa6d6ab776ab5c10afbe74b088b7eca22e7b445675db957ca2";
        assert!(verify_password("s3cret", stored), "正确密码应通过");
        assert!(!verify_password("wrong", stored), "错误密码应拒绝");
    }

    #[test]
    fn throttle_blocks_only_the_offending_ip() {
        let mut t = LoginThrottle::new(3, 600);
        for i in 0..3 {
            assert!(!t.is_blocked("1.1.1.1", 1000 + i));
            t.record_failure("1.1.1.1", 1000 + i);
        }
        assert!(t.is_blocked("1.1.1.1", 1010), "超限 IP 应被锁");
        assert!(!t.is_blocked("2.2.2.2", 1010), "其它 IP 不受影响");
        assert!(!t.is_blocked("1.1.1.1", 1010 + 600), "窗口过期后解锁");
    }

    fn test_state(default_password: bool) -> AuthState {
        AuthState {
            security: std::sync::Arc::new(Mutex::new(Security {
                username: "admin".into(),
                password_hash: hash_password("s3cret", "abc"),
                password_is_default: default_password,
                token_version: 1,
                session_ttl_hours: 12,
                master_key: b"master-key".to_vec(),
                salt: "salt".into(),
            })),
            throttle: std::sync::Arc::new(Mutex::new(LoginThrottle::new(3, 600))),
            db: None,
        }
    }

    #[tokio::test]
    async fn default_password_blocks_trading_but_not_me() {
        let state = test_state(true);
        let token = {
            let sec = state.security.lock().unwrap();
            issue_token(
                &sec.username,
                1,
                sec.token_version,
                &sec.master_key,
                &sec.salt,
                crate::now(),
            )
            .0
        };
        let app = Router::new()
            .nest(
                "/api/v1",
                Router::new()
                    .route("/auth/me", get(me))
                    .with_state(state.clone()),
            )
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                force_password_change,
            ))
            .layer(axum::middleware::from_fn_with_state(state, require_token));
        let blocked = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/engine/status")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
        let me = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/me")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_token_returns_401() {
        let state = test_state(false);
        let app = routes()
            .with_state(state.clone())
            .layer(axum::middleware::from_fn_with_state(state, require_token));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn repeated_login_failures_return_429() {
        let state = test_state(false);
        let app = routes().with_state(state);
        for _ in 0..3 {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/auth/login")
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"username":"admin","password":"wrong"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = app
            .oneshot(
                Request::post("/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"username":"admin","password":"wrong"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    fn password_db() -> crate::db::Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);",
        )
        .unwrap();
        let security = serde_json::json!({
            "username": "admin", "password_hash": hash_password("s3cret", "fixture"),
            "password_is_default": true, "token_version": 7, "session_ttl_hours": 8,
            "login_fail_limit": 4,
        });
        conn.execute(
            "INSERT INTO app_settings VALUES('security', ?1, 0)",
            [security.to_string()],
        )
        .unwrap();
        crate::db::Db {
            path: String::new(),
            conn: std::sync::Arc::new(Mutex::new(conn)),
        }
    }

    async fn submit_password_change(state: AuthState) -> Response {
        routes()
            .with_state(state)
            .oneshot(
                Request::post("/auth/password")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"old_password":"s3cret","new_password":"new-password"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn password_change_survives_auth_reload() {
        let db = password_db();
        let key = b"test-key";
        let state = db.auth_state(key.to_vec(), "salt".into());
        let now = crate::now();
        let old_token = issue_token("admin", 8, 7, key, "salt", now).0;
        let response = submit_password_change(state.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let reloaded = db.auth_state(key.to_vec(), "salt".into());
        let saved = reloaded.security.lock().unwrap();
        assert_eq!(saved.token_version, 8);
        assert!(!saved.password_is_default);
        assert!(verify_password("new-password", &saved.password_hash));
        assert!(!verify_password("s3cret", &saved.password_hash));
        assert_eq!(
            db.setting("security").unwrap().unwrap()["login_fail_limit"],
            4
        );
        assert_eq!(
            state.security.lock().unwrap().password_hash,
            saved.password_hash
        );
        assert!(!verify_token(
            &old_token,
            Some(saved.token_version),
            key,
            "salt",
            now
        ));
        assert!(verify_token(
            body["token"].as_str().unwrap(),
            Some(saved.token_version),
            key,
            "salt",
            now
        ));
    }

    #[tokio::test]
    async fn password_change_write_failure_keeps_current_credentials() {
        let db = password_db();
        let original = db.setting("security").unwrap();
        db.lock().execute_batch("CREATE TRIGGER reject_security_update BEFORE UPDATE ON app_settings WHEN OLD.key = 'security' BEGIN SELECT RAISE(ABORT, 'write rejected'); END;").unwrap();
        let state = db.auth_state(b"test-key".to_vec(), "salt".into());
        let response = submit_password_change(state.clone()).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let sec = state.security.lock().unwrap();
        assert_eq!(sec.token_version, 7);
        assert!(sec.password_is_default);
        assert!(verify_password("s3cret", &sec.password_hash));
        assert_eq!(db.setting("security").unwrap(), original);
    }

    #[test]
    fn old_token_dies_after_version_bump() {
        let key = b"master-key";
        let (token, _) = issue_token("admin", 12, 1, key, "salt", 1_000);
        assert!(verify_token(&token, Some(1), key, "salt", 1_000));
        assert!(
            !verify_token(&token, Some(2), key, "salt", 1_000),
            "改密后旧代际应失效"
        );
    }
}
