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
    logs_path: std::path::PathBuf,
    archives_dir: std::path::PathBuf,
    // 告警冷却记账表；告警冷却链路尚未接线，接线后取消豁免。
    #[allow(dead_code)]
    alert_last: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, i64>>>,
    exec: std::sync::Arc<std::sync::Mutex<ExecStats>>,
}

/// 执行内核的实测计数与往返耗时样本。
///
/// 每个数字都必须来自真实观测：探活成功记 ok、失败记 errors、
/// 已连通但没配 Secret（没法比对签名）记 skipped。
/// 签名逐字节比对不在进程内做（主服务用 Rust 内核自己签名），
/// 真实签名比对用 `ops verify-sidecar`。
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

impl ExecStats {
    /// 记录一次探活往返耗时。任何触发探活的地方都记，样本才有意义。
    fn observe_latency(&mut self, latency_ms: Option<f64>) {
        let Some(ms) = latency_ms else { return };
        self.latencies_ms.push(ms);
        if self.latencies_ms.len() > EXEC_LATENCY_SAMPLES {
            self.latencies_ms.remove(0);
        }
    }

    /// 记一次自检：连通且配了 Secret 才算「可比对」。
    fn record_check(&mut self, connected: bool, has_key: bool) {
        self.checks += 1;
        if !connected {
            self.errors += 1;
        } else if !has_key {
            self.skipped += 1;
        } else {
            self.ok += 1;
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
            logs_path: logs_dir.join("nanoquant.log"),
            archives_dir: home.join("data").join("archives"),
            alert_last: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::HashMap::new()),
            ),
            exec: std::sync::Arc::new(std::sync::Mutex::new(ExecStats::default())),
        }
    }

    pub fn with_auth(mut self, auth: AuthState) -> Self {
        self.auth = Some(auth);
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
        .route("/system/exec-sidecar", get(exec_sidecar))
        .route("/system/exec-sidecar/self-test", post(exec_self_test))
        .route("/system/exec-sidecar/reconcile", get(exec_reconcile))
        .route("/system/exec-sidecar/reset", post(exec_reset))
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
    let conn = state.db.conn.lock().expect("db");
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
    let conn = state.db.conn.lock().expect("db");
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
        let conn = state.db.conn.lock().expect("db");
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
    let conn = db.conn.lock().expect("db");
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
    let conn = state.db.conn.lock().expect("db");
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
    webhook: String,
    error_webhook: String,
    telegram_token: String,
    telegram_chat_id: String,
    // 告警冷却与静默时段：判定函数已实现但告警发送尚未接入，接线后取消豁免。
    #[allow(dead_code)]
    cooldown_sec: i64,
    #[allow(dead_code)]
    quiet_hours: String,
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
    AlertCfg {
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
    let conn = state.db.conn.lock().expect("db");
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
        let conn = state.db.conn.lock().expect("db");
        alert_cfg(&conn)
    };
    let mut urls: Vec<String> = cfg
        .webhook
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect();
    if level == "error" && !cfg.error_webhook.is_empty() {
        urls = cfg
            .error_webhook
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect();
    }
    let telegram = !cfg.telegram_token.is_empty() && !cfg.telegram_chat_id.is_empty();
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
#[cfg(test)]
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
#[cfg(test)]
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

/// 执行内核探活结果。
struct ExecProbe {
    endpoint: Option<String>,
    connected: bool,
    algo: String,
    detail: String,
    /// 探一帧 health 的实测往返耗时（毫秒）。没探到就是 None，不填 0。
    latency_ms: Option<f64>,
}

/// 探一次执行内核：发一帧 health 协议，能回就算「有内核」。
/// 只 ping 端口不算数——端口开着不代表内核活着。
fn exec_probe_state() -> ExecProbe {
    let Some(endpoint) = exec_endpoint() else {
        return ExecProbe {
            endpoint: None,
            connected: false,
            algo: String::new(),
            detail: "未部署执行内核（可选）：Rust 内核直连交易所签名下单，不影响交易".to_string(),
            latency_ms: None,
        };
    };
    let started = std::time::Instant::now();
    match exec_probe(&endpoint) {
        Some(response) => {
            let algo = response
                .get("algo")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            ExecProbe {
                endpoint: Some(endpoint.clone()),
                connected: true,
                algo,
                detail: format!("执行内核已连接（端点 {endpoint}）"),
                latency_ms: Some(round(started.elapsed().as_secs_f64() * 1000.0, 2)),
            }
        }
        None => ExecProbe {
            endpoint: Some(endpoint.clone()),
            connected: false,
            algo: String::new(),
            detail: format!("执行内核端点不可达（{endpoint}），保护单代发会失败"),
            latency_ms: None,
        },
    }
}

/// 发一帧 health 协议，返回 sidecar 的应答。
fn exec_probe(endpoint: &str) -> Option<Value> {
    if let Some(path) = endpoint.strip_prefix("unix:") {
        #[cfg(unix)]
        {
            return exec_probe_unix(path);
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            return None;
        }
    }
    exec_probe_tcp(endpoint.strip_prefix("tcp:").unwrap_or(endpoint))
}

fn exec_probe_tcp(address: &str) -> Option<Value> {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    let stream = TcpStream::connect(address).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(800)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(800)))
        .ok()?;
    let mut writer = stream.try_clone().ok()?;
    writer
        .write_all(b"{\"id\":\"probe\",\"op\":\"health\"}\n")
        .ok()?;
    writer.flush().ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str::<Value>(line.trim()).ok()
}

