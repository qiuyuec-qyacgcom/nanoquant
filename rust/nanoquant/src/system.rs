//! 系统只读接口。事件列表对齐 `api/routes/system.py` 的 `list_events`。

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::auth::{self, AuthState};
use crate::db::Db;

#[derive(Clone)]
pub struct SystemState {
    pub db: Db,
    pub started_at: i64,
    pub free_bytes: fn(&std::path::Path) -> u64,
    pub auth: Option<AuthState>,
    runtime_probe: RuntimeProbe,
    /// 日志目录：服务日志接口只允许读这个目录里的文件。
    logs_dir: std::path::PathBuf,
    logs_path: std::path::PathBuf,
    archives_dir: std::path::PathBuf,
    /// 告警冷却记账表：同一类别在冷却窗口内只推一次。
    alert_last: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, i64>>>,
    /// 摘要模式缓冲：开启后 warn 级事件先攒着，到点合并成一条推送。
    alert_digest: std::sync::Arc<std::sync::Mutex<Vec<AlertEvent>>>,
    exec: std::sync::Arc<std::sync::Mutex<ExecStats>>,
    exec_client: Option<std::sync::Arc<crate::exec_client::ExecClient>>,
}

/// 执行内核的实测计数与往返耗时样本。
///
/// 每个数字都必须来自真实观测：探活成功记 ok、失败记 errors、
/// 已连通但没配 Secret（没法比对签名）记 skipped。
/// 签名逐字节比对不在进程内做（主服务用 Rust 内核自己签名），
/// 真实签名比对用 `ops verify-exec-core`。
#[derive(Clone, Default)]
struct ExecStats {
    checks: i64,
    ok: i64,
    errors: i64,
    skipped: i64,
    /// 探活往返耗时样本（毫秒），只保留最近若干个用于算分位数。
    latencies_ms: Vec<f64>,
}

/// 往返耗时样本保留上限：够算分位数，又不会把内存吃大。
const EXEC_LATENCY_SAMPLES: usize = 64;

/// 执行内核自检的三种结论。
///
/// 执行内核是**默认必选**组件：未连接就是故障，不再有「没部署也算通过」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExecCheck {
    /// 通过：已连接且配了 Secret（可逐字节比对）。
    Ok,
    /// 跳过：端点可达但没配交易所 Secret，只能探活、不能比对签名。
    Skipped,
    /// 失败：未连接（默认执行内核必选）。
    Error,
}

impl ExecCheck {
    fn as_str(self) -> &'static str {
        match self {
            ExecCheck::Ok => "ok",
            ExecCheck::Skipped => "skipped",
            ExecCheck::Error => "error",
        }
    }
}

impl ExecStats {
    /// 记录一次探活往返耗时。任何触发探活的地方都记，样本才有意义。
    fn observe_latency(&mut self, latency_ms: Option<f64>) {
        let Some(ms) = latency_ms else { return };
        self.latencies_ms.push(ms);
        if self.latencies_ms.len() > EXEC_LATENCY_SAMPLES {
            self.latencies_ms.remove(0);
        }
    }

    /// 记一次自检：三种结论分别落到「成功 / 跳过 / 失败」三个计数里。
    fn record_check(&mut self, outcome: ExecCheck) {
        self.checks += 1;
        match outcome {
            ExecCheck::Ok => self.ok += 1,
            ExecCheck::Skipped => self.skipped += 1,
            ExecCheck::Error => self.errors += 1,
        }
    }

    /// 最近一次往返耗时与分位数。没有样本时返回 null，前端显示「—」而不是 0。
    fn latency_view(&self) -> Value {
        let mut ordered = self.latencies_ms.clone();
        ordered.sort_by(f64::total_cmp);
        let percentile = |q: f64| {
            if ordered.is_empty() {
                Value::Null
            } else {
                json!(round(
                    ordered[((ordered.len() as f64 * q) as usize).min(ordered.len() - 1)],
                    2
                ))
            }
        };
        json!({
            "last": self.latencies_ms.last().copied().map(|value| round(value, 2)),
            "p50": percentile(0.50),
            "p95": percentile(0.95),
            "p99": percentile(0.99),
            "samples": ordered.len(),
        })
    }
}

impl SystemState {
    pub fn new(db: Db, started_at: i64) -> Self {
        let home = home_dir();
        let logs_dir = std::env::var_os("NANOQUANT_LOG_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join("logs"));
        Self {
            db,
            started_at,
            free_bytes: disk_free_bytes,
            auth: None,
            runtime_probe: RuntimeProbe::new(started_at),
            logs_dir: logs_dir.clone(),
            logs_path: logs_dir.join("nanoquant.log"),
            archives_dir: home.join("data").join("archives"),
            alert_last: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            alert_digest: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            exec: std::sync::Arc::new(std::sync::Mutex::new(ExecStats::default())),
            exec_client: None,
        }
    }

    pub fn with_auth(mut self, auth: AuthState) -> Self {
        self.auth = Some(auth);
        self
    }

    pub fn with_exec_client(
        mut self,
        client: std::sync::Arc<crate::exec_client::ExecClient>,
    ) -> Self {
        self.exec_client = Some(client);
        self
    }

    #[cfg(test)]
    fn with_archives_dir(mut self, path: std::path::PathBuf) -> Self {
        self.archives_dir = path;
        self
    }

    pub fn start_runtime_probe(&self) {
        let probe = self.runtime_probe.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(50));
            let mut expected = tokio::time::Instant::now() + std::time::Duration::from_millis(50);
            loop {
                interval.tick().await;
                let now = tokio::time::Instant::now();
                probe.record(now.duration_since(expected).as_secs_f64() * 1000.0);
                expected = now + std::time::Duration::from_millis(50);
            }
        });
    }
}

fn home_dir() -> std::path::PathBuf {
    std::env::var_os("NANOQUANT_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
}

#[derive(Clone)]
struct RuntimeProbe {
    started_at: i64,
    lags_ms: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<f64>>>,
}

impl RuntimeProbe {
    fn new(started_at: i64) -> Self {
        Self {
            started_at,
            lags_ms: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(4000),
            )),
        }
    }

    fn record(&self, lag_ms: f64) {
        if let Ok(mut lags) = self.lags_ms.lock() {
            if lags.len() == 4000 {
                lags.pop_front();
            }
            lags.push_back(lag_ms.max(0.0));
        }
    }

    fn summary(&self, now: i64) -> Value {
        let lags = self
            .lags_ms
            .lock()
            .map(|values| values.iter().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut ordered = lags.clone();
        ordered.sort_by(f64::total_cmp);
        let percentile = |q: f64| {
            if ordered.is_empty() {
                0.0
            } else {
                ordered[((ordered.len() as f64 * q) as usize).min(ordered.len() - 1)]
            }
        };
        json!({
            "uptime_sec": if self.started_at > 0 { now.saturating_sub(self.started_at).max(0) } else { 0 },
            "samples": lags.len(),
            "loop_lag_ms": {
                "p50": round(percentile(0.50), 2),
                "p95": round(percentile(0.95), 2),
                "p99": round(percentile(0.99), 2),
                "max": round(lags.iter().copied().fold(0.0, f64::max), 2),
            },
            "gc": {
                "cycles": 0,
                "pause_ms_p50": 0.0,
                "pause_ms_p99": 0.0,
                "pause_ms_max": 0.0,
                "pause_total_ms": 0.0,
                "supported": false,
                "reason": "not_applicable_rust",
            },
            "timer_note": "Rust runtime probe uses a 50ms target interval; scheduler granularity may affect measured lag",
            "blocked_over_20ms": lags.iter().filter(|lag| **lag > 20.0).count(),
            "blocked_over_50ms": lags.iter().filter(|lag| **lag > 50.0).count(),
        })
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<i64>,
    limit: Option<i64>,
    level: Option<String>,
}

#[derive(Deserialize)]
struct LogsQuery {
    lines: Option<usize>,
    keyword: Option<String>,
    path: Option<String>,
}

pub fn routes() -> Router<SystemState> {
    Router::new()
        .route("/system/events", get(list_events))
        .route("/system/logs", get(logs))
        .route("/system/info", get(info))
        .route("/system/diagnostics", get(diagnostics))
        .route("/system/runtime", get(runtime_probe_endpoint))
        .route("/system/dead-letters", get(dead_letters))
        .route("/system/dead-letters/{id}/replay", post(replay_one_handler))
        .route("/system/dead-letters/{id}/ignore", post(ignore_handler))
        .route("/system/dead-letters/replay-all", post(replay_all_handler))
        .route("/system/write-health", get(write_health))
        .route("/system/archives", get(list_archives))
        .route("/system/archives/{name}", get(preview_archive))
        .route("/system/alerts/logs", get(alert_logs))
        .route("/system/alerts/test", post(alert_test))
        .route("/system/exec-core", get(exec_core))
        .route("/system/exec-core/self-test", post(exec_self_test))
        .route("/system/exec-core/reconcile", get(exec_reconcile))
        .route("/system/exec-core/reset", post(exec_reset))
        .route(
            "/settings/credentials/exchange",
            get(get_exchange_credentials)
                .post(save_exchange_credentials)
                .delete(clear_exchange_credentials),
        )
        .route("/metrics", get(metrics))
}

const AUTO_MAX_ATTEMPTS: i64 = 3;

#[derive(Deserialize)]
struct DeadLettersQuery {
    #[serde(default = "default_dead_limit")]
    limit: i64,
    #[serde(default)]
    include_resolved: bool,
}

fn default_dead_limit() -> i64 {
    100
}

async fn dead_letters(
    State(state): State<SystemState>,
    Query(query): Query<DeadLettersQuery>,
) -> Json<Value> {
    let limit = query.limit.clamp(1, 500);
    let Ok(conn) = state.db.read() else {
        return Json(json!({"items": [], "summary": {}, "error": "数据库不可用"}));
    };
    let mut stmt = conn
        .prepare(
            "SELECT id, ts, kind, payload, error, attempts, auto_attempts, resolved \
             FROM write_failures WHERE (?1 = 1 OR resolved = 0) ORDER BY ts DESC LIMIT ?2",
        )
        .expect("prepare dead letters");
    let items: Vec<Value> = stmt
        .query_map((i64::from(query.include_resolved), limit), |row| {
            let raw: Option<String> = row.get(3)?;
            let payload: Value = raw
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or(Value::Null);
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "ts": row.get::<_, i64>(1)?,
                "kind": row.get::<_, String>(2)?,
                "payload": sanitize_payload(&payload),
                "error": row.get::<_, String>(4)?,
                "attempts": row.get::<_, i64>(5)?,
                "auto_attempts": row.get::<_, i64>(6)?,
                "resolved": row.get::<_, i64>(7)? != 0,
            }))
        })
        .expect("query dead letters")
        .collect::<Result<_, _>>()
        .expect("dead letter rows");
    Json(json!({"items": items, "summary": dead_letter_summary(&conn)}))
}

/// 载荷脱敏：密钥类键统一替换，字符串截断，避免面板把凭证回显。
fn sanitize_payload(value: &Value) -> Value {
    const SECRET_KEYS: &[&str] = &[
        "key",
        "secret",
        "token",
        "password",
        "passwd",
        "credential",
        "api_key",
        "signature",
    ];
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, item) in map {
                let lower = key.to_lowercase();
                if SECRET_KEYS.iter().any(|needle| lower.contains(needle)) {
                    out.insert(key.clone(), json!("***"));
                } else {
                    out.insert(key.clone(), sanitize_payload(item));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().take(50).map(sanitize_payload).collect()),
        Value::String(text) => json!(text.chars().take(500).collect::<String>()),
        other => other.clone(),
    }
}

fn dead_letter_summary(conn: &rusqlite::Connection) -> Value {
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM write_failures WHERE resolved = 0",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let stuck: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM write_failures WHERE resolved = 0 AND auto_attempts >= ?1",
            [AUTO_MAX_ATTEMPTS],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let last_ts: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(ts),0) FROM write_failures",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let mut stmt = match conn.prepare(
        "SELECT kind, COUNT(*) FROM write_failures WHERE resolved = 0 \
         GROUP BY kind ORDER BY COUNT(*) DESC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => {
            return json!({"pending": pending, "stuck": stuck, "last_ts": last_ts, "by_kind": []})
        }
    };
    let by_kind: Vec<Value> = stmt
        .query_map([], |row| {
            Ok(json!({
                "kind": row.get::<_, String>(0)?,
                "count": row.get::<_, i64>(1)?,
            }))
        })
        .expect("query dead summary")
        .collect::<Result<_, _>>()
        .expect("dead summary rows");
    json!({"pending": pending, "stuck": stuck, "last_ts": last_ts, "by_kind": by_kind})
}

async fn replay_one_handler(State(state): State<SystemState>, Path(id): Path<i64>) -> Json<Value> {
    Json(replay_one(&state.db, id, false))
}

async fn ignore_handler(State(state): State<SystemState>, Path(id): Path<i64>) -> Json<Value> {
    let conn = state.db.lock();
    let changed = conn
        .execute("UPDATE write_failures SET resolved=1 WHERE id=?1", [id])
        .unwrap_or(0);
    if changed == 0 {
        return json!({"ok": false, "status": "missing", "detail": "死信不存在"}).into();
    }
    record_db_event(&conn, "warn", format!("死信 #{id} 已被人工忽略"));
    json!({"ok": true, "status": "ignored", "detail": "已标记为处理"}).into()
}

