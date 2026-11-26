//! NanoQuant 主服务入口。
//!
//! 这是 Rust 主进程。当前阶段只提供与前端对齐的
//! 探活契约（`/api/v1/health`）和静态前端托管；交易、鉴权、行情等域按
//! `docs/superpowers/plans/2026-11-18-rust-rewrite.md` 逐个迁入。

#![allow(
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::collapsible_if,
    clippy::needless_borrows_for_generic_args,
    clippy::cloned_ref_to_slice_refs,
    clippy::needless_borrow,
    clippy::borrow_deref_ref
)]

mod ai;

mod ai_analysis;
mod ai_client;
mod ai_market;
mod ai_providers;
mod ai_review;
mod app_state;
mod archive;
mod auth;
mod backtest;
mod backtest_engine;
mod backup;
mod book_stream;
mod bootstrap;
mod cli;
mod config;
mod cycle;
mod db;
mod engine;
mod engine_actions;
mod events;
mod evidence;
mod evidence_task;
mod evolution_api;
mod evolution_review;
mod exchange_time;
mod exec_client;
mod execution;
mod ga_tune;
mod history;
mod interpreter_refs;
mod journal;
mod journal_api;
mod kline_stream;
mod live;
mod manage;
mod mean_reversion;
mod news;
mod oi_regime;
mod ops_tools;
mod param_registry;
mod ai_tuning;
mod ai_tuning_api;
mod protection_api;
mod providers;
mod public;
mod reasoning;
mod reconcile;
mod regime;
mod rl_exec;
mod review;
mod risk;
mod runtime;
mod scan;
mod schema;
mod secret;
mod settings_api;
mod sign;
mod signal_audit;


mod strategies;
mod submit;
mod symbol_scan;
mod symbols;
mod system;
mod trade_policy;
mod tuning;
mod ws;
mod wsauth;

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::app_state::AppState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing::info;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Serialize)]
struct Health {
    ok: bool,
    name: &'static str,
    version: &'static str,
    uptime_sec: i64,
    kernel: &'static str,
}

/// 日志双写：stdout（systemd journal 里能看到）+ 日志文件（面板「服务日志」读它）。
/// 之前只写 stdout，面板的「服务日志」永远是空的——引擎出错根本无从查起。
#[derive(Clone)]
struct TeeWriter {
    file: std::sync::Arc<std::sync::Mutex<std::fs::File>>,
}

impl std::io::Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(buf);
            let _ = file.flush();
        }
        std::io::stdout().write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TeeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// 装好日志订阅器：文件写不进去就退回 stdout，不能因为日志把服务拖垮。
fn init_tracing(home: &std::path::Path, level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("nanoquant={level},tower_http=info").into());
    let logs_dir = std::env::var_os("NANOQUANT_LOG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join("logs"));
    let file = std::fs::create_dir_all(&logs_dir).ok().and_then(|_| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs_dir.join("nanoquant.log"))
            .ok()
    });
    match file {
        Some(file) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(TeeWriter {
                    file: std::sync::Arc::new(std::sync::Mutex::new(file)),
                })
                .init();
        }
        None => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }
}