#[cfg(unix)]
fn exec_probe_unix(path: &str) -> Option<Value> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let mut stream = UnixStream::connect(path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(800)))
        .ok()?;
    let mut writer = stream.try_clone().ok()?;
    writer
        .write_all(b"{\"id\":\"probe\",\"op\":\"health\"}\n")
        .ok()?;
    writer.flush().ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str::<Value>(line.trim()).ok()
}

/// 当前交易环境是否配置了交易所 Secret（sidecar 要用它签名）。
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

async fn exec_sidecar(State(state): State<SystemState>) -> Json<Value> {
    let conn = state.db.conn.lock().expect("db");
    let trading_mode = current_mode(&conn);
    let has_key = exec_secret_configured(&conn, &trading_mode);
    let probe = exec_probe_state();
    let stats = state
        .exec
        .lock()
        .map(|mut stats| {
            stats.observe_latency(probe.latency_ms);
            stats.clone()
        })
        .unwrap_or_default();
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM orders WHERE status NOT IN ('FILLED','REJECTED','CANCELED','EXPIRED')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let verdict = if probe.connected && !has_key {
        format!(
            "{}；但当前交易环境没有配置交易所 Secret，sidecar 不能用于真实签名",
            probe.detail
        )
    } else {
        probe.detail.clone()
    };
    Json(json!({
        "trading_mode": trading_mode,
        "configured": probe.endpoint.is_some(),
        "deployed": probe.connected,
        "connected": probe.connected,
        "target": probe.endpoint.clone().unwrap_or_default(),
        "algo": probe.algo,
        "has_key": has_key,
        "counts": {
            "checks": stats.checks,
            "ok": stats.ok,
            "errors": stats.errors,
            "skipped": stats.skipped,
        },
        "latency_ms": stats.latency_view(),
        "order_ledger": {"pending": pending},
        "verdict": verdict,
        "reason": verdict,
    }))
}