async fn replay_all_handler(State(state): State<SystemState>) -> Json<Value> {
    let ids: Vec<i64> = {
        let conn = state.db.lock();
        let mut stmt = conn
            .prepare("SELECT id FROM write_failures WHERE resolved = 0 ORDER BY ts ASC LIMIT 50")
            .expect("prepare replay all");
        stmt.query_map([], |row| row.get(0))
            .expect("query replay all")
            .collect::<Result<_, _>>()
            .expect("replay all rows")
    };
    let mut stats =
        json!({"total": ids.len(), "replayed": 0, "skipped": 0, "failed": 0, "unsupported": 0});
    for id in ids {
        let result = replay_one(&state.db, id, false);
        let status = result["status"].as_str().unwrap_or("failed");
        let key = match status {
            "replayed" => "replayed",
            "skipped" => "skipped",
            "unsupported" => "unsupported",
            _ => "failed",
        };
        let next = stats[key].as_i64().unwrap_or(0) + 1;
        stats[key] = json!(next);
    }
    Json(stats)
}

/// 单条重放。幂等靠 `client_order_id`：订单已落库即标记处理，绝不重复记账。
fn replay_one(db: &Db, id: i64, auto: bool) -> Value {
    let conn = db.lock();
    let row = conn
        .query_row(
            "SELECT kind, payload, resolved FROM write_failures WHERE id=?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)? != 0,
                ))
            },
        )
        .ok();
    let Some((kind, payload, resolved)) = row else {
        return json!({"ok": false, "status": "missing", "detail": "死信不存在"});
    };
    if resolved {
        return json!({"ok": false, "status": "resolved", "detail": "该死信已处理"});
    }
    let payload: Value = payload
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null);
    let client_order_id = payload["args"]
        .as_array()
        .and_then(|args| args.first())
        .and_then(|first| first["client_order_id"].as_str())
        .or_else(|| payload["fill"]["client_order_id"].as_str())
        .unwrap_or("");
    if !client_order_id.is_empty() {
        let applied: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM orders WHERE client_order_id=?1)",
                [client_order_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if applied {
            let _ = conn.execute("UPDATE write_failures SET resolved=1 WHERE id=?1", [id]);
            record_db_event(
                &conn,
                "info",
                format!("死信 #{id} 幂等跳过 (client_order_id 已落库)"),
            );
            return json!({
                "ok": true,
                "status": "skipped",
                "detail": "同 client_order_id 已落库, 直接标记为已处理",
            });
        }
    }
    let replay = match kind.as_str() {
        "execution_demo_fill" | "execution_live_fill" => replay_fill(&conn, &payload, &kind),
        _ => Err(format!("不支持重放的类型: {kind}")),
    };
    match replay {
        Ok(true) => {
            let _ = conn.execute("UPDATE write_failures SET resolved=1 WHERE id=?1", [id]);
            record_db_event(&conn, "info", format!("死信 #{id} 重放成功"));
            json!({"ok": true, "status": "replayed", "detail": "已重放"})
        }
        Ok(false) => {
            let _ = conn.execute(
                "UPDATE write_failures SET resolved=1, attempts=attempts+1 WHERE id=?1",
                [id],
            );
            record_db_event(&conn, "info", format!("死信 #{id} 幂等跳过 (成交已结算)"));
            json!({"ok": true, "status": "skipped", "detail": "成交已结算, 直接标记为已处理"})
        }
        Err(detail) => {
            if auto {
                let _ = conn.execute(
                    "UPDATE write_failures SET attempts=attempts+1, auto_attempts=auto_attempts+1 \
                     WHERE id=?1",
                    [id],
                );
            } else {
                let _ = conn.execute(
                    "UPDATE write_failures SET attempts=attempts+1 WHERE id=?1",
                    [id],
                );
            }
            let unsupported = detail.starts_with("不支持重放");
            json!({
                "ok": false,
                "status": if unsupported { "unsupported" } else { "failed" },
                "detail": detail,
            })
        }
    }
}

fn replay_fill(conn: &rusqlite::Connection, payload: &Value, kind: &str) -> Result<bool, String> {
    let mode = payload["mode"].as_str().unwrap_or("demo");
    let fee_rate = payload["fee_rate"].as_f64().unwrap_or(0.0005);
    let now = payload["now"].as_i64().unwrap_or_else(crate::now);
    let fill = &payload["fill"];
    let fill = crate::execution::DemoFill {
        client_order_id: fill["client_order_id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        exchange_order_id: fill["exchange_order_id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        symbol: fill["symbol"].as_str().unwrap_or_default().to_string(),
        side: fill["side"].as_str().unwrap_or_default().to_string(),
        qty: fill["qty"].as_f64().unwrap_or(0.0),
        price: fill["price"].as_f64().unwrap_or(0.0),
        reduce_only: fill["reduce_only"].as_bool().unwrap_or(true),
        reason: fill["reason"].as_str().unwrap_or("replay").to_string(),
    };
    if mode == "demo" && kind == "execution_demo_fill" {
        return crate::execution::apply_demo_fill(conn, "demo", now, fee_rate, &fill)
            .map(|(applied, _)| applied);
    }
    if mode == "live" && kind == "execution_live_fill" {
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let applied = crate::execution::apply_live_fill_tx(&tx, "live", now, fee_rate, &fill)?.0;
        tx.commit().map_err(|e| e.to_string())?;
        return Ok(applied);
    }
    Err(format!("不支持重放的类型: {kind} (mode={mode})"))
}

fn record_db_event(conn: &rusqlite::Connection, level: &str, message: String) {
    let _ = conn.execute(
        "INSERT INTO engine_events(ts, level, category, message, payload) \
         VALUES(?1, ?2, 'db', ?3, '{}')",
        (crate::now(), level, message),
    );
}

async fn write_health(State(state): State<SystemState>) -> Json<Value> {
    let conn = state.db.lock();
    let dead = dead_letter_summary(&conn);
    let total = engine_int(&conn, "write_total");
    let slow = engine_int(&conn, "write_slow_count");
    let max_ms = engine_int(&conn, "write_max_ms") as f64;
    let pending = dead["pending"].as_i64().unwrap_or(0);
    let slow_ratio = if total > 0 {
        round(slow as f64 / total as f64 * 100.0, 2)
    } else {
        0.0
    };
    let backlog_warn = 50;
    let slow_ms = 200.0;
    let verdict = if pending > backlog_warn {
        "队列积压, 检查是否有慢查询或写事务未提交"
    } else if pending > 0 && slow > 0 {
        "有死信且存在慢写, 优先排查磁盘 IO 与锁竞争"
    } else if pending > 0 {
        "有死信但无慢写, 更可能是并发写冲突或字段约束"
    } else if max_ms > slow_ms {
        "偶发慢写但已自动恢复, 继续观察"
    } else {
        "正常"
    };
    Json(json!({
        "queue": {"pending": pending, "total": total, "slow": slow, "max_ms": max_ms},
        "dead_letters": dead,
        "slow_ratio_pct": slow_ratio,
        "thresholds": {"backlog": backlog_warn, "slow_ms": slow_ms},
        "verdict": verdict,
    }))
}

async fn list_archives(State(state): State<SystemState>) -> Json<Value> {
    let mut items = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&state.archives_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !archive_name_ok(&name) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let (kind, date) = name
                .trim_end_matches(".jsonl.gz")
                .rsplit_once('-')
                .map(|(kind, date)| (kind.to_string(), date.to_string()))
                .unwrap_or_default();
            items.push(json!({
                "name": name,
                "kind": kind,
                "date": date,
                "size": meta.len(),
                "mtime": meta.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok()).map(|elapsed| elapsed.as_secs() as i64).unwrap_or(0),
            }));
        }
    }
    items.sort_by_key(|item| std::cmp::Reverse(item["mtime"].as_i64().unwrap_or(0)));
    items.truncate(50);
    Json(json!({"items": items, "dir": state.archives_dir.to_string_lossy()}))
}

#[derive(Deserialize)]
struct ArchiveQuery {
    #[serde(default = "default_archive_limit")]
    limit: i64,
}

fn default_archive_limit() -> i64 {
    20
}

async fn preview_archive(
    State(state): State<SystemState>,
    Path(name): Path<String>,
    Query(query): Query<ArchiveQuery>,
) -> axum::response::Response {
    if !archive_name_ok(&name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "文件名不合法"})),
        )
            .into_response();
    }
    let dir =
        std::fs::canonicalize(&state.archives_dir).unwrap_or_else(|_| state.archives_dir.clone());
    let path = state.archives_dir.join(&name);
    let Ok(canonical) = std::fs::canonicalize(&path) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"detail": "归档文件不存在"})),
        )
            .into_response();
    };
    if !canonical.starts_with(&dir) {
        return (StatusCode::BAD_REQUEST, Json(json!({"detail": "路径越界"}))).into_response();
    }
    let Ok(file) = std::fs::File::open(&canonical) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": "归档读取失败"})),
        )
            .into_response();
    };
    let mut decoder = flate2::read::GzDecoder::new(file);
    let mut content = String::new();
    if let Err(error) = std::io::Read::read_to_string(&mut decoder, &mut content) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": format!("归档读取失败: {error}")})),
        )
            .into_response();
    }
    let limit = query.limit.clamp(1, 200) as usize;
    let lines: Vec<&str> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let mut rows: Vec<Value> = lines
        .iter()
        .rev()
        .take(limit)
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    rows.reverse();
    Json(json!({"name": name, "items": rows, "count": rows.len()})).into_response()
}

fn archive_name_ok(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl.gz") else {
        return false;
    };
    let Some((kind, date)) = stem.rsplit_once('-') else {
        return false;
    };
    !kind.is_empty()
        && kind.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_')
        && date.len() == 8
        && date.chars().all(|ch| ch.is_ascii_digit())
}

/// 归档指定表的旧行：gzip JSONL 落盘后再删除。表名白名单，避免任意 SQL。
#[cfg(test)]
pub(crate) fn archive_table(
    db: &Db,
    kind: &str,
    table: &str,
    before_ts: i64,
    dir: &std::path::Path,
) -> Result<(usize, String), String> {
    if !["engine_events", "signals"].contains(&table) {
        return Err("不允许归档该表".to_string());
    }
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let mut columns_stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let columns: Vec<String> = columns_stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<String>, _>>()
        .map_err(|error| error.to_string())?;
    let mut rows_stmt = conn
        .prepare(&format!(
            "SELECT * FROM {table} WHERE ts < ?1 ORDER BY ts ASC, rowid ASC LIMIT 5000"
        ))
        .map_err(|error| error.to_string())?;
    let rows: Vec<Value> = rows_stmt
        .query_map([before_ts], |row| {
            let mut map = Map::new();
            for (index, column) in columns.iter().enumerate() {
                map.insert(column.clone(), sql_value(row, index));
            }
            Ok(Value::Object(map))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<Value>, _>>()
        .map_err(|error| error.to_string())?;
    if rows.is_empty() {
        return Ok((0, String::new()));
    }
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let name = format!(
        "{kind}-{}.jsonl.gz",
        date_stamp(before_ts.max(crate::now()))
    );
    let file = std::fs::File::create(dir.join(&name)).map_err(|error| error.to_string())?;
    let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    for row in &rows {
        use std::io::Write;
        writeln!(
            encoder,
            "{}",
            serde_json::to_string(row).map_err(|error| error.to_string())?
        )
        .map_err(|error| error.to_string())?;
    }
    encoder.finish().map_err(|error| error.to_string())?;
    let deleted = conn
        .execute(
            &format!(
                "DELETE FROM {table} WHERE rowid IN \
                 (SELECT rowid FROM {table} WHERE ts < ?1 ORDER BY ts ASC, rowid ASC LIMIT 5000)"
            ),
            [before_ts],
        )
        .map_err(|error| error.to_string())?;
    Ok((deleted, name))
}

#[cfg(test)]
fn sql_value(row: &rusqlite::Row, index: usize) -> Value {
    use rusqlite::types::ValueRef;
    match row.get_ref(index) {
        Ok(ValueRef::Null) => Value::Null,
        Ok(ValueRef::Integer(value)) => json!(value),
        Ok(ValueRef::Real(value)) => json!(value),
        Ok(ValueRef::Text(text)) => json!(String::from_utf8_lossy(text)),
        Ok(ValueRef::Blob(blob)) => json!(format!("<blob {} bytes>", blob.len())),
        Err(_) => Value::Null,
    }
}

#[cfg(test)]
fn date_stamp(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}{month:02}{day:02}")
}

struct AlertCfg {
    /// 总开关。默认关闭，避免刚装好就往群里推历史告警。
    enabled: bool,
    /// 最低告警级别：info / warn / error。
    min_level: String,
    /// 自定义模板，可含 {message} {level} {category} 占位符；空则用默认文案。
    template: String,
    webhook: String,
    error_webhook: String,
    telegram_token: String,
    telegram_chat_id: String,
    /// 同类别冷却窗口（秒）。
    cooldown_sec: i64,
    /// 静默时段，如 `23:00-07:00`，支持跨天；空串表示不静默。
    quiet_hours: String,
    /// 摘要模式：warn 级告警不即时推，按间隔合并成一条。
    digest_enabled: bool,
    /// 摘要间隔（分钟），5–1440，缺省 60。
    digest_minutes: i64,
}

impl AlertCfg {
    /// 某级别要发到哪些 webhook：错误级别走独立地址，没配置则回落到普通地址。
    fn webhook_urls(&self, level: &str) -> Vec<String> {
        let source = if level == "error" && !self.error_webhook.is_empty() {
            &self.error_webhook
        } else {
            &self.webhook
        };
        source
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn telegram_enabled(&self) -> bool {
        !self.telegram_token.is_empty() && !self.telegram_chat_id.is_empty()
    }
}

fn alert_cfg(conn: &rusqlite::Connection) -> AlertCfg {
    let raw = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='system'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null);
    let text = |key: &str| {
        raw.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let min_level = text("alert_min_level");
    AlertCfg {
        enabled: raw
            .get("alert_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        min_level: if min_level.is_empty() {
            "warn".to_string()
        } else {
            min_level
        },
        template: text("alert_template"),
        webhook: text("alert_webhook_url"),
        error_webhook: text("alert_error_webhook_url"),
        telegram_token: text("alert_telegram_token"),
        telegram_chat_id: text("alert_telegram_chat_id"),
        cooldown_sec: raw
            .get("alert_cooldown_sec")
            .and_then(Value::as_i64)
            .unwrap_or(120)
            .clamp(10, 3600),
        quiet_hours: text("alert_quiet_hours"),
        digest_enabled: raw
            .get("alert_digest_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        digest_minutes: raw
            .get("alert_digest_minutes")
            .and_then(Value::as_i64)
            .unwrap_or(60)
            .clamp(5, 1440),
    }
}

#[derive(Deserialize)]
struct AlertQuery {
    #[serde(default = "default_alert_limit")]
    limit: i64,
    #[serde(default = "default_alert_level")]
    level: String,
}

fn default_alert_limit() -> i64 {
    20
}

fn default_alert_level() -> String {
    "warn".to_string()
}

async fn alert_logs(
    State(state): State<SystemState>,
    Query(query): Query<AlertQuery>,
) -> Json<Value> {
    let limit = query.limit.clamp(1, 200);
    let conn = state.db.lock();
    let mut stmt = conn
        .prepare(
            "SELECT id, ts, level, category, message, channels, ok, error \
             FROM alert_logs ORDER BY ts DESC LIMIT ?1",
        )
        .expect("prepare alert logs");
    let items: Vec<Value> = stmt
        .query_map([limit], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "ts": row.get::<_, i64>(1)?,
                "level": row.get::<_, String>(2)?,
                "category": row.get::<_, String>(3)?,
                "message": row.get::<_, String>(4)?,
                "channels": row.get::<_, String>(5)?,
                "ok": row.get::<_, i64>(6)? != 0,
                "error": row.get::<_, String>(7)?,
            }))
        })
        .expect("query alert logs")
        .collect::<Result<_, _>>()
        .expect("alert log rows");
    Json(json!({"items": items}))
}