/// 把后台任务状态写进 engine_state：面板据此区分「引擎停了」和「任务根本没起来」。
fn set_runtime_task(db: Option<&crate::db::Db>, value: &str) {
    let Some(db) = db else { return };
    let Ok(conn) = db.conn.lock() else { return };
    let _ = conn.execute(
        "INSERT INTO engine_state(key, value, updated_at) VALUES('runtime_task', ?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        rusqlite::params![value, now()],
    );
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let config = config::Config::from_env(&args);
    let log_level = config::resolve_log_level(std::path::Path::new(&config.db_path));
    init_tracing(&config.home, &log_level);
    if let Some(index) = args.iter().position(|arg| arg == "ops") {
        let cli_args = args[index + 1..].to_vec();
        std::process::exit(cli::run(&cli_args, &config));
    }
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .expect("NANOQUANT_HOST/NANOQUANT_PORT 不是合法地址");

    if args.iter().any(|arg| arg == "--healthcheck") {
        let ok = reqwest::get(healthcheck_target(config.port))
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        std::process::exit(if ok { 0 } else { 1 });
    }

    let data_dir = config.data_dir.clone();
    let db_path = config.db_path.clone();
    std::fs::create_dir_all(&data_dir)
        .unwrap_or_else(|error| panic!("无法创建数据目录 {}: {error}", data_dir.display()));
    let key_path = config.key_file.clone();
    let admin_password = config.admin_password.clone();
    let bootstrap_report = bootstrap::initialize(
        std::path::Path::new(&db_path),
        &key_path,
        admin_password.as_deref(),
    )
    .unwrap_or_else(|error| panic!("数据库初始化失败: {error}"));
    if bootstrap_report.created_master_key {
        tracing::warn!("已生成主密钥文件，请单独备份保存（丢失后无法解密历史备份）");
    }
    if let Some(password) = bootstrap_report.generated_password.as_deref() {
        tracing::warn!("已生成初始管理员密码: {password}（首次登录后请立即修改）");
    }
    info!(
        tables = bootstrap_report.tables,
        created = bootstrap_report.created_database,
        "数据库已就绪"
    );
    let raw = std::fs::read_to_string(&key_path)
        .unwrap_or_else(|error| panic!("无法读取主密钥文件 {}: {error}", key_path.display()));
    let key = raw.trim();
    let decoded = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE, key);
    assert!(
        matches!(decoded, Ok(bytes) if bytes.len() == 32),
        "主密钥文件格式无效: {}",
        key_path.display()
    );
    let salt = env::var("NANOQUANT_SALT").unwrap_or_else(|_| "nanoquant-static-salt-v1".into());
    let (auth_state, db_handle) = match db::Db::open(&db_path) {
        Ok(db) => {
            info!("已加载数据库 {db_path}");
            (db.auth_state(key.as_bytes().to_vec(), salt), Some(db))
        }
        Err(e) => {
            tracing::warn!("{e}；以空鉴权状态启动（登录不可用，直到配置数据库）");
            (default_auth(), None)
        }
    };
    if let Some(db) = db_handle.as_ref() {
        // 记录本次启动时间：总览的「运行时长」与「进程是否重启过」都靠它。
        let conn = db.conn.lock().expect("db");
        let started = now();
        let _ = conn.execute(
            "INSERT INTO engine_state(key, value, updated_at) VALUES('service_started_at', ?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            (started.to_string(), started),
        );
    }
    let bus = events::EventBus::new(db_handle.clone());
    let mode_db = db_handle.clone();
    let klines = kline_stream::KlineHub::new(
        std::sync::Arc::new(kline_stream::BinanceWsSource),
        std::sync::Arc::new(move || current_mode(mode_db.as_ref())),
    );
    let master_key = auth_state
        .security
        .lock()
        .expect("security")
        .master_key
        .clone();
    let cycle_signal = std::sync::Arc::new(tokio::sync::Notify::new());
    let cycle_busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runtime_active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let engine_control = runtime::EngineControl::with_cycle(
        db_handle.clone().expect("runtime database is available"),
        master_key.clone(),
        Some(cycle_signal.clone()),
        cycle_busy.clone(),
        runtime_active.clone(),
    );
    let state = AppState {
        started_at: now(),
        auth: auth_state,
        db: db_handle,
        engine_control,
        events: bus,
        klines,
    };
    let database = state.db.clone();
    let master_key = state
        .auth
        .security
        .lock()
        .expect("security")
        .master_key
        .clone();
    let events_bus = state.events.clone();
    let klines = state.klines.clone();
    // 每日自动备份：备份页承诺「下一轮检查（≤15 分钟）自动跑」，这里真的按 15 分钟检查一次。
    // 与交易引擎解耦：引擎停用时同样要备份。
    let auto_backup_state = state.clone();
    tokio::spawn(backup::spawn_auto_backup_task(auto_backup_state));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("绑定 {addr} 失败: {e}"));

    // 签名时间戳依赖交易所时钟：先用 /fapi/v1/time 对时，之后每分钟刷新
    exchange_time::spawn_sync_task();
    let (stop_tx, stop_rx) = runtime::stop_channel();
    // 自动换池独立任务：与交易引擎解耦，进程退出时随 stop_tx 一起收敛。
    if let Some(db) = database.clone() {
        tokio::spawn(crate::symbol_scan::run_loop(db.clone(), stop_tx.subscribe()));
        // 本地盘口：bookTicker 合并流只在开关打开且 live/demo 时启动；
        // 未启动或断线时 snapshot 会自动回退 REST。
        if crate::book_stream::startup_enabled(&db) {
            tokio::spawn(crate::book_stream::run_task(db.clone(), stop_tx.subscribe()));
        }
        // 只要存在仓位，0.5 秒风控任务会直接检查止盈止损并平仓，不再等 480 秒周期。
        // 标记价/浮盈在同一循环里刷新，无需额外的 3 秒持仓标记任务。
        tokio::spawn(crate::runtime::run_fast_risk_task(
            db.clone(),
            master_key.clone(),
            stop_tx.subscribe(),
        ));
        // 可用余额/钱包直接读交易所账户接口，同样不受策略周期限制。
        tokio::spawn(crate::runtime::run_account_balance_task(
            db,
            master_key.clone(),
            stop_tx.subscribe(),
        ));
    }
    // AI 复核器必须注入引擎：漏了这一步，每个候选都会以「AI 复核不可用」被拒，
    // 信号审计里全是同一句话，真正的复核原因根本落不了库。
    let ai_state = database.as_ref().map(|db| {
        ai_providers::AiState::new(db.clone(), master_key.clone()).with_market(std::sync::Arc::new(
            ai_market::BinanceMarketSource::new(db.clone()),
        ))
    });
    // 独立调参任务：只在有数据库时启动，只生成候选参数集，与交易引擎完全解耦。
    if let Some(ai) = ai_state.clone() {
        tokio::spawn(crate::tuning::spawn_tuning_task(ai, stop_tx.subscribe()));
    }
    // 执行内核端点：EXEC_TCP 优先，其次 EXEC_SOCKET（与旧 exec_endpoint 同口径）。
    let exec_endpoint = std::env::var("EXEC_TCP")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|addr| format!("tcp:{addr}"))
        .or_else(|| {
            std::env::var("EXEC_SOCKET")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|path| format!("unix:{path}"))
        });
    let mut core_client: Option<std::sync::Arc<exec_client::ExecClient>> = None;
    let mut core_ready = false;
    match exec_endpoint {
        Some(endpoint) => {
            let client = exec_client::ExecClient::new(&endpoint);
            match client.handshake().await {
                Ok(hello) => {
                    info!(
                        endpoint = %endpoint,
                        kernel_version = hello["kernel_version"].as_str().unwrap_or("?"),
                        "执行内核握手成功"
                    );
                    client.spawn_maintainer();
                    client.spawn_reporter();
                    exec_client::install(client.clone());
                    core_client = Some(client);
                    core_ready = true;
                }
                Err(error) => {
                    // 强依赖：握手失败 → 交易引擎不启动（HTTP 查询/前端照常）。
                    eprintln!("执行内核握手失败：{error}，交易引擎不启动");
                    if let Some(db) = database.as_ref() {
                        if let Ok(conn) = db.conn.lock() {
                            let _ = conn.execute(
                                "INSERT INTO engine_events(ts, level, category, message, payload) \
                                 VALUES(?1, 'error', 'exec', ?2, '{}')",
                                (
                                    now(),
                                    format!("执行内核握手失败，交易引擎未启动：{error}"),
                                ),
                            );
                        }
                    }
                    exec_client::install(client.clone());
                    core_client = Some(client);
                }
            }
        }
        None => {
            eprintln!("未配置 EXEC_TCP/EXEC_SOCKET，交易引擎不启动（默认执行内核为必选组件）");
            if let Some(db) = database.as_ref() {
                if let Ok(conn) = db.conn.lock() {
                    let _ = conn.execute(
                        "INSERT INTO engine_events(ts, level, category, message, payload) \
                         VALUES(?1, 'error', 'exec', ?2, '{}')",
                        (now(), "未配置执行内核端点（EXEC_TCP/EXEC_SOCKET），交易引擎未启动"),
                    );
                }
            }
        }
    }
    let app = router(state, config.frontend.clone());
    // 先落一个「准备中」：任务真正跑起来会改成 alive，锁没拿到会改成 lock_failed。
    // 面板靠这个字段区分「引擎停了」和「引擎压根没起来」。
    set_runtime_task(database.as_ref(), "starting");
    let runtime_task = match (database.clone(), ai_state.clone()) {
        (Some(db), Some(ai)) if core_ready => match runtime::acquire_lock(&data_dir) {
            Ok(lock) => {
                let mut engine = runtime::EngineRuntime::new(db, lock, master_key, events_bus)
                    .with_reviewer(std::sync::Arc::new(ai_review::EngineAiReviewer::new(
                        ai,
                        tokio::runtime::Handle::try_current().ok(),
                    )))
                    .with_cycle(cycle_signal, cycle_busy, runtime_active);
                if let Some(client) = core_client.clone() {
                    engine = engine.with_exec_client(client);
                }
                Some(tokio::spawn(engine.run(stop_rx)))
            }
            Err(error) => {
                tracing::warn!(%error, "无法获取引擎锁，仅启动 HTTP 服务");
                set_runtime_task(database.as_ref(), "lock_failed");
                None
            }
        },
        (Some(_), Some(_)) => {
            tracing::warn!("执行内核未连接，交易引擎不启动");
            set_runtime_task(database.as_ref(), "exec_unavailable");
            None
        }
        _ => {
            set_runtime_task(database.as_ref(), "no_runtime");
            None
        }
    };
    info!("NanoQuant (Rust) 监听 http://{addr}");
    let signal_stop_tx = stop_tx.clone();
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = signal_stop_tx.send(true);
        })
        .await;
    let _ = stop_tx.send(true);
    // 关掉上游 K 线流，别让后台任务在进程退出后还挂着。
    klines.stop();
    if let Some(task) = runtime_task {
        if let Err(error) = task.await {
            tracing::error!(%error, "Rust 引擎后台任务异常结束");
        }
    }
    server_result.expect("服务异常退出");
}