async fn exec_self_test(State(state): State<SystemState>) -> Json<Value> {
    let conn = state.db.conn.lock().expect("db");
    let mode = current_mode(&conn);
    let has_key = conn
        .query_row(
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
        .unwrap_or(false);
    drop(conn);
    let probe = exec_probe_state();
    if let Ok(mut stats) = state.exec.lock() {
        stats.observe_latency(probe.latency_ms);
        stats.record_check(probe.connected, has_key);
    }
    json!({
        "ok": probe.connected,
        "connected": probe.connected,
        "endpoint": probe.endpoint.clone().unwrap_or_default(),
        "algo": probe.algo,
        "has_key": has_key,
        "detail": if probe.connected {
            "端点可达（health 协议应答正常）；签名逐字节比对用 ops verify-sidecar".to_string()
        } else {
            probe.detail.clone()
        },
    })
    .into()
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
    let conn = state.db.conn.lock().expect("db");
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
    if let Ok(conn) = state.db.conn.lock() {
        let _ = conn.execute(
            "INSERT INTO engine_events(ts, level, category, message, payload) \
             VALUES(?1, 'info', 'exec', '清零执行内核比对计数', '{}')",
            [crate::now()],
        );
    }
    json!({
        "ok": true,
        "counts": {"checks": 0, "ok": 0, "errors": 0, "skipped": 0},
        "detail": "仅清零观测计数，签名密钥与端点配置未被修改",
    })
    .into()
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

fn mask_secret(plain: &str) -> String {
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
    let conn = state.db.conn.lock().expect("db");
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
        match crate::secret::encrypt_secret(&master_key, api_key.trim()) {
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
        match crate::secret::encrypt_secret(&master_key, api_secret.trim()) {
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
    let conn = state.db.conn.lock().expect("db");
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
    let conn = state.db.conn.lock().expect("db");
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
    let conn = state.db.conn.lock().expect("db");
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
    let path = query
        .path
        .map(std::path::PathBuf::from)
        .unwrap_or(state.logs_path);
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
    Json(json!({"lines": lines, "count": lines.len()}))
}

async fn info(State(state): State<SystemState>) -> impl IntoResponse {
    let conn = state.db.conn.lock().expect("db");
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
    let conn = state.db.conn.lock().expect("db");
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
    // 实盘走 HttpGateway 直连币安；执行内核只是可选组件，不能当成交易通道的必要条件。
    let channel_ok = mode == "demo" || connected || credentials_ok;
    checks.push(diag_check(
        "交易通道",
        channel_ok,
        if mode == "demo" {
            "模拟盘本地记账，无需执行内核".to_string()
        } else if connected {
            "实盘执行内核端点可达".to_string()
        } else if credentials_ok {
            "实盘凭证可用，直连交易所；未配置执行内核（可选）".to_string()
        } else {
            "实盘缺少可用的交易凭证".to_string()
        },
        started,
    ));

    let started = std::time::Instant::now();
    let enabled = engine_value(&conn, "enabled") == "true";
    let last_cycle = engine_int(&conn, "last_cycle_ts");
    let engine_ok = !enabled || (last_cycle > 0 && crate::now().saturating_sub(last_cycle) < 180);
    checks.push(diag_check(
        "引擎心跳",
        engine_ok,
        if !enabled {
            "引擎已停止".to_string()
        } else if engine_ok {
            format!("最近心跳 {} 秒前", crate::now().saturating_sub(last_cycle))
        } else {
            "引擎心跳超时".to_string()
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
    // 未配置视为正常（可选组件）；只有显式配置了却连不上才算异常。
    let exec_probe = exec_probe_state();
    let exec_ok = exec_probe.endpoint.is_none() || exec_probe.connected;
    checks.push(diag_check(
        "执行内核",
        exec_ok,
        if exec_probe.connected {
            format!(
                "{}（端点 {}，算法 {}）",
                exec_probe.detail,
                exec_probe.endpoint.clone().unwrap_or_default(),
                if exec_probe.algo.is_empty() {
                    "未知"
                } else {
                    &exec_probe.algo
                }
            )
        } else {
            exec_probe.detail.clone()
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
        db.conn
            .lock()
            .unwrap()
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

        let conn = db.conn.lock().unwrap();
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
        let conn = db.conn.lock().unwrap();
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
            let conn = db.conn.lock().unwrap();
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
            let conn = db.conn.lock().unwrap();
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
            .conn
            .lock()
            .unwrap()
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
            let conn = db.conn.lock().unwrap();
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
            .conn
            .lock()
            .unwrap()
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
        db.conn
            .lock()
            .unwrap()
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
    async fn exec_sidecar_status_self_test_and_reset() {
        let path = std::env::temp_dir().join("nq-exec-sidecar.db");
        let db = dead_fixture(&path);
        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(SystemState::new(db.clone(), crate::now()));
        let (status, body) = request_json(app.clone(), "GET", "/api/v1/system/exec-sidecar").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("trading_mode").is_some());
        assert!(body.get("counts").is_some());
        assert!(body.get("order_ledger").is_some());
        let (_, tested) =
            request_json(app.clone(), "POST", "/api/v1/system/exec-sidecar/self-test").await;
        assert!(
            tested.get("detail").is_some(),
            "自检必须给出逐项结论: {tested}"
        );
        let (_, reset) =
            request_json(app.clone(), "POST", "/api/v1/system/exec-sidecar/reset").await;
        assert_eq!(reset["counts"]["checks"], 0);
        assert_eq!(reset["counts"]["ok"], 0);
        let events: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM engine_events WHERE category='exec'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(events >= 1, "reset 必须写审计");
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

        // 计数语义：探活失败是 errors，连通但没 Secret 是 skipped
        let mut stats = ExecStats::default();
        stats.record_check(false, false);
        assert_eq!(stats.checks, 1);
        assert_eq!(stats.errors, 1);
        stats.record_check(true, false);
        assert_eq!(stats.skipped, 1, "连通但没配 Secret 记 skipped");
        stats.record_check(true, true);
        assert_eq!(stats.ok, 1);
    }

    #[tokio::test]
    async fn exchange_credentials_are_masked_and_deletable() {
        let path = std::env::temp_dir().join("nq-credentials.db");
        let db = dead_fixture(&path);
        let app = Router::new().nest("/api/v1", routes()).with_state(
            SystemState::new(db.clone(), crate::now()).with_auth(test_auth_valid_key()),
        );
        let (status, saved) = request_json_body(
            app.clone(),
            "POST",
            "/api/v1/settings/credentials/exchange",
            json!({"env": "demo", "api_key": "abcdefghijk", "api_secret": "secret123456"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["env"], "demo");
        let (status, view) =
            request_json(app.clone(), "GET", "/api/v1/settings/credentials/exchange").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(view["demo"]["configured"], true);
        assert_eq!(view["demo"]["api_key_masked"], "abcd******hijk");
        assert!(!view.to_string().contains("abcdefghijk"));
        assert!(!view.to_string().contains("secret123456"));
        let stored: String = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM app_settings WHERE key='data'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored.contains("abcdefghijk"));
        assert!(!stored.contains("secret123456"));
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
    async fn diagnostics_has_ten_timed_checks_and_isolates_credential_failure() {
        let path = std::env::temp_dir().join("nq-diagnostics-nine.db");
        let db = dead_fixture(&path);
        db.conn
            .lock()
            .unwrap()
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
        assert_eq!(failed.len(), 1, "注入凭证故障只影响该项: {failed:?}");
        assert_eq!(failed[0]["name"], "交易凭证");
        assert!(failed[0]["detail"].as_str().unwrap().contains("无法解密"));
        let find = |name: &str| {
            checks
                .iter()
                .find(|check| check["name"] == name)
                .cloned()
                .unwrap_or_else(|| panic!("缺少诊断项 {name}"))
        };
        assert_eq!(
            find("执行内核")["ok"],
            true,
            "未配置执行内核是可选组件，不能算异常"
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
            let conn = db.conn.lock().unwrap();
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