async fn alert_test(
    State(state): State<SystemState>,
    Query(query): Query<AlertQuery>,
) -> axum::response::Response {
    let level = query.level.to_lowercase();
    if !matches!(level.as_str(), "info" | "warn" | "error") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "level 必须是 info/warn/error"})),
        )
            .into_response();
    }
    let cfg = {
        let conn = state.db.lock();
        alert_cfg(&conn)
    };
    let urls = cfg.webhook_urls(&level);
    let telegram = cfg.telegram_enabled();
    if urls.is_empty() && !telegram {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "未配置任何告警通道（Webhook 或 Telegram）"})),
        )
            .into_response();
    }
    let text = format!(
        "🟠 [NanoQuant] {} · test\n这是一条 {} 测试告警：收到即表示配置正确。",
        level.to_uppercase(),
        level
    );
    let results = send_alerts(&urls, &cfg, &text).await;
    let ok = !results.is_empty() && results.iter().all(|item| item["ok"] == true);
    if let Ok(conn) = state.db.conn.lock() {
        let channels = results
            .iter()
            .filter_map(|item| item["channel"].as_str())
            .collect::<Vec<_>>()
            .join(",");
        let error = results
            .iter()
            .filter(|item| item["ok"] != true)
            .filter_map(|item| item["error"].as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let _ = conn.execute(
            "INSERT INTO alert_logs(ts, level, category, message, channels, ok, error) \
             VALUES(?1, ?2, 'test', ?3, ?4, ?5, ?6)",
            (
                crate::now(),
                &level,
                &text,
                channels,
                i64::from(ok),
                error.chars().take(500).collect::<String>(),
            ),
        );
    }
    Json(json!({"ok": ok, "results": results})).into_response()
}

async fn send_alerts(urls: &[String], cfg: &AlertCfg, text: &str) -> Vec<Value> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return vec![json!({"channel": "webhook", "ok": false, "error": error.to_string()})]
        }
    };
    let mut results = Vec::new();
    for url in urls {
        let payload = json!({"msgtype": "text", "text": {"content": text}});
        let result = match client.post(url).json(&payload).send().await {
            Ok(response) => {
                let status = response.status();
                json!({"channel": "webhook", "ok": status.is_success(), "status": status.as_u16()})
            }
            Err(error) => json!({"channel": "webhook", "ok": false, "error": error.to_string()}),
        };
        results.push(result);
    }
    if !cfg.telegram_token.is_empty() && !cfg.telegram_chat_id.is_empty() {
        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            cfg.telegram_token
        );
        let payload = json!({"chat_id": cfg.telegram_chat_id, "text": text});
        let result = match client.post(&url).json(&payload).send().await {
            Ok(response) => {
                let status = response.status();
                json!({"channel": "telegram", "ok": status.is_success(), "status": status.as_u16()})
            }
            Err(error) => json!({"channel": "telegram", "ok": false, "error": error.to_string()}),
        };
        results.push(result);
    }
    results
}

/// 告警冷却：同一类别在窗口内只放行一次。返回 true 表示允许发送并已记账。
fn alert_allowed(
    last: &mut std::collections::HashMap<String, i64>,
    key: &str,
    now: i64,
    cooldown_sec: i64,
) -> bool {
    let previous = last.get(key).copied().unwrap_or(0);
    if previous > 0 && now - previous < cooldown_sec.max(10) {
        return false;
    }
    last.insert(key.to_string(), now);
    true
}

/// 静默时段（支持跨天，如 23:00-07:00）；空串表示不静默。
fn in_quiet_hours(spec: &str, minutes_of_day: i64) -> bool {
    let Some((start, end)) = spec.trim().split_once('-') else {
        return false;
    };
    let parse = |text: &str| -> Option<i64> {
        let (hour, minute) = text.trim().split_once(':')?;
        let hour = hour.parse::<i64>().ok()?;
        let minute = minute.parse::<i64>().ok()?;
        (0..=23).contains(&hour).then_some(hour * 60 + minute)
    };
    let (Some(start), Some(end)) = (parse(start), parse(end)) else {
        return false;
    };
    if start == end {
        return false;
    }
    if start < end {
        (start..end).contains(&minutes_of_day)
    } else {
        minutes_of_day >= start || minutes_of_day < end
    }
}

/// 自动告警的轮询间隔（秒）。
const ALERT_POLL_SEC: u64 = 15;

/// 摘要缓冲上限：告警风暴或推送持续失败时不无限增长，丢最旧的。
const ALERT_DIGEST_BUFFER_MAX: usize = 200;

/// 一条待推送的引擎事件。
#[derive(Clone)]
struct AlertEvent {
    id: i64,
    ts: i64,
    level: String,
    category: String,
    message: String,
}

/// 后台任务：把引擎事件按告警规则推送到 webhook / Telegram。
///
/// 规则（与「发送测试告警」共用同一套设置）：
/// - `alert_enabled` 默认关闭，打开后才推送，避免刚装好就往群里推历史告警；
/// - 只看 level 不低于 `alert_min_level`（默认 warn）的事件；
/// - 同一类别在 `alert_cooldown_sec` 内只推一次；
/// - `alert_quiet_hours`（如 `23:00-07:00`，支持跨天）内静默丢弃；
/// - 推送结果写 `alert_logs`，和手动测试告警同表。
///
/// 启动时不回补历史事件：重启一次就把陈年告警全推一遍没有意义。
pub fn spawn_alert_task(state: SystemState) {
    tokio::spawn(async move {
        let mut cursor = latest_event_id(&state);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(ALERT_POLL_SEC));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            for event in new_events(&state, cursor) {
                cursor = cursor.max(event.id);
                if let Err(error) = dispatch_alert(&state, &event).await {
                    tracing::warn!(%error, category = %event.category, "自动告警推送失败");
                }
            }
            if let Err(error) = flush_digest(&state).await {
                tracing::warn!(%error, "告警摘要推送失败");
            }
        }
    });
}

/// 启动时的游标：只处理这之后的新事件。
fn latest_event_id(state: &SystemState) -> i64 {
    let conn = state.db.lock();
    conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM engine_events",
        [],
        |row| row.get(0),
    )
    .unwrap_or(0)
}