fn router(state: AppState, frontend: Option<PathBuf>) -> Router {
    let mut api = Router::new()
        .route("/health", get(health))
        .with_state(state.clone())
        .merge(auth::routes().with_state(state.auth.clone()))
        .merge(ws::routes().with_state(state.clone()))
        .merge(kline_stream::routes().with_state(state.clone()));
    if let Some(database) = state.db.clone() {
        api = api.merge(symbols::routes().with_state(database.clone()));
        api = api.merge(backtest::routes().with_state(database.clone()));
        api = api.merge(backup::routes().with_state(state.clone()));
        api = api.merge(strategies::routes().with_state(database.clone()));
        api = api.merge(journal::routes().with_state(database.clone()));
        api = api.merge(journal_api::routes().with_state(database.clone()));
        let ai_state = ai_providers::AiState::new(
            database.clone(),
            state
                .auth
                .security
                .lock()
                .expect("security")
                .master_key
                .clone(),
        )
        .with_market(std::sync::Arc::new(ai_market::BinanceMarketSource::new(
            database.clone(),
        )));
        api = api.merge(
            news::routes()
                .with_state(news::NewsState::new(database.clone()).with_ai(ai_state.clone())),
        );
        api = api.merge(public::routes().with_state(database.clone()));
        api = api.merge(engine::routes().with_state(database.clone()));
        api = api.merge(ai_tuning_api::routes().with_state(database.clone()));
        api = api.merge(engine_actions::routes().with_state(state.clone()));
        api = api.merge(protection_api::routes().with_state(state.clone()));
        api = api.merge(evolution_api::routes().with_state(database.clone()));
        api = api.merge(reasoning::routes());
        api = api.merge(ai::routes().with_state(database.clone()));
        api = api.merge(ai_providers::routes().with_state(ai_state.clone()));
        api = api.merge(ai_analysis::routes().with_state(ai_state.clone()));
        api = api.merge(evolution_review::routes().with_state(ai_state));

        let system_state = system::SystemState::new(database.clone(), state.started_at)
            .with_auth(state.auth.clone());
        let system_state = match exec_client::global() {
            Some(client) => system_state.with_exec_client(client),
            None => system_state,
        };
        system_state.start_runtime_probe();
        system::spawn_alert_task(system_state.clone());
        api = api.merge(system::routes().with_state(system_state));
        api = api.merge(settings_api::routes().with_state(database.clone()));

    }

    let mut app = Router::new()
        .nest("/api/v1", api.fallback(unmigrated))
        .layer(axum::middleware::from_fn_with_state(
            state.auth.clone(),
            auth::force_password_change,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.auth,
            auth::require_token,
        ))
        .layer(CorsLayer::permissive());

    if let Some(dir) = frontend {
        let index = dir.join("index.html");
        info!("托管前端静态文件: {}", dir.display());
        let assets = dir.join("assets");
        if assets.is_dir() {
            app = app.nest_service("/assets", ServeDir::new(assets));
        }
        // index.html 不能在启动时固化：前端重新构建后资源 hash 会变，
        // 缓存旧 HTML 会让页面指向已不存在的 JS，整页白屏且控制台无报错。
        // 改为每次请求重读；读取失败时退回上一次成功的内容。
        let index_cache = std::sync::Arc::new(std::sync::Mutex::new(
            std::fs::read_to_string(&index).unwrap_or_default(),
        ));
        app = app.fallback(move |request: axum::extract::Request| {
            let index = index.clone();
            let index_cache = index_cache.clone();
            async move {
                let path = request.uri().path().to_string();
                if path.starts_with("/api/") {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({ "detail": "Not Found" })),
                    )
                        .into_response();
                }
                let body = match std::fs::read_to_string(&index) {
                    Ok(text) => {
                        let mut cache = index_cache.lock().expect("index cache");
                        *cache = text.clone();
                        text
                    }
                    Err(_) => index_cache.lock().expect("index cache").clone(),
                };
                axum::response::Html(body).into_response()
            }
        });
    }
    app
}

/// legacy 内核（已退役） 里存在、Rust 尚未迁移的路由统一回 501，并点明具体路径。
/// 退化成 404 会让前端把「还没做」误判成「接口不存在」。
async fn unmigrated(req: axum::extract::Request) -> impl IntoResponse {
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "detail": format!("接口尚未迁移到 Rust: {method} {path}"),
            "kernel": "rust",
        })),
    )
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(Health {
            ok: true,
            name: "NanoQuant",
            version: VERSION,
            uptime_sec: now() - state.started_at,
            kernel: "rust",
        }),
    )
}

/// 构造测试用的应用状态：带事件总线与 K 线中枢，但不连真实上游。
#[cfg(test)]
pub(crate) fn test_state(db: Option<db::Db>) -> AppState {
    let auth = default_auth();
    let master_key = auth.security.lock().expect("security").master_key.clone();
    AppState {
        started_at: now(),
        auth: auth.clone(),
        engine_control: runtime::EngineControl::new(
            db.clone().unwrap_or_else(|| db::Db {
                path: String::new(),
                conn: std::sync::Arc::new(std::sync::Mutex::new(
                    rusqlite::Connection::open_in_memory().expect("test database"),
                )),
            }),
            master_key,
        ),
        db,
        events: events::EventBus::new(None),
        klines: kline_stream::KlineHub::new(
            std::sync::Arc::new(kline_stream::BinanceWsSource),
            std::sync::Arc::new(|| "demo".to_string()),
        ),
    }
}