/// 取游标之后的新事件。单次上限 200 条，避免一次把几千行全捞出来。
fn new_events(state: &SystemState, cursor: i64) -> Vec<AlertEvent> {
    let conn = state.db.lock();
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, ts, level, category, message FROM engine_events \
         WHERE id > ?1 ORDER BY id ASC LIMIT 200",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([cursor], |row| {
        Ok(AlertEvent {
            id: row.get(0)?,
            ts: row.get(1)?,
            level: row.get(2)?,
            category: row.get(3)?,
            message: row.get(4)?,
        })
    });
    match rows {
        Ok(rows) => rows.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

/// 判断一条事件要不要推：总开关、级别、通道、静默时段、冷却窗口。
async fn dispatch_alert(state: &SystemState, event: &AlertEvent) -> Result<(), String> {
    let cfg = {
        let conn = state.db.lock();
        alert_cfg(&conn)
    };
    if !cfg.enabled {
        return Ok(());
    }
    if !level_at_least(&event.level, &cfg.min_level) {
        return Ok(());
    }
    let urls = cfg.webhook_urls(&event.level);
    if urls.is_empty() && !cfg.telegram_enabled() {
        return Ok(());
    }
    if cfg.digest_enabled && event.level == "warn" {
        if let Ok(mut buffer) = state.alert_digest.lock() {
            if buffer.len() >= ALERT_DIGEST_BUFFER_MAX {
                buffer.remove(0);
            }
            buffer.push(event.clone());
        }
        return Ok(());
    }
    if !cfg.quiet_hours.is_empty() {
        let minutes_of_day = crate::now().rem_euclid(86_400) / 60;
        if in_quiet_hours(&cfg.quiet_hours, minutes_of_day) {
            return Ok(());
        }
    }
    {
        let mut last = state.alert_last.lock().expect("alert last");
        if !alert_allowed(&mut last, &event.category, crate::now(), cfg.cooldown_sec) {
            return Ok(());
        }
    }
    let text = render_alert(&cfg, event);
    let results = send_alerts(&urls, &cfg, &text).await;
    let ok = !results.is_empty() && results.iter().all(|item| item["ok"] == true);
    let channels = results
        .iter()
        .filter_map(|item| item["channel"].as_str())
        .collect::<Vec<_>>()
        .join(",");
    let error = results
        .iter()
        .filter(|item| item["ok"] != true)
        .filter_map(|item| item["error"].as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let conn = state
        .db
        .conn
        .lock()
        .map_err(|_| "数据库锁不可用".to_string())?;
    conn.execute(
        "INSERT INTO alert_logs(ts, level, category, message, channels, ok, error) \
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        (
            crate::now(),
            &event.level,
            &event.category,
            event.message.chars().take(500).collect::<String>(),
            channels,
            i64::from(ok),
            error.chars().take(500).collect::<String>(),
        ),
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// 摘要是否到期：缓冲最早一条已经等待满 `minutes` 分钟。
fn digest_due(oldest_ts: i64, now: i64, minutes: i64) -> bool {
    minutes > 0 && now.saturating_sub(oldest_ts) >= minutes * 60
}

/// 摘要正文：标题写明合并了几条，后面逐条列出类别与消息。
fn render_digest(events: &[AlertEvent]) -> String {
    let mut text = format!("🟠 [NanoQuant] warn 摘要 {} 条", events.len());
    for event in events {
        text.push_str(&format!("\n- [{}] {}", event.category, event.message));
    }
    text
}

/// 到点把 warn 缓冲合并成一条推送。单次最多 20 条，剩余留到下一轮。
async fn flush_digest(state: &SystemState) -> Result<(), String> {
    let cfg = {
        let conn = state.db.lock();
        alert_cfg(&conn)
    };
    if !cfg.digest_enabled {
        if let Ok(mut buffer) = state.alert_digest.lock() {
            buffer.clear();
        }
        return Ok(());
    }
    if !cfg.quiet_hours.is_empty() {
        let minutes_of_day = crate::now().rem_euclid(86_400) / 60;
        if in_quiet_hours(&cfg.quiet_hours, minutes_of_day) {
            return Ok(());
        }
    }
    let now = crate::now();
    let batch = {
        let mut buffer = state
            .alert_digest
            .lock()
            .map_err(|_| "告警摘要缓冲不可用".to_string())?;
        let Some(oldest) = buffer.first() else {
            return Ok(());
        };
        if !digest_due(oldest.ts, now, cfg.digest_minutes) {
            return Ok(());
        }
        let take = buffer.len().min(20);
        buffer.drain(0..take).collect::<Vec<_>>()
    };
    let urls = cfg.webhook_urls("warn");
    if urls.is_empty() && !cfg.telegram_enabled() {
        return Ok(());
    }
    let text = render_digest(&batch);
    let results = send_alerts(&urls, &cfg, &text).await;
    let ok = !results.is_empty() && results.iter().all(|item| item["ok"] == true);
    let channels = results
        .iter()
        .filter_map(|item| item["channel"].as_str())
        .collect::<Vec<_>>()
        .join(",");
    let error = results
        .iter()
        .filter(|item| item["ok"] != true)
        .filter_map(|item| item["error"].as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let conn = state
        .db
        .conn
        .lock()
        .map_err(|_| "数据库锁不可用".to_string())?;
    conn.execute(
        "INSERT INTO alert_logs(ts, level, category, message, channels, ok, error) \
         VALUES(?1, 'warn', 'digest', ?2, ?3, ?4, ?5)",
        (
            now,
            text.chars().take(500).collect::<String>(),
            channels,
            i64::from(ok),
            error.chars().take(500).collect::<String>(),
        ),
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// 级别排序：info < warn < error。未知级别一律不推。
fn level_at_least(level: &str, min_level: &str) -> bool {
    fn rank(value: &str) -> i32 {
        match value {
            "info" => 0,
            "warn" => 1,
            "error" => 2,
            _ => -1,
        }
    }
    rank(level) >= rank(min_level)
}

/// 告警文案：有模板就用模板，否则用默认格式。
fn render_alert(cfg: &AlertCfg, event: &AlertEvent) -> String {
    if !cfg.template.is_empty() {
        return cfg
            .template
            .replace("{message}", &event.message)
            .replace("{level}", &event.level)
            .replace("{category}", &event.category);
    }
    format!(
        "🟠 [NanoQuant] {} · {}\n{}",
        event.level.to_uppercase(),
        event.category,
        event.message
    )
}

/// 当前交易环境是否配置了交易所 Secret（内核要用它签名）。
fn exec_secret_configured(conn: &rusqlite::Connection, mode: &str) -> bool {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key='data'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .map(|data| {
        let field = if mode == "live" {
            "live_api_secret_enc"
        } else {
            "demo_api_secret_enc"
        };
        data[field]
            .as_str()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    })
    .unwrap_or(false)
}

async fn exec_core(State(state): State<SystemState>) -> Json<Value> {
    let (trading_mode, has_key, pending) = {
        let conn = state.db.lock();
        let trading_mode = current_mode(&conn);
        let has_key = exec_secret_configured(&conn, &trading_mode);
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM orders WHERE status NOT IN ('FILLED','REJECTED','CANCELED','EXPIRED')",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        (trading_mode, has_key, pending)
    };
    let client = state.exec_client.clone();
    let connected = client
        .as_ref()
        .map(|client| client.state() == crate::exec_client::ConnState::Connected)
        .unwrap_or(false);
    let mut algo: Option<String> = None;
    let mut probe_ms: Option<f64> = None;
    let mut segments = Value::Null;
    if connected {
        if let Some(client) = client.as_ref() {
            if let Ok((value, ms)) = client.probe().await {
                algo = Some(value);
                probe_ms = Some(round(ms, 2));
            }
            if let Ok(view) = client.query_segments().await {
                segments = view;
            }
        }
    }
    let stats = state
        .exec
        .lock()
        .map(|mut stats| {
            stats.observe_latency(probe_ms);
            stats.clone()
        })
        .unwrap_or_default();
    let state_text = client
        .as_ref()
        .map(|client| format!("{:?}", client.state()))
        .unwrap_or_else(|| "Disconnected".to_string());
    let last_error = client.as_ref().and_then(|client| client.last_error());
    let target = client
        .as_ref()
        .map(|client| client.endpoint().to_string())
        .unwrap_or_default();
    let dropped = client.as_ref().map(|client| client.dropped()).unwrap_or(0);
    let verdict = if connected {
        format!(
            "执行内核已连接（algo={}），探活与七段指标正常",
            algo.as_deref().unwrap_or("?")
        )
    } else {
        "默认执行内核未连接：主服务启动需完成握手，请检查 EXEC_TCP/EXEC_SOCKET 与内核进程"
            .to_string()
    };
    Json(json!({
        "trading_mode": trading_mode,
        "configured": client.is_some(),
        "deployed": connected,
        "connected": connected,
        "state": state_text,
        "last_error": last_error,
        "target": target,
        "algo": algo.unwrap_or_default(),
        "has_key": has_key,
        "counts": {
            "checks": stats.checks,
            "ok": stats.ok,
            "errors": stats.errors,
            "skipped": stats.skipped,
        },
        "latency_ms": stats.latency_view(),
        "segments": segments,
        "dropped": dropped,
        "order_ledger": {"pending": pending},
        "verdict": verdict,
        "reason": verdict,
    }))
}

/// 把一次探活结果归到自检结论里。
///
/// 执行内核是默认必选组件：未连接就是故障（Error），不再有「没部署也算通过」。
fn exec_check_outcome(connected: bool, has_key: bool) -> ExecCheck {
    if !connected {
        ExecCheck::Error
    } else if !has_key {
        ExecCheck::Skipped
    } else {
        ExecCheck::Ok
    }
}

async fn exec_self_test(State(state): State<SystemState>) -> Json<Value> {
    let has_key = {
        let conn = state.db.lock();
        exec_secret_configured(&conn, &current_mode(&conn))
    };
    let client = state.exec_client.clone();
    let connected = client
        .as_ref()
        .map(|client| client.state() == crate::exec_client::ConnState::Connected)
        .unwrap_or(false);
    let mut algo = String::new();
    let mut latency_ms = None;
    if connected {
        if let Some(client) = client.as_ref() {
            if let Ok((value, ms)) = client.probe().await {
                algo = value;
                latency_ms = Some(round(ms, 2));
            }
        }
    }
    let outcome = exec_check_outcome(connected, has_key);
    if let Ok(mut stats) = state.exec.lock() {
        stats.observe_latency(latency_ms);
        stats.record_check(outcome);
    }
    let detail = match outcome {
        ExecCheck::Ok => {
            format!("执行内核已连接（algo={algo}）；签名逐字节比对用 ops verify-exec-core")
        }
        ExecCheck::Skipped => {
            "执行内核已连接但当前交易环境没有配置交易所 Secret，只能探活、不能比对签名".to_string()
        }
        ExecCheck::Error => client
            .as_ref()
            .and_then(|client| client.last_error())
            .unwrap_or_else(|| {
                "默认执行内核未连接：主服务启动需完成握手，请检查 EXEC_TCP/EXEC_SOCKET 与内核进程"
                    .to_string()
            }),
    };
    Json(json!({
        "ok": outcome == ExecCheck::Ok,
        "outcome": outcome.as_str(),
        "connected": connected,
        "configured": client.is_some(),
        "endpoint": client.as_ref().map(|client| client.endpoint().to_string()).unwrap_or_default(),
        "algo": algo,
        "has_key": has_key,
        "latency_ms": latency_ms,
        "detail": detail,
    }))
}

#[derive(Deserialize)]
struct ExecReconcileQuery {
    #[serde(default = "default_lookback_hours")]
    lookback_hours: i64,
    #[serde(default = "default_reconcile_limit")]
    limit: i64,
}

fn default_lookback_hours() -> i64 {
    24
}

fn default_reconcile_limit() -> i64 {
    500
}

async fn exec_reconcile(
    State(state): State<SystemState>,
    Query(query): Query<ExecReconcileQuery>,
) -> Json<Value> {
    let lookback = query.lookback_hours.clamp(1, 168) * 3600;
    let limit = query.limit.clamp(1, 5000);
    let conn = state.db.lock();
    let checked: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM orders WHERE ts >= ?1",
            [crate::now() - lookback],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let mismatched: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM write_failures WHERE resolved=0 AND kind LIKE 'execution_%'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    Json(json!({
        "ok": mismatched == 0,
        "checked": checked.min(limit),
        "mismatched": mismatched,
        "lookback_hours": query.lookback_hours,
        "detail": if mismatched == 0 {
            "本地订单窗口内无未处理执行死信；执行内核未接线时以本地台账为准"
        } else {
            "存在未处理的执行死信，先重放或忽略后再切流"
        },
    }))
}

async fn exec_reset(State(state): State<SystemState>) -> Json<Value> {
    if let Ok(mut stats) = state.exec.lock() {
        *stats = ExecStats::default();
    }
    let core_reset = match state.exec_client.as_ref() {
        Some(client) if client.state() == crate::exec_client::ConnState::Connected => {
            client.reset_metrics().await.is_ok()
        }
        _ => false,
    };
    if let Ok(conn) = state.db.conn.lock() {
        let _ = conn.execute(
            "INSERT INTO engine_events(ts, level, category, message, payload) \
             VALUES(?1, 'info', 'exec', '清零执行内核探活与分段计数', '{}')",
            [crate::now()],
        );
    }
    Json(json!({
        "ok": true,
        "core_reset": core_reset,
        "counts": {"checks": 0, "ok": 0, "errors": 0, "skipped": 0},
        "detail": "已清零探活计数；内核已连接时同时转发 metrics_reset",
    }))
}

/// 执行内核端点。Linux 生产用 UDS（EXEC_SOCKET），Windows/容器联调用 TCP（EXEC_TCP）；
/// 只认 TCP 会让 UDS 部署永远显示「未配置」。
fn exec_endpoint() -> Option<String> {
    exec_endpoint_from(
        std::env::var("EXEC_TCP").ok(),
        std::env::var("EXEC_SOCKET").ok(),
    )
}

fn exec_endpoint_from(tcp: Option<String>, socket: Option<String>) -> Option<String> {
    let clean = |value: Option<String>| {
        value
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    if let Some(address) = clean(tcp) {
        return Some(format!("tcp:{address}"));
    }
    clean(socket).map(|path| format!("unix:{path}"))
}

fn endpoint_reachable(endpoint: &str) -> bool {
    if let Some(path) = endpoint.strip_prefix("unix:") {
        #[cfg(unix)]
        {
            return std::os::unix::net::UnixStream::connect(path).is_ok();
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            return false;
        }
    }
    tcp_reachable(endpoint.strip_prefix("tcp:").unwrap_or(endpoint))
}

fn tcp_reachable(address: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(mut addresses) = address.to_socket_addrs() else {
        return false;
    };
    addresses.any(|address| {
        std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(300))
            .is_ok()
    })
}

pub(crate) fn mask_secret(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    if plain.len() <= 8 {
        return "*".repeat(plain.len());
    }
    format!(
        "{}{}{}",
        &plain[..4],
        "*".repeat(6),
        &plain[plain.len() - 4..]
    )
}

fn master_key_text(state: &SystemState) -> Result<String, String> {
    let auth = state.auth.as_ref().ok_or("服务未装配鉴权状态")?;
    let master_key = auth
        .security
        .lock()
        .map_err(|_| "鉴权状态锁不可用".to_string())?
        .master_key
        .clone();
    crate::secret::key_text_from_master_bytes(&master_key)
}

fn read_data_setting(conn: &rusqlite::Connection) -> Value {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key='data'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .unwrap_or_else(|| json!({}))
}

fn update_data_setting(
    conn: &rusqlite::Connection,
    patch: &Map<String, Value>,
) -> Result<(), String> {
    let mut data = read_data_setting(conn);
    let object = data
        .as_object_mut()
        .ok_or_else(|| "data 配置不是对象".to_string())?;
    for (key, value) in patch {
        object.insert(key.clone(), value.clone());
    }
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES('data', ?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        (
            serde_json::to_string(&data).map_err(|error| error.to_string())?,
            crate::now(),
        ),
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

fn credential_view(data: &Value, prefix: &str, master_key: &str) -> Value {
    let decrypt = |field: &str| -> Result<String, String> {
        let encrypted = data[format!("{prefix}_api_{field}_enc")]
            .as_str()
            .unwrap_or("");
        crate::secret::decrypt_secret(master_key, encrypted)
    };
    match (decrypt("key"), decrypt("secret")) {
        (Ok(key), Ok(secret)) => json!({
            "has_key": !key.is_empty(),
            "has_secret": !secret.is_empty(),
            "configured": !key.is_empty() && !secret.is_empty(),
            "api_key_masked": mask_secret(&key),
            "api_secret_masked": mask_secret(&secret),
            "error": "",
        }),
        (key, secret) => json!({
            "has_key": false,
            "has_secret": false,
            "configured": false,
            "api_key_masked": "",
            "api_secret_masked": "",
            "error": key.err().or(secret.err()).unwrap_or_default(),
        }),
    }
}

async fn get_exchange_credentials(State(state): State<SystemState>) -> axum::response::Response {
    let master_key = match master_key_text(&state) {
        Ok(key) => key,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": error})),
            )
                .into_response()
        }
    };
    let conn = state.db.lock();
    let data = read_data_setting(&conn);
    let mode = current_mode(&conn);
    Json(json!({
        "demo": credential_view(&data, "demo", &master_key),
        "live": credential_view(&data, "live", &master_key),
        "env": mode,
        "proxy": data["proxy"].as_str().unwrap_or(""),
        "demo_base_url": data["demo_base_url"].as_str().unwrap_or(""),
        "live_base_url": data["live_base_url"].as_str().unwrap_or(""),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CredentialsIn {
    #[serde(default)]
    env: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_secret: Option<String>,
    #[serde(default)]
    proxy: Option<String>,
}

async fn save_exchange_credentials(
    State(state): State<SystemState>,
    Json(payload): Json<CredentialsIn>,
) -> axum::response::Response {
    let master_key = match master_key_text(&state) {
        Ok(key) => key,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"detail": error})),
            )
                .into_response()
        }
    };
    let env = if payload.env == "demo" {
        "demo"
    } else {
        "live"
    };
    let mut patch = Map::new();
    if let Some(api_key) = payload.api_key.filter(|value| !value.trim().is_empty()) {
        let api_key = api_key.trim();
        if let Err(error) = crate::sign::validate_api_key(api_key) {
            return (StatusCode::BAD_REQUEST, Json(json!({"detail": error}))).into_response();
        }
        match crate::secret::encrypt_secret(&master_key, api_key) {
            Ok(value) => {
                patch.insert(format!("{env}_api_key_enc"), json!(value));
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail": error})),
                )
                    .into_response()
            }
        }
    }
    if let Some(api_secret) = payload.api_secret.filter(|value| !value.trim().is_empty()) {
        let api_secret = api_secret.trim();
        if let Err(error) = crate::sign::validate_api_secret(api_secret) {
            return (StatusCode::BAD_REQUEST, Json(json!({"detail": error}))).into_response();
        }
        match crate::secret::encrypt_secret(&master_key, api_secret) {
            Ok(value) => {
                patch.insert(format!("{env}_api_secret_enc"), json!(value));
            }
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail": error})),
                )
                    .into_response()
            }
        }
    }
    if let Some(proxy) = payload.proxy {
        patch.insert("proxy".to_string(), json!(proxy.trim()));
    }
    if patch.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"detail": "没有需要保存的内容"})),
        )
            .into_response();
    }
    let updated = patch.keys().cloned().collect::<Vec<_>>();
    let conn = state.db.lock();
    if let Err(error) = update_data_setting(&conn, &patch) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": error})),
        )
            .into_response();
    }
    audit_settings(&conn, "data", &patch);
    Json(json!({"ok": true, "env": env, "updated": updated})).into_response()
}

#[derive(Deserialize)]
struct CredentialsEnvQuery {
    #[serde(default = "default_live_env")]
    env: String,
}

fn default_live_env() -> String {
    "live".to_string()
}

async fn clear_exchange_credentials(
    State(state): State<SystemState>,
    Query(query): Query<CredentialsEnvQuery>,
) -> Json<Value> {
    let env = if query.env == "demo" { "demo" } else { "live" };
    let mut patch = Map::new();
    patch.insert(format!("{env}_api_key_enc"), json!(""));
    patch.insert(format!("{env}_api_secret_enc"), json!(""));
    if let Ok(conn) = state.db.conn.lock() {
        let _ = update_data_setting(&conn, &patch);
        audit_settings(&conn, "data", &patch);
        let _ = conn.execute(
            "INSERT INTO engine_events(ts, level, category, message, payload) \
             VALUES(?1, 'warn', 'settings', ?2, ?3)",
            (
                crate::now(),
                format!("清空{env}环境的交易所 API 密钥"),
                json!({"env": env}).to_string(),
            ),
        );
    }
    json!({"ok": true, "env": env}).into()
}

fn audit_settings(conn: &rusqlite::Connection, group: &str, patch: &Map<String, Value>) {
    let changes = patch
        .iter()
        .map(|(key, value)| {
            let shown = if key.contains("key") || key.contains("secret") {
                json!("***")
            } else {
                value.clone()
            };
            (key.clone(), json!({"from": null, "to": shown}))
        })
        .collect::<Map<_, _>>();
    let _ = conn.execute(
        "INSERT INTO settings_audit(ts, user, group_name, changes, ip) VALUES(?1, '', ?2, ?3, '')",
        (
            crate::now(),
            group,
            serde_json::to_string(&changes).unwrap_or_else(|_| "{}".to_string()),
        ),
    );
}

async fn runtime_probe_endpoint(
    State(state): State<SystemState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(auth) = state.auth.as_ref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !auth::bearer_is_valid(auth, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(state.runtime_probe.summary(crate::now())).into_response()
}

/// Prometheus 文本。账户类指标只在带有效令牌时出现：这条端点免登录，
/// 把权益放进去等于对全网广播资金规模。
async fn metrics(State(state): State<SystemState>, headers: HeaderMap) -> impl IntoResponse {
    let authorized = state
        .auth
        .as_ref()
        .is_some_and(|auth| auth::bearer_is_valid(auth, &headers));
    let Ok(conn) = state.db.read() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "数据库不可用").into_response();
    };
    if !metrics_enabled(&conn) {
        return (StatusCode::NOT_FOUND, "metrics 已关闭").into_response();
    }
    let now = crate::now();
    let signals_1h: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signals WHERE ts >= ?1",
            [now - 3600],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let last_cycle = engine_int(&conn, "last_cycle_ts");
    let age = if last_cycle > 0 {
        (now - last_cycle) as f64
    } else {
        -1.0
    };
    let mut lines = vec![
        gauge("nanoquant_up", "服务是否在线", 1.0),
        gauge(
            "nanoquant_signals_last_1h",
            "最近 1 小时信号数",
            signals_1h as f64,
        ),
        gauge(
            "nanoquant_halted",
            "是否处于熔断状态",
            if engine_value(&conn, "halted") == "1" {
                1.0
            } else {
                0.0
            },
        ),
        gauge(
            "nanoquant_last_cycle_age_seconds",
            "距上一轮扫描的秒数",
            age,
        ),
    ];
    if authorized {
        let mode = current_mode(&conn);
        let equity: f64 = conn
            .query_row(
                "SELECT wallet FROM equity_points ORDER BY ts DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0.0);
        let open: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM positions WHERE status = 'open' AND mode = ?1",
                [&mode],
                |r| r.get(0),
            )
            .unwrap_or(0);
        lines.push(gauge("nanoquant_account_equity_usdt", "账户权益", equity));
        lines.push(gauge(
            "nanoquant_account_open_positions",
            "当前持仓数",
            open as f64,
        ));
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        lines.join("\n") + "\n",
    )
        .into_response()
}

fn gauge(name: &str, help: &str, value: f64) -> String {
    format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}")
}

fn metrics_enabled(conn: &rusqlite::Connection) -> bool {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key='system'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .and_then(|value| value.get("metrics_enabled").and_then(Value::as_bool))
    .unwrap_or(true)
}

fn engine_int(conn: &rusqlite::Connection, key: &str) -> i64 {
    conn.query_row(
        "SELECT value FROM engine_state WHERE key=?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| raw.parse().ok())
    .unwrap_or(0)
}

fn engine_value(conn: &rusqlite::Connection, key: &str) -> String {
    conn.query_row(
        "SELECT value FROM engine_state WHERE key=?1",
        [key],
        |row| row.get(0),
    )
    .unwrap_or_default()
}

fn current_mode(conn: &rusqlite::Connection) -> String {
    let mode = engine_value(conn, "mode");
    if mode.is_empty() {
        "demo".to_string()
    } else {
        mode
    }
}

async fn list_events(
    State(state): State<SystemState>,
    Query(query): Query<EventsQuery>,
) -> impl IntoResponse {
    let conn = state.db.lock();
    let since = query.since.unwrap_or(0);
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let level = query.level;
    let sql = if level.is_some() {
        "SELECT id, ts, level, category, message FROM engine_events WHERE id > ?1 AND level = ?2 ORDER BY id DESC LIMIT ?3"
    } else {
        "SELECT id, ts, level, category, message FROM engine_events WHERE id > ?1 ORDER BY id DESC LIMIT ?2"
    };
    let mut stmt = conn.prepare(sql).expect("prepare events");
    let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
        Ok(
            json!({"id": r.get::<_, i64>(0)?, "ts": r.get::<_, i64>(1)?, "level": r.get::<_, String>(2)?, "category": r.get::<_, String>(3)?, "message": r.get::<_, String>(4)?}),
        )
    };
    let items: Vec<Value> = match &level {
        None => stmt
            .query_map([since, limit], map)
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows"),
        Some(lv) => stmt
            .query_map(rusqlite::params![since, lv, limit], map)
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows"),
    };
    Json(json!({"items": items}))
}

async fn logs(
    State(state): State<SystemState>,
    Query(query): Query<LogsQuery>,
) -> impl IntoResponse {
    let tail = query.lines.unwrap_or(200).clamp(1, 2000);
    let path = match query.path {
        Some(raw) => {
            // 只允许读日志目录里的文件。没有这道闸，这个接口就是任意文件读取：
            // 拿到管理员口令的人能读 .master_key 和交易所密钥。
            let requested = std::path::PathBuf::from(raw);
            let base = state
                .logs_dir
                .canonicalize()
                .unwrap_or_else(|_| state.logs_dir.clone());
            let resolved = requested.canonicalize().unwrap_or(requested);
            if !resolved.starts_with(&base) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"detail": "只允许读取日志目录内的文件"})),
                )
                    .into_response();
            }
            resolved
        }
        None => state.logs_path.clone(),
    };
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let keyword = query.keyword.map(|value| value.to_lowercase());
    let lines: Vec<&str> = content
        .lines()
        .rev()
        .filter(|line| {
            keyword
                .as_ref()
                .is_none_or(|key| line.to_lowercase().contains(key))
        })
        .take(tail)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Json(json!({"lines": lines, "count": lines.len()})).into_response()
}

async fn info(State(state): State<SystemState>) -> impl IntoResponse {
    let Ok(conn) = state.db.read() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": "数据库不可用"})),
        )
            .into_response();
    };
    let engine = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='engine'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or_else(|| json!({}));
    let mut engine = engine;
    if let Some(object) = engine.as_object_mut() {
        for key in [
            "enabled",
            "halted",
            "status",
            "mode",
            "last_cycle_ts",
            "last_cycle_at",
            "last_error",
            "risk_halted",
            "risk_halt_reason",
            "halted_at",
        ] {
            let value = engine_value(&conn, key);
            if !value.is_empty() {
                object.insert(key.to_string(), json!(value));
            }
        }
    }
    let settings_groups = conn
        .prepare("SELECT key FROM app_settings ORDER BY key")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(Result::ok).collect::<Vec<_>>())
        })
        .unwrap_or_default();
    Json(
        json!({"version": env!("CARGO_PKG_VERSION"), "uptime_sec": crate::now().saturating_sub(state.started_at).max(0), "kernel": "rust", "engine": engine, "settings_groups": settings_groups}),
    )
    .into_response()
}

fn diag_check(name: &str, ok: bool, detail: String, started: std::time::Instant) -> Value {
    json!({
        "name": name,
        "ok": ok,
        "detail": detail,
        "latency_ms": round(started.elapsed().as_secs_f64() * 1000.0, 2),
        "timeout": false,
    })
}