/// 当前交易环境：`engine_state.mode` 优先，其次 `engine` 配置组。
fn current_mode(db: Option<&db::Db>) -> String {
    let Some(db) = db else {
        return "demo".to_string();
    };
    let stored = db
        .conn
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
        .filter(|mode| !mode.is_empty());
    if let Some(mode) = stored {
        return mode;
    }
    db.setting("engine")
        .ok()
        .flatten()
        .and_then(|value| {
            value
                .get("mode")
                .and_then(|mode| mode.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| "demo".to_string())
}

fn default_auth() -> auth::AuthState {
    use std::sync::{Arc, Mutex};
    auth::AuthState {
        security: Arc::new(Mutex::new(auth::Security {
            username: "admin".into(),
            password_hash: String::new(),
            password_is_default: false,
            token_version: 0,
            session_ttl_hours: 12,
            master_key: Vec::new(),
            salt: "nanoquant-static-salt-v1".into(),
        })),
        throttle: Arc::new(Mutex::new(auth::LoginThrottle::new(8, 600))),
        db: None,
    }
}

/// 容器 HEALTHCHECK 用：本机探活地址。走 127.0.0.1 而不是 0.0.0.0。
fn healthcheck_target(port: u16) -> String {
    format!("http://127.0.0.1:{port}/api/v1/health")
}

pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut terminate) => tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        },
        Err(error) => {
            tracing::warn!(%error, "无法监听 SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
    info!("收到停止信号，正在退出");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_reports_rust_kernel() {
        let app = router(test_state(None), None);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["kernel"], "rust");
        assert_eq!(body["name"], "NanoQuant");
    }

    #[tokio::test]
    async fn private_api_requires_a_valid_token_while_health_remains_public() {
        let app = router(test_state(None), None);
        let private = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/engine/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(private.status(), StatusCode::UNAUTHORIZED);
        let login = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let me = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::UNAUTHORIZED);
        let state = test_auth();
        state.security.lock().unwrap().password_is_default = true;
        let bearer = test_bearer(&state);
        let forced = router(
            AppState {
                auth: state,
                ..test_state(None)
            },
            None,
        );
        let change = forced
            .oneshot(
                Request::builder()
                    .uri("/api/v1/engine/state")
                    .header("authorization", bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(change.status(), StatusCode::FORBIDDEN);
        let health = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }

    fn test_auth() -> auth::AuthState {
        let state = default_auth();
        state.security.lock().unwrap().master_key = vec![17; 32];
        state
    }

    fn test_bearer(state: &auth::AuthState) -> String {
        let sec = state.security.lock().unwrap();
        let (token, _) = auth::issue_token(
            &sec.username,
            1,
            sec.token_version,
            &sec.master_key,
            &sec.salt,
            now(),
        );
        format!("Bearer {token}")
    }

    #[tokio::test]
    async fn metrics_show_account_data_only_to_the_current_session() {
        let auth = test_auth();
        let valid = test_bearer(&auth);
        let unrelated = test_auth();
        unrelated.security.lock().unwrap().master_key = vec![91; 32];
        let invalid = test_bearer(&unrelated);
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE equity_points(ts INTEGER, wallet REAL); INSERT INTO equity_points VALUES(1, 42.5);").unwrap();
        let db = db::Db {
            path: String::new(),
            conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        };
        let app = router(
            AppState {
                auth: auth.clone(),
                ..test_state(Some(db))
            },
            None,
        );
        for (bearer, expected) in [
            (None, false),
            (Some(&invalid), false),
            (Some(&valid), true),
            (Some(&valid), false),
        ] {
            let mut request = Request::builder().uri("/api/v1/metrics");
            if let Some(bearer) = bearer {
                request = request.header("authorization", bearer.as_str());
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let text = std::str::from_utf8(&body).unwrap();
            assert!(text.contains("nanoquant_up 1"));
            assert_eq!(
                text.contains("nanoquant_account_equity_usdt 42.5"),
                expected
            );
            assert_eq!(text.contains("nanoquant_account_open_positions"), expected);
            if expected {
                auth.security.lock().unwrap().token_version += 1;
            }
        }
    }

    #[tokio::test]
    async fn router_starts_with_database_and_keeps_journal_routes() {
        let auth = test_auth();
        let bearer = test_bearer(&auth);
        let db = db::Db {
            path: String::new(),
            conn: std::sync::Arc::new(std::sync::Mutex::new(
                rusqlite::Connection::open_in_memory().unwrap(),
            )),
        };
        let app = router(
            AppState {
                auth,
                ..test_state(Some(db))
            },
            None,
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/journal")
                    .header("authorization", bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn healthcheck_targets_local_health_endpoint() {
        assert_eq!(
            healthcheck_target(8899),
            "http://127.0.0.1:8899/api/v1/health"
        );
    }

    #[tokio::test]
    async fn unmigrated_api_routes_answer_501_with_detail() {
        let auth = test_auth();
        let bearer = test_bearer(&auth);
        let app = router(
            AppState {
                auth,
                ..test_state(None)
            },
            None,
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/engine/cycle-now")
                    .header("authorization", bearer)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_IMPLEMENTED,
            "未迁移接口不能退化成 404"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let detail = body["detail"].as_str().unwrap_or_default();
        assert!(
            detail.contains("engine/cycle-now"),
            "detail 应点明具体路径，实际: {detail}"
        );
    }

    #[tokio::test]
    async fn all_engine_contract_endpoints_reject_missing_token_with_401() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::migrate(&conn).unwrap();
        let db = db::Db {
            path: String::new(),
            conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        };
        let app = router(
            AppState {
                auth: test_auth(),
                ..test_state(Some(db))
            },
            None,
        );
        let cases = [
            ("GET", "/api/v1/engine/status"),
            ("GET", "/api/v1/engine/admission"),
            ("GET", "/api/v1/engine/trade-mode"),
            ("POST", "/api/v1/engine/trade-mode"),
            ("POST", "/api/v1/engine/control"),
            ("GET", "/api/v1/engine/execution"),
            ("POST", "/api/v1/engine/cycle-now"),
            ("POST", "/api/v1/engine/flatten"),
            ("POST", "/api/v1/engine/reset-anchors"),
            ("GET", "/api/v1/engine/positions"),
            ("POST", "/api/v1/engine/positions/1/protect"),
            ("POST", "/api/v1/engine/positions/1/close"),
            ("GET", "/api/v1/engine/orders"),
            ("GET", "/api/v1/engine/protection"),
            ("DELETE", "/api/v1/engine/protection/1"),
            ("GET", "/api/v1/engine/trades"),
            ("GET", "/api/v1/engine/signals"),
            ("GET", "/api/v1/engine/equity"),
            ("GET", "/api/v1/engine/stats"),
        ];
        for (method, path) in cases {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path} 必须要求鉴权"
            );
        }
    }
}