async fn diagnostics(State(state): State<SystemState>) -> impl IntoResponse {
    let Ok(conn) = state.db.read() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": "数据库不可用"})),
        )
            .into_response();
    };
    let mut checks = Vec::new();

    let started = std::time::Instant::now();
    let database_ok = conn
        .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
        .is_ok();
    checks.push(diag_check(
        "数据库",
        database_ok,
        if database_ok {
            "连接正常".to_string()
        } else {
            "连接失败".to_string()
        },
        started,
    ));

    let started = std::time::Instant::now();
    let total = engine_int(&conn, "write_total");
    let slow = engine_int(&conn, "write_slow_count");
    let max_ms = engine_int(&conn, "write_max_ms");
    let queue_ok = max_ms <= 200;
    checks.push(diag_check(
        "写库队列",
        queue_ok,
        format!("累计 {total} 次, 慢写 {slow} 次, 最大 {max_ms}ms"),
        started,
    ));

    let started = std::time::Instant::now();
    let dead = dead_letter_summary(&conn);
    let pending = dead["pending"].as_i64().unwrap_or(0);
    checks.push(diag_check(
        "落库补偿",
        pending == 0,
        format!(
            "未处理死信 {pending} 条 (卡死 {} 条)",
            dead["stuck"].as_i64().unwrap_or(0)
        ),
        started,
    ));

    let started = std::time::Instant::now();
    let mode = {
        let value = engine_value(&conn, "mode");
        if value.is_empty() {
            "demo".to_string()
        } else {
            value
        }
    };
    let data = read_data_setting(&conn);
    let master_text = state
        .auth
        .as_ref()
        .and_then(|auth| {
            auth.security
                .lock()
                .ok()
                .map(|security| security.master_key.clone())
        })
        .and_then(|key| crate::secret::key_text_from_master_bytes(&key).ok());
    let credential = master_text.map(|master_key| credential_view(&data, &mode, &master_key));
    let credentials_ok = credential.as_ref().is_some_and(|view| {
        view["error"].as_str().unwrap_or("").is_empty() && view["configured"] == true
    });
    checks.push(match credential {
        Some(view) if view["error"].as_str().unwrap_or("").is_empty() => diag_check(
            "交易凭证",
            true,
            if view["configured"] == true {
                format!("{mode} 环境凭证可解密")
            } else {
                format!("{mode} 环境未配置凭证")
            },
            started,
        ),
        _ => diag_check(
            "交易凭证",
            false,
            "凭证无法解密：主密钥与密文不匹配".to_string(),
            started,
        ),
    });

    let started = std::time::Instant::now();
    let endpoint = exec_endpoint();
    let connected = endpoint.as_deref().is_some_and(endpoint_reachable);
    // 执行内核是默认必选组件：未连接时交易引擎不会启动。
    let channel_ok = connected;
    checks.push(diag_check(
        "交易通道",
        channel_ok,
        if connected {
            "执行内核端点可达".to_string()
        } else if credentials_ok {
            "执行内核未连接（凭证可解密，但交易引擎需内核握手）".to_string()
        } else {
            "执行内核未连接，且当前环境凭证不可用".to_string()
        },
        started,
    ));

    let started = std::time::Instant::now();
    let enabled = engine_value(&conn, "enabled") == "true";
    let last_cycle = engine_int(&conn, "last_cycle_ts");
    // 阈值跟着扫描周期走：写死 180 秒会把 10 分钟周期的间歇期误报成心跳超时。
    let cycle_interval_sec = crate::runtime::cycle_interval_sec(&conn);
    let liveness_threshold_sec = crate::runtime::liveness_threshold_sec(cycle_interval_sec);
    let heartbeat_age = crate::now().saturating_sub(last_cycle);
    let engine_ok = !enabled || (last_cycle > 0 && heartbeat_age < liveness_threshold_sec);
    let runtime_task = engine_value(&conn, "runtime_task");
    let runtime_task = if runtime_task.is_empty() {
        "未知".to_string()
    } else {
        runtime_task
    };
    checks.push(diag_check(
        "引擎心跳",
        engine_ok,
        if !enabled {
            "引擎已停止".to_string()
        } else if engine_ok {
            format!(
                "最近心跳 {heartbeat_age} 秒前（扫描周期 {cycle_interval_sec} 秒，判活阈值 {liveness_threshold_sec} 秒）"
            )
        } else {
            format!(
                "引擎心跳超时：上次扫描 {heartbeat_age} 秒前，超过判活阈值 {liveness_threshold_sec} 秒（扫描周期 {cycle_interval_sec} 秒，后台任务状态 {runtime_task}）"
            )
        },
        started,
    ));

    let started = std::time::Instant::now();
    let ai_enabled = conn
        .query_row("SELECT value FROM app_settings WHERE key='ai'", [], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.get("enabled").and_then(Value::as_bool))
        .unwrap_or(false);
    let providers: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ai_providers WHERE enabled=1",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    checks.push(diag_check(
        "AI 模型",
        !ai_enabled || providers > 0,
        if !ai_enabled {
            "AI 未启用".to_string()
        } else if providers == 0 {
            "未配置 AI 供应商：AI 复核会 fail-closed 跳过开仓（去「AI 接入」页配置）".to_string()
        } else {
            format!("启用供应商 {providers} 个")
        },
        started,
    ));

    let started = std::time::Instant::now();
    let storage_ok = std::fs::create_dir_all(&state.archives_dir).is_ok();
    let free = (state.free_bytes)(&state.archives_dir);
    checks.push(diag_check(
        "存储",
        storage_ok,
        format!(
            "归档目录 {}，剩余 {}",
            if storage_ok { "可写" } else { "不可写" },
            human_bytes(free)
        ),
        started,
    ));

    let started = std::time::Instant::now();
    // 执行内核是默认必选组件：未连接即异常。
    let core_client = state.exec_client.as_ref();
    let core_connected = core_client
        .map(|client| client.state() == crate::exec_client::ConnState::Connected)
        .unwrap_or(false);
    checks.push(diag_check(
        "执行内核",
        core_connected,
        if let Some(client) = core_client {
            if core_connected {
                format!("已连接（端点 {}）", client.endpoint())
            } else {
                client.last_error().unwrap_or_else(|| {
                    "内核未连接：检查 EXEC_TCP/EXEC_SOCKET 与内核进程".to_string()
                })
            }
        } else {
            "未配置执行内核端点（EXEC_TCP/EXEC_SOCKET）：交易引擎不会启动".to_string()
        },
        started,
    ));

    let started = std::time::Instant::now();
    let event_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM engine_events", [], |row| row.get(0))
        .unwrap_or(0);
    checks.push(diag_check(
        "事件日志",
        event_count >= 0,
        format!("当前 {event_count} 条"),
        started,
    ));

    let ok = checks.iter().all(|check| check["ok"] == true);
    Json(json!({
        "ok": ok,
        "checks": checks,
        "free_bytes": (state.free_bytes)(&state.archives_dir)
    }))
    .into_response()
}

fn round(value: f64, digits: u32) -> f64 {
    let scale = 10_f64.powi(digits as i32);
    (value * scale).round() / scale
}

fn disk_free_bytes(path: &std::path::Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return 0;
        };
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
            return 0;
        }
        (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut free = 0_u64;
        let ok = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            free
        } else {
            0
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        0
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use rusqlite::Connection;
    use tower::ServiceExt;

    fn test_auth() -> AuthState {
        use std::sync::Mutex;
        AuthState {
            security: std::sync::Arc::new(Mutex::new(auth::Security {
                username: "admin".into(),
                password_hash: String::new(),
                password_is_default: false,
                token_version: 1,
                session_ttl_hours: 12,
                master_key: b"master-key".to_vec(),
                salt: "salt".into(),
            })),
            throttle: std::sync::Arc::new(Mutex::new(auth::LoginThrottle::new(3, 60))),
            db: None,
        }
    }

    fn fixture(path: &std::path::Path) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_events(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, category TEXT, message TEXT); \
             INSERT INTO engine_events VALUES(1,100,'info','engine','启动'); \
             INSERT INTO engine_events VALUES(2,200,'warn','risk','回撤超限');",
        ).unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn diagnostics_flags_undecryptable_credentials() {
        let path = std::env::temp_dir().join("nq-diagnostics-creds.db");
        let db = fixture(&path);
        db.lock()
            .execute(
                "INSERT INTO app_settings VALUES('data','{\"demo_api_key_enc\":\"invalid-token\"}',0)",
                [],
            )
            .unwrap();
        let state = SystemState::new(db, crate::now()).with_auth(test_auth());
        let app = routes().with_state(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/system/diagnostics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let check = body["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "交易凭证")
            .unwrap();
        assert_eq!(check["ok"], false);
        assert!(check["detail"].as_str().unwrap().contains("无法解密"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn runtime_probe_summary_matches_legacy_fields_and_marks_gc_unsupported() {
        let probe = RuntimeProbe::new(100);
        probe.record(1.234);
        probe.record(21.0);
        probe.record(51.0);
        let summary = probe.summary(175);
        assert_eq!(summary["uptime_sec"], 75);
        assert_eq!(summary["samples"], 3);
        assert_eq!(summary["loop_lag_ms"]["p50"], 21.0);
        assert_eq!(summary["loop_lag_ms"]["p95"], 51.0);
        assert_eq!(summary["loop_lag_ms"]["p99"], 51.0);
        assert_eq!(summary["loop_lag_ms"]["max"], 51.0);
        assert_eq!(summary["gc"]["cycles"], 0);
        assert_eq!(summary["gc"]["supported"], false);
        assert_eq!(summary["blocked_over_20ms"], 2);
        assert_eq!(summary["blocked_over_50ms"], 1);
        assert!(summary["timer_note"].is_string());
    }

    #[test]
    fn runtime_probe_caps_samples_and_clamps_negative_drift() {
        let probe = RuntimeProbe::new(0);
        probe.record(-2.0);
        for _ in 0..4001 {
            probe.record(1.0);
        }
        let summary = probe.summary(100);
        assert_eq!(summary["samples"], 4000);
        assert_eq!(summary["loop_lag_ms"]["p50"], 1.0);
        assert_eq!(summary["uptime_sec"], 0);
    }

    #[tokio::test]
    async fn runtime_route_requires_a_valid_bearer_token() {
        let path = std::env::temp_dir().join("nq-runtime-auth.db");
        let auth = test_auth();
        let state = SystemState::new(fixture(&path), crate::now()).with_auth(auth);
        let app = Router::new().nest("/api/v1", routes()).with_state(state);
        let response = app
            .oneshot(
                Request::get("/api/v1/system/runtime")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn runtime_route_returns_the_legacy_compatible_summary() {
        let path = std::env::temp_dir().join("nq-runtime-response.db");
        let auth = test_auth();
        let started = crate::now() - 10;
        let state = SystemState::new(fixture(&path), started).with_auth(auth);
        let probe = state.runtime_probe.clone();
        state.start_runtime_probe();
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        probe.record(12.5);
        let token = auth::issue_token("admin", 12, 1, b"master-key", "salt", crate::now()).0;
        let app = Router::new().nest("/api/v1", routes()).with_state(state);
        let response = app
            .oneshot(
                Request::get("/api/v1/system/runtime")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        for key in [
            "uptime_sec",
            "samples",
            "loop_lag_ms",
            "gc",
            "timer_note",
            "blocked_over_20ms",
            "blocked_over_50ms",
        ] {
            assert!(body.get(key).is_some(), "missing field {key}");
        }
        assert!(body["samples"].as_u64().unwrap() >= 1);
        assert!(body["loop_lag_ms"]["max"].as_f64().unwrap() >= 12.5);
        assert_eq!(body["gc"]["supported"], false);
        assert!(body["uptime_sec"].as_i64().unwrap() >= 10);
    }

    fn dead_fixture(path: &std::path::Path) -> Db {
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE engine_events(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, category TEXT, message TEXT, payload TEXT); \
             CREATE TABLE write_failures(id INTEGER PRIMARY KEY, ts INTEGER, kind TEXT, payload TEXT, error TEXT, \
             attempts INTEGER, auto_attempts INTEGER, resolved INTEGER); \
             CREATE TABLE orders(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, side TEXT, order_type TEXT, \
             qty REAL, price REAL, status TEXT, purpose TEXT, client_order_id TEXT, exchange_order_id TEXT, \
             reduce_only INTEGER, mode TEXT); \
             CREATE TABLE positions(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, qty REAL, entry_price REAL, \
             mark_price REAL, leverage REAL, margin REAL, sl_price REAL, tp_price REAL, peak_price REAL, \
             initial_sl_price REAL, signal_score REAL, opened_at INTEGER, closed_at INTEGER, exit_price REAL, \
             exit_reason TEXT, realized_pnl REAL, fees REAL, status TEXT, mode TEXT, venue_unrealized REAL, \
             venue_ts INTEGER, partial_taken_at INTEGER); \
             CREATE TABLE trades(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, qty REAL, entry_price REAL, \
             exit_price REAL, pnl REAL, opened_at INTEGER, closed_at INTEGER, hold_seconds INTEGER, \
             exit_reason TEXT, mode TEXT); \
             CREATE TABLE alert_logs(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, category TEXT, \
             message TEXT, channels TEXT, ok INTEGER, error TEXT); \
             CREATE TABLE settings_audit(id INTEGER PRIMARY KEY, ts INTEGER, user TEXT, group_name TEXT, \
             changes TEXT, ip TEXT); \
             CREATE TABLE ai_providers(id INTEGER PRIMARY KEY, name TEXT, protocol TEXT, base_url TEXT, \
             api_key_enc TEXT, model TEXT, temperature REAL, max_tokens INTEGER, reasoning_effort TEXT, \
             timeout_sec INTEGER, json_mode INTEGER, enabled INTEGER, priority INTEGER, role TEXT, \
             extra TEXT, created_at INTEGER, updated_at INTEGER); \
             CREATE TABLE equity_points(id INTEGER PRIMARY KEY, ts INTEGER, mode TEXT, wallet REAL, \
             available REAL, unrealized REAL, margin_used REAL, positions INTEGER); \
             CREATE TABLE signals(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, direction TEXT, \
             score REAL, ok INTEGER); \
             INSERT INTO orders VALUES(1,41,'BTCUSDT','SELL','MARKET',0.1,100,'FILLED','tp','cid-1','',1,'demo');",
        )
        .unwrap();
        let payload = json!({
            "mode": "demo",
            "fee_rate": 0.0005,
            "now": 42,
            "fill": {
                "client_order_id": "cid-1",
                "exchange_order_id": "",
                "symbol": "BTCUSDT",
                "side": "SELL",
                "qty": 0.1,
                "price": 100.0,
                "reduce_only": true,
                "reason": "tp",
            },
            "args": [{"client_order_id": "cid-1"}],
            "api_secret": "s3cret-value",
        });
        conn.execute(
            "INSERT INTO write_failures VALUES(1,30,'execution_demo_fill',?1,'write failed',0,0,0)",
            [serde_json::to_string(&payload).unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO write_failures VALUES(2,31,'unknown_kind','{}','bad kind',0,0,0)",
            [],
        )
        .unwrap();
        drop(conn);
        Db::open(path.to_str().unwrap()).unwrap()
    }

    async fn request_json(app: Router, method: &str, uri: &str) -> (StatusCode, Value) {
        let response = app
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
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn dead_letters_are_sanitized_and_replay_is_idempotent() {
        let path = std::env::temp_dir().join("nq-dead-letters.db");
        let db = dead_fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(SystemState::new(db.clone(), crate::now()));
        let (status, body) = request_json(
            app.clone(),
            "GET",
            "/api/v1/system/dead-letters?include_resolved=true",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        let payload = &items.iter().find(|item| item["id"] == 1).expect("死信 #1")["payload"];
        assert_eq!(payload["api_secret"], "***", "载荷必须脱敏: {payload}");
        assert!(
            !body.to_string().contains("s3cret-value"),
            "明文密钥不能回显"
        );
        assert_eq!(body["summary"]["pending"], 2);
        assert_eq!(body["summary"]["by_kind"][0]["count"], 1);

        let (_, first) =
            request_json(app.clone(), "POST", "/api/v1/system/dead-letters/1/replay").await;
        assert_eq!(first["status"], "skipped", "{first}");
        let (_, second) =
            request_json(app.clone(), "POST", "/api/v1/system/dead-letters/1/replay").await;
        assert_eq!(
            second["status"], "resolved",
            "第二次必须返回已处理: {second}"
        );
        let (_, missing) = request_json(
            app.clone(),
            "POST",
            "/api/v1/system/dead-letters/999/replay",
        )
        .await;
        assert_eq!(missing["status"], "missing");

        let conn = db.lock();
        let orders: i64 = conn
            .query_row("SELECT COUNT(*) FROM orders", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orders, 1, "幂等重放不得产生第二条业务记录");
        let resolved: i64 = conn
            .query_row("SELECT resolved FROM write_failures WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(resolved, 1);
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE category='db'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(events >= 1);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn replay_all_reports_each_status_and_ignore_marks_resolved() {
        let path = std::env::temp_dir().join("nq-dead-all.db");
        let db = dead_fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(SystemState::new(db.clone(), crate::now()));
        let (_, stats) = request_json(
            app.clone(),
            "POST",
            "/api/v1/system/dead-letters/replay-all",
        )
        .await;
        assert_eq!(stats["total"], 2);
        assert_eq!(stats["skipped"], 1, "{stats}");
        assert_eq!(stats["unsupported"], 1, "{stats}");
        let (_, ignored) =
            request_json(app.clone(), "POST", "/api/v1/system/dead-letters/2/ignore").await;
        assert_eq!(ignored["status"], "ignored");
        let (_, again) =
            request_json(app.clone(), "POST", "/api/v1/system/dead-letters/2/ignore").await;
        assert_eq!(again["status"], "ignored", "重复忽略仍标记为已处理");
        let conn = db.lock();
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM write_failures WHERE resolved=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0);
        let _ = std::fs::remove_file(&path);
    }

    fn test_auth_valid_key() -> AuthState {
        let auth = test_auth();
        auth.security.lock().unwrap().master_key =
            b"MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=".to_vec();
        auth
    }

    async fn request_json_body(
        app: Router,
        method: &str,
        uri: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
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

    async fn request_text(app: Router, uri: &str, token: Option<&str>) -> (StatusCode, String) {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn write_health_shows_backlog_and_dead_letters() {
        let path = std::env::temp_dir().join("nq-write-health.db");
        let db = dead_fixture(&path);
        {
            let conn = db.lock();
            conn.execute_batch(
                "INSERT INTO engine_state VALUES('write_total','20',0); \
                 INSERT INTO engine_state VALUES('write_slow_count','3',0); \
                 INSERT INTO engine_state VALUES('write_max_ms','50',0);",
            )
            .unwrap();
        }
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(SystemState::new(db, crate::now()));
        let (status, body) = request_json(app, "GET", "/api/v1/system/write-health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["queue"]["total"], 20);
        assert_eq!(body["queue"]["slow"], 3);
        assert_eq!(
            body["dead_letters"]["pending"], 2,
            "注入死信后积压可见: {body}"
        );
        assert!(body["verdict"].as_str().unwrap().contains("死信"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn archive_gzip_roundtrip_list_preview_and_traversal_guard() {
        let path = std::env::temp_dir().join("nq-archive.db");
        let db = dead_fixture(&path);
        let dir = std::env::temp_dir().join(format!("nq-archive-{}", crate::now_millis()));
        {
            let conn = db.lock();
            conn.execute_batch(
                "INSERT INTO engine_events VALUES(1,100,'info','engine','旧一','{}'); \
                 INSERT INTO engine_events VALUES(2,200,'warn','risk','旧二','{}');",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO engine_events VALUES(3,?1,'info','engine','新','{}')",
                [crate::now()],
            )
            .unwrap();
        }
        let (count, name) = archive_table(
            &db,
            "engine_events",
            "engine_events",
            crate::now() - 10,
            &dir,
        )
        .unwrap();
        assert_eq!(count, 2, "只归档过期行");
        assert!(name.starts_with("engine_events-") && name.ends_with(".jsonl.gz"));
        assert!(dir.join(&name).exists());
        let left: i64 = db
            .lock()
            .query_row("SELECT COUNT(*) FROM engine_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 1, "归档后原表减少");

        let state = SystemState::new(db, crate::now()).with_archives_dir(dir.clone());
        let app = Router::new().nest("/api/v1", routes()).with_state(state);
        let (status, list) = request_json(app.clone(), "GET", "/api/v1/system/archives").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["items"].as_array().unwrap().len(), 1);
        assert_eq!(list["items"][0]["name"], name);

        let uri = format!("/api/v1/system/archives/{}?limit=1", name);
        let (status, preview) = request_json(app.clone(), "GET", &uri).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(preview["count"], 1);

        let (status, body) = request_json(
            app.clone(),
            "GET",
            "/api/v1/system/archives/%2E%2E%2Fsecret.jsonl.gz",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["detail"].as_str().unwrap().contains("不合法"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn archive_deletes_only_the_rows_it_wrote() {
        let path = std::env::temp_dir().join("nq-archive-cap.db");
        let db = dead_fixture(&path);
        let dir = std::env::temp_dir().join(format!("nq-archive-cap-{}", crate::now_millis()));
        {
            let conn = db.lock();
            let tx = conn.unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO engine_events(ts,level,category,message,payload) \
                         VALUES(?1,'info','engine','old','{}')",
                    )
                    .unwrap();
                for ts in 1..=5002i64 {
                    stmt.execute([ts]).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let (count, _) = archive_table(&db, "engine_events", "engine_events", 6000, &dir).unwrap();
        assert_eq!(count, 5000, "单次最多归档 5000 行");
        let (left, oldest): (i64, i64) = db
            .lock()
            .query_row("SELECT COUNT(*), MIN(ts) FROM engine_events", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(left, 2, "只删除实际写入归档的 5000 行");
        assert_eq!(oldest, 5001, "未归档的旧行必须保留");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn quiet_hours_and_cooldown_are_enforced() {
        assert!(in_quiet_hours("23:00-07:00", 23 * 60 + 30));
        assert!(in_quiet_hours("23:00-07:00", 2 * 60));
        assert!(!in_quiet_hours("23:00-07:00", 12 * 60));
        assert!(!in_quiet_hours("", 12 * 60));
        let mut last = std::collections::HashMap::new();
        assert!(alert_allowed(&mut last, "risk", 100, 120));
        assert!(
            !alert_allowed(&mut last, "risk", 101, 120),
            "冷却期内重复告警被拦截"
        );
        assert!(alert_allowed(&mut last, "risk", 221, 120));
    }

    /// 起一个只回 200 的本地 webhook，用来验证自动告警真的发出去了。
    async fn fake_webhook() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 2048];
                let _ = socket.read(&mut buf).await;
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = "{}";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (port, hits)
    }

    fn alert_event() -> AlertEvent {
        AlertEvent {
            id: 1,
            ts: crate::now(),
            level: "warn".to_string(),
            category: "risk".to_string(),
            message: "回撤超限".to_string(),
        }
    }

    #[tokio::test]
    async fn an_enabled_alert_is_pushed_to_the_webhook_and_written_to_the_log() {
        let (port, hits) = fake_webhook().await;
        let path = std::env::temp_dir().join("nq-auto-alert.db");
        let db = dead_fixture(&path);
        db.lock()
            .execute(
                "INSERT OR REPLACE INTO app_settings VALUES('system',?1,0)",
                [json!({
                    "alert_enabled": true,
                    "alert_min_level": "warn",
                    "alert_webhook_url": format!("http://127.0.0.1:{port}/hook"),
                    "alert_cooldown_sec": 120
                })
                .to_string()],
            )
            .unwrap();
        let state = SystemState::new(db.clone(), crate::now());
        dispatch_alert(&state, &alert_event())
            .await
            .expect("推送应成功");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "启用后应真的发出一次 webhook"
        );
        let logged: i64 = db
            .lock()
            .query_row("SELECT COUNT(*) FROM alert_logs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(logged, 1, "推送结果应写入 alert_logs");

        // 冷却期内同类别不再推
        dispatch_alert(&state, &alert_event())
            .await
            .expect("冷却期静默跳过");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "冷却期内不重复推送"
        );
    }

    #[tokio::test]
    async fn a_disabled_alert_never_leaves_the_process() {
        let (port, hits) = fake_webhook().await;
        let path = std::env::temp_dir().join("nq-auto-alert-off.db");
        let db = dead_fixture(&path);
        db.lock()
            .execute(
                "INSERT OR REPLACE INTO app_settings VALUES('system',?1,0)",
                [json!({
                    "alert_enabled": false,
                    "alert_webhook_url": format!("http://127.0.0.1:{port}/hook")
                })
                .to_string()],
            )
            .unwrap();
        let state = SystemState::new(db.clone(), crate::now());
        dispatch_alert(&state, &alert_event())
            .await
            .expect("关闭时只是跳过，不该报错");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "总开关关闭时不应发出任何请求"
        );
        let logged: i64 = db
            .lock()
            .query_row("SELECT COUNT(*) FROM alert_logs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(logged, 0, "关闭时不写告警记录");
    }

    #[test]
    fn digest_flushes_after_interval() {
        assert!(!digest_due(1_000, 1_000 + 59 * 60, 60), "未满间隔不发");
        assert!(digest_due(1_000, 1_000 + 60 * 60, 60), "满间隔该发");
        assert!(!digest_due(1_000, 1_000 + 60 * 60, 0), "0 分钟视为未启用");
    }

    #[test]
    fn digest_cfg_reads_minutes() {
        let path = std::env::temp_dir().join("nq-digest-cfg.db");
        let db = dead_fixture(&path);
        {
            let conn = db.lock();
            let cfg = alert_cfg(&conn);
            assert!(!cfg.digest_enabled, "摘要默认关闭");
            assert_eq!(cfg.digest_minutes, 60, "摘要间隔缺省 60 分钟");
        }
        db.lock()
            .execute(
                "INSERT OR REPLACE INTO app_settings VALUES('system',?1,0)",
                [json!({"alert_digest_enabled": true, "alert_digest_minutes": 15}).to_string()],
            )
            .unwrap();
        {
            let conn = db.lock();
            let cfg = alert_cfg(&conn);
            assert!(cfg.digest_enabled);
            assert_eq!(cfg.digest_minutes, 15);
        }
        db.lock()
            .execute(
                "INSERT OR REPLACE INTO app_settings VALUES('system',?1,0)",
                [json!({"alert_digest_minutes": 100_000}).to_string()],
            )
            .unwrap();
        let clamped = {
            let conn = db.lock();
            alert_cfg(&conn).digest_minutes
        };
        assert_eq!(clamped, 1440, "超过上限应被夹到 1440");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn warn_is_buffered_when_digest_enabled() {
        let (port, hits) = fake_webhook().await;
        let path = std::env::temp_dir().join("nq-digest-buffer.db");
        let db = dead_fixture(&path);
        db.lock()
            .execute(
                "INSERT OR REPLACE INTO app_settings VALUES('system',?1,0)",
                [json!({
                    "alert_enabled": true,
                    "alert_webhook_url": format!("http://127.0.0.1:{port}/hook"),
                    "alert_digest_enabled": true,
                    "alert_digest_minutes": 5
                })
                .to_string()],
            )
            .unwrap();
        let state = SystemState::new(db.clone(), crate::now());
        dispatch_alert(&state, &alert_event())
            .await
            .expect("缓冲不该报错");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "摘要模式下 warn 不应即时推送"
        );
        assert_eq!(
            state.alert_digest.lock().unwrap().len(),
            1,
            "warn 应进入摘要缓冲"
        );
        let logged: i64 = db
            .lock()
            .query_row("SELECT COUNT(*) FROM alert_logs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(logged, 0, "缓冲阶段不写 alert_logs");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn error_is_sent_immediately_when_digest_enabled() {
        let (port, hits) = fake_webhook().await;
        let path = std::env::temp_dir().join("nq-digest-error.db");
        let db = dead_fixture(&path);
        db.lock()
            .execute(
                "INSERT OR REPLACE INTO app_settings VALUES('system',?1,0)",
                [json!({
                    "alert_enabled": true,
                    "alert_webhook_url": format!("http://127.0.0.1:{port}/hook"),
                    "alert_digest_enabled": true,
                    "alert_digest_minutes": 60
                })
                .to_string()],
            )
            .unwrap();
        let state = SystemState::new(db.clone(), crate::now());
        let event = AlertEvent {
            id: 2,
            ts: crate::now(),
            level: "error".to_string(),
            category: "risk".to_string(),
            message: "账户异常".to_string(),
        };
        dispatch_alert(&state, &event)
            .await
            .expect("error 应即时推送");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "摘要模式下 error 仍然即时推送"
        );
        assert!(state.alert_digest.lock().unwrap().is_empty());
        let logged: i64 = db
            .lock()
            .query_row("SELECT COUNT(*) FROM alert_logs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(logged, 1, "即时推送应写 alert_logs");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn digest_flush_sends_and_logs() {
        let (port, hits) = fake_webhook().await;
        let path = std::env::temp_dir().join("nq-digest-flush.db");
        let db = dead_fixture(&path);
        db.lock()
            .execute(
                "INSERT OR REPLACE INTO app_settings VALUES('system',?1,0)",
                [json!({
                    "alert_enabled": true,
                    "alert_webhook_url": format!("http://127.0.0.1:{port}/hook"),
                    "alert_digest_enabled": true,
                    "alert_digest_minutes": 5
                })
                .to_string()],
            )
            .unwrap();
        let state = SystemState::new(db.clone(), crate::now());
        state.alert_digest.lock().unwrap().push(AlertEvent {
            id: 9,
            ts: crate::now() - 6 * 60,
            level: "warn".to_string(),
            category: "risk".to_string(),
            message: "回撤超限".to_string(),
        });
        flush_digest(&state).await.expect("摘要应推送成功");
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "到点应发出一次合并推送"
        );
        assert!(
            state.alert_digest.lock().unwrap().is_empty(),
            "推送后缓冲应清空"
        );
        let (category, message): (String, String) = db
            .lock()
            .query_row(
                "SELECT category, message FROM alert_logs ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(category, "digest");
        assert!(message.contains("warn 摘要 1 条"), "摘要标题应含条数");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_minimum_level_decides_what_gets_pushed() {
        assert!(level_at_least("error", "warn"));
        assert!(level_at_least("warn", "warn"));
        assert!(!level_at_least("info", "warn"));
        assert!(!level_at_least("weird", "warn"));
        assert!(level_at_least("info", "info"));
    }

    #[tokio::test]
    async fn alert_test_without_channels_is_400_and_logs_are_readable() {
        let path = std::env::temp_dir().join("nq-alerts.db");
        let db = dead_fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(SystemState::new(db.clone(), crate::now()));
        let (status, body) =
            request_json(app.clone(), "POST", "/api/v1/system/alerts/test?level=warn").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("未配置"));
        db.lock()
            .execute(
                "INSERT INTO alert_logs VALUES(1,100,'warn','test','测试告警','webhook',1,'')",
                [],
            )
            .unwrap();
        let (status, logs) = request_json(app, "GET", "/api/v1/system/alerts/logs").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(logs["items"][0]["channels"], "webhook");
        assert_eq!(logs["items"][0]["ok"], true);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exec_endpoint_accepts_tcp_or_unix_socket() {
        assert_eq!(
            exec_endpoint_from(Some("127.0.0.1:9101".into()), Some("/tmp/x.sock".into()))
                .as_deref(),
            Some("tcp:127.0.0.1:9101"),
            "TCP 优先"
        );
        assert_eq!(
            exec_endpoint_from(None, Some(" /run/nanoquant/exec.sock ".into())).as_deref(),
            Some("unix:/run/nanoquant/exec.sock"),
            "Linux 生产用 UDS 也必须被识别"
        );
        assert_eq!(exec_endpoint_from(Some("  ".into()), Some("".into())), None);
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_endpoint_is_probed() {
        let dir = std::env::temp_dir().join(format!("nq-exec-{}", crate::now_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("exec.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(endpoint_reachable(&format!("unix:{}", path.display())));
        drop(listener);
        assert!(!endpoint_reachable("unix:/nonexistent/nq-exec.sock"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn exec_core_status_self_test_and_reset() {
        let path = std::env::temp_dir().join("nq-exec-core.db");
        let db = dead_fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(SystemState::new(db.clone(), crate::now()));
        let (status, body) = request_json(app.clone(), "GET", "/api/v1/system/exec-core").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("trading_mode").is_some());
        assert!(body.get("counts").is_some());
        assert!(body.get("order_ledger").is_some());
        assert_eq!(body["connected"], json!(false));
        assert_eq!(body["configured"], json!(false));
        assert!(
            body["segments"].is_null(),
            "未连接时七段指标为 null: {body}"
        );
        let (_, tested) =
            request_json(app.clone(), "POST", "/api/v1/system/exec-core/self-test").await;
        assert!(
            tested.get("detail").is_some(),
            "自检必须给出逐项结论: {tested}"
        );
        // 默认执行内核为必选组件：未连接就是故障，不能再算通过
        assert_eq!(tested["ok"], json!(false), "未连接时自检必须失败: {tested}");
        assert_eq!(tested["outcome"], json!("error"));
        assert_eq!(tested["configured"], json!(false));
        let (_, after) = request_json(app.clone(), "GET", "/api/v1/system/exec-core").await;
        assert_eq!(
            after["counts"]["errors"],
            json!(1),
            "未连接的自检应计入失败: {after}"
        );
        let (_, reset) = request_json(app.clone(), "POST", "/api/v1/system/exec-core/reset").await;
        assert_eq!(reset["counts"]["checks"], 0);
        assert_eq!(reset["counts"]["errors"], 0);
        assert_eq!(reset["core_reset"], json!(false), "未连接时无内核可清零");
        let events: i64 = db
            .lock()
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE category='exec'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(events >= 1, "reset 必须写审计");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn the_diagnostics_heartbeat_follows_the_scan_interval() {
        let path = std::env::temp_dir().join("nq-diag-heartbeat.db");
        let db = dead_fixture(&path);
        {
            let conn = db.lock();
            conn.execute(
                "INSERT OR REPLACE INTO app_settings(key, value, updated_at) VALUES('engine', ?1, 0)",
                [json!({"enabled": true, "cycle_interval_sec": 600}).to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO engine_state(key, value, updated_at) VALUES('enabled', 'true', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO engine_state(key, value, updated_at) VALUES('last_cycle_ts', ?1, 0)",
                [(crate::now() - 300).to_string()],
            )
            .unwrap();
        }
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(SystemState::new(db.clone(), crate::now()));
        let (status, body) = request_json(app, "GET", "/api/v1/system/diagnostics").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let heartbeat = body["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == "引擎心跳")
            .expect("诊断必须有引擎心跳项")
            .clone();
        assert_eq!(
            heartbeat["ok"],
            json!(true),
            "间歇期不该报心跳超时: {heartbeat}"
        );
        let detail = heartbeat["detail"].as_str().unwrap();
        assert!(detail.contains("判活阈值"), "{detail}");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn the_logs_endpoint_refuses_to_read_outside_the_log_directory() {
        let path = std::env::temp_dir().join("nq-logs-guard.db");
        let db = dead_fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(SystemState::new(db.clone(), crate::now()));
        // 日志目录之外的文件一律拒绝：不然这个接口就是任意文件读取，
        // 拿到管理员口令的人能读 .master_key 和交易所密钥。
        for uri in [
            "/api/v1/system/logs?path=%2Fetc%2Fpasswd",
            "/api/v1/system/logs?path=..%2F..%2Fetc%2Fpasswd",
            "/api/v1/system/logs?path=C%3A%5CWindows%5Cwin.ini",
        ] {
            let (status, body) = request_json(app.clone(), "GET", uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} 不该被放行: {body}");
        }
        // 默认路径（服务自己的日志文件）不受影响。
        let (status, _) = request_json(app, "GET", "/api/v1/system/logs?lines=10").await;
        assert_eq!(status, StatusCode::OK);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exec_stats_never_report_a_placeholder_as_a_measurement() {
        let mut stats = ExecStats::default();
        // 没有实测样本时必须是 null：前端据此显示「—」，不能显示 0ms
        let view = stats.latency_view();
        assert!(view["last"].is_null(), "无样本时 last 必须是 null: {view}");
        assert!(view["p50"].is_null());
        assert!(view["p95"].is_null());
        assert!(view["p99"].is_null());
        assert_eq!(view["samples"], 0);

        stats.observe_latency(Some(1.0));
        stats.observe_latency(Some(2.0));
        stats.observe_latency(Some(3.0));
        let view = stats.latency_view();
        assert_eq!(view["last"], json!(3.0), "最近一次是最后记的样本");
        assert_eq!(view["p50"], json!(2.0));
        assert_eq!(view["p95"], json!(3.0));
        assert_eq!(view["p99"], json!(3.0));
        assert_eq!(view["samples"], 3);

        // 样本上限：不能无限涨
        for _ in 0..(EXEC_LATENCY_SAMPLES + 10) {
            stats.observe_latency(Some(1.0));
        }
        assert_eq!(stats.latencies_ms.len(), EXEC_LATENCY_SAMPLES);

        // 计数语义：真失败记 errors，连通但没 Secret 记 skipped，其余记 ok
        let mut stats = ExecStats::default();
        stats.record_check(ExecCheck::Error);
        assert_eq!(stats.checks, 1);
        assert_eq!(stats.errors, 1);
        stats.record_check(ExecCheck::Skipped);
        assert_eq!(stats.skipped, 1, "连通但没配 Secret 记 skipped");
        stats.record_check(ExecCheck::Ok);
        assert_eq!(stats.ok, 1);
    }

    #[tokio::test]
    async fn exchange_credentials_are_masked_and_deletable() {
        let path = std::env::temp_dir().join("nq-credentials.db");
        let db = dead_fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(
            SystemState::new(db.clone(), crate::now()).with_auth(test_auth_valid_key()),
        );
        let api_key = "a".repeat(64);
        let api_secret = "b".repeat(64);
        let masked = format!("{}******{}", &api_key[..4], &api_key[60..]);
        let (status, saved) = request_json_body(
            app.clone(),
            "POST",
            "/api/v1/settings/credentials/exchange",
            json!({"env": "demo", "api_key": &api_key, "api_secret": &api_secret}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["env"], "demo");
        let (status, view) =
            request_json(app.clone(), "GET", "/api/v1/settings/credentials/exchange").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["demo"]["configured"], true);
        assert_eq!(view["demo"]["api_key_masked"], masked);
        assert!(!view.to_string().contains(&api_key));
        assert!(!view.to_string().contains(&api_secret));
        let stored: String = db
            .lock()
            .query_row(
                "SELECT value FROM app_settings WHERE key='data'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored.contains(&api_key));
        assert!(!stored.contains(&api_secret));
        let (status, cleared) = request_json(
            app.clone(),
            "DELETE",
            "/api/v1/settings/credentials/exchange?env=demo",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(cleared["env"], "demo");
        let (_, view) = request_json(app, "GET", "/api/v1/settings/credentials/exchange").await;
        assert_eq!(view["demo"]["configured"], false);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn exchange_credentials_reject_key_with_leading_dot() {
        let path = std::env::temp_dir().join("nq-credentials-dot.db");
        let db = dead_fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(
            SystemState::new(db.clone(), crate::now()).with_auth(test_auth_valid_key()),
        );
        let bad_key = format!(".{}", "a".repeat(64));
        let (status, body) = request_json_body(
            app.clone(),
            "POST",
            "/api/v1/settings/credentials/exchange",
            json!({"env": "demo", "api_key": bad_key, "api_secret": "b".repeat(64)}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("64 位字母数字"),
            "错误信息要说明 Key 格式: {body}"
        );
        let (_, view) = request_json(app, "GET", "/api/v1/settings/credentials/exchange").await;
        assert_eq!(view["demo"]["configured"], false, "坏 Key 不能写入");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn diagnostics_has_ten_timed_checks_and_isolates_credential_failure() {
        let path = std::env::temp_dir().join("nq-diagnostics-nine.db");
        let db = dead_fixture(&path);
        db.lock()
            .execute_batch(
                "DELETE FROM write_failures; \
                 INSERT INTO app_settings VALUES('data','{\"demo_api_key_enc\":\"invalid-token\"}',0);",
            )
            .unwrap();
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(SystemState::new(db, crate::now()).with_auth(test_auth_valid_key()));
        let (status, body) = request_json(app, "GET", "/api/v1/system/diagnostics").await;
        assert_eq!(status, StatusCode::OK);
        let checks = body["checks"].as_array().unwrap();
        assert!(checks.len() >= 9, "诊断项必须 ≥9: {}", checks.len());
        for check in checks {
            assert!(check["latency_ms"].is_number(), "每项都要有耗时: {check}");
        }
        let failed = checks
            .iter()
            .filter(|check| check["ok"] != true)
            .collect::<Vec<_>>();
        assert!(!failed.is_empty(), "凭证故障必须被诊断出来: {failed:?}");
        let credential = failed
            .iter()
            .find(|check| check["name"] == "交易凭证")
            .unwrap_or_else(|| panic!("缺少交易凭证失败项: {failed:?}"));
        assert!(credential["detail"].as_str().unwrap().contains("无法解密"));
        // 未连接执行内核是默认必选组件的故障，会同时让「交易通道」失败；
        // 除此之外不应有别的检查被凭证故障牵连。
        for check in &failed {
            let name = check["name"].as_str().unwrap_or("");
            assert!(
                ["交易凭证", "交易通道", "执行内核"].contains(&name),
                "凭证故障不应影响 {name}: {check}"
            );
        }
        let find = |name: &str| {
            checks
                .iter()
                .find(|check| check["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("缺少诊断项 {name}"))
        };
        assert_eq!(
            find("执行内核")["ok"],
            false,
            "默认执行内核未连接必须算异常"
        );
        assert!(
            !find("存储")["detail"]
                .as_str()
                .unwrap_or("")
                .contains("0 字节"),
            "剩余空间要显示真实值"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn metrics_hide_account_fields_without_token() {
        let path = std::env::temp_dir().join("nq-metrics-secret.db");
        let db = dead_fixture(&path);
        {
            let conn = db.lock();
            conn.execute_batch(
                "INSERT INTO equity_points VALUES(1,100,'demo',12345,1000,0,0,1); \
                 INSERT INTO signals VALUES(1,100,'BTCUSDT','long',0.8,1); \
                 INSERT INTO positions(symbol,side,qty,entry_price,mark_price,status,mode) \
                 VALUES('BTCUSDT','long',1,100,100,'open','demo');",
            )
            .unwrap();
        }
        let auth = test_auth();
        let token = auth::issue_token("admin", 12, 1, b"master-key", "salt", crate::now()).0;
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(SystemState::new(db, crate::now()).with_auth(auth));
        let (status, public) = request_text(app.clone(), "/api/v1/metrics", None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(public.contains("nanoquant_up 1"));
        assert!(!public.contains("nanoquant_account_equity_usdt"));
        assert!(!public.contains("12345"));
        let (status, authorized) = request_text(app, "/api/v1/metrics", Some(&token)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(authorized.contains("nanoquant_account_equity_usdt 12345"));
        let _ = std::fs::remove_file(&path);
    }
}
