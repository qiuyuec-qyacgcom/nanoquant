//! 备份与恢复：选择性配置/表/文件打包、SHA-256 校验、Fernet 加密、上传识别、
//! 事务还原、保留策略与异盘镜像。主密钥永不入包。
//!
//! 自动备份判定保持 legacy 内核（已退役） 语义：一天一次；失败后回拨到一小时后重试，
//! 避免大库失败时每 15 分钟打一次磁盘。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::app_state::AppState;
use crate::archive::{self, Entry};
use crate::db::Db;

const AUTO_INTERVAL_SEC: i64 = 24 * 3600;
const AUTO_RETRY_SEC: i64 = 3600;
const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_LISTED_FILES: usize = 200;

const SETTINGS_GROUPS: [&str; 13] = [
    "engine",
    "risk",
    "strategy",
    "ai",
    "data",
    "news",
    "evolution",
    "backup",
    "security",
    "system",
    "public",
    "signal_follow",
    "journal",
];
const DEFAULT_TABLES: [&str; 6] = [
    "app_settings",
    "ai_providers",
    "trading_symbols",
    "mantras",
    "trades",
    "positions",
];
const BACKUP_TABLES: [&str; 14] = [
    "app_settings",
    "ai_providers",
    "trading_symbols",
    "signals",
    "orders",
    "positions",
    "trades",
    "mantras",
    "evolution_runs",
    "news_items",
    "equity_points",
    "cooldowns",
    "engine_events",
    "backup_jobs",
];

/// 从未执行过返回 0；否则是上次执行时间加一天。
pub fn next_run_ts(last_run_ts: i64) -> i64 {
    if last_run_ts <= 0 {
        0
    } else {
        last_run_ts + AUTO_INTERVAL_SEC
    }
}

/// 失败时写回的「上次执行时间」：让下一次到点恰好落在一小时后。
pub fn retry_anchor(now: i64) -> i64 {
    now - (AUTO_INTERVAL_SEC - AUTO_RETRY_SEC)
}

/// 自动备份的检查间隔（秒）：与备份页承诺的「下一轮检查（≤15 分钟）」一致。
const AUTO_CHECK_INTERVAL_SEC: u64 = 15 * 60;

/// 写回「上次执行时间」：自动备份的成功锚点与失败重试锚点都落在这里。
fn write_last_run_ts(conn: &Connection, last_run_ts: i64) -> Result<(), String> {
    let mut cfg = setting(conn, "backup");
    if !cfg.is_object() {
        cfg = json!({});
    }
    cfg["last_run_ts"] = json!(last_run_ts);
    let value = serde_json::to_string(&cfg).map_err(|error| error.to_string())?;
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES('backup', ?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        rusqlite::params![value, crate::now()],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// 到点且开启了每日自动备份时执行一次；成功写回本次时间，失败写回一小时后重试的锚点。
/// 返回 `Ok(None)` 表示本轮未到期或未开启自动备份。
pub fn maybe_auto_backup(state: &AppState) -> Result<Option<Value>, String> {
    let Some(db) = state.db.clone() else {
        return Ok(None);
    };
    let (auto_daily, last_run_ts) = {
        let conn = db.conn.lock().map_err(|_| "数据库锁被占用".to_string())?;
        let cfg = backup_config(&conn);
        (
            cfg["auto_daily"].as_bool().unwrap_or(true),
            cfg["last_run_ts"].as_i64().unwrap_or(0),
        )
    };
    if !auto_daily {
        return Ok(None);
    }
    let now = crate::now();
    if next_run_ts(last_run_ts) > now {
        return Ok(None);
    }
    let input = CreateIn {
        name: "auto".to_string(),
        settings_groups: None,
        tables: None,
        files: None,
        encrypt: None,
        created_by: Some("auto".to_string()),
    };
    let outcome = create_backup_inner(state, input);
    // 成功记本次时间；失败按「一小时后重试」回拨，避免大库失败时每 15 分钟打一次磁盘。
    let anchor = if outcome.is_ok() {
        now
    } else {
        retry_anchor(now)
    };
    {
        let conn = db.conn.lock().map_err(|_| "数据库锁被占用".to_string())?;
        write_last_run_ts(&conn, anchor)?;
    }
    match outcome {
        Ok(value) => Ok(Some(value)),
        Err((status, body)) => {
            let detail = body["detail"].as_str().unwrap_or("未知错误").to_string();
            state.events.record(
                "error",
                "backup",
                &format!("自动备份失败: {detail}"),
                json!({"next_retry_sec": AUTO_RETRY_SEC}),
            );
            Err(format!("自动备份失败({status}): {detail}"))
        }
    }
}

/// 自动备份调度：每 15 分钟检查一次，到点就备份一次。
/// 与交易引擎解耦：引擎停用时同样要备份。
pub async fn spawn_auto_backup_task(state: AppState) {
    loop {
        let snapshot = state.clone();
        match tokio::task::spawn_blocking(move || maybe_auto_backup(&snapshot)).await {
            Ok(Err(error)) => tracing::warn!(%error, "自动备份未完成"),
            Ok(Ok(_)) => {}
            Err(error) => tracing::error!(%error, "自动备份任务异常"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(AUTO_CHECK_INTERVAL_SEC)).await;
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/backup/options", get(options))
        .route("/backup/jobs", get(jobs))
        .route("/backup/create", post(create))
        .route(
            "/backup/upload",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/backup/{job_id}/restore", post(restore))
        .route("/backup/{job_id}/download", get(download))
        .route("/backup/{job_id}", axum::routing::delete(delete_job))
}

#[derive(Deserialize)]
struct CreateIn {
    #[serde(default)]
    name: String,
    settings_groups: Option<Vec<String>>,
    tables: Option<Vec<String>>,
    files: Option<Vec<String>>,
    encrypt: Option<bool>,
    /// 任务来源：手工创建留空（记 manual），自动备份记 auto。
    /// 外部请求无法伪造——HTTP 入口会把这个字段清空。
    #[serde(default)]
    created_by: Option<String>,
}

fn data_dir(db: &Db) -> PathBuf {
    Path::new(&db.path)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn home_dir(db: &Db) -> PathBuf {
    data_dir(db)
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn backups_dir(db: &Db) -> PathBuf {
    data_dir(db).join("backups")
}

fn setting(conn: &Connection, key: &str) -> Value {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key=?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|raw| serde_json::from_str(&raw).ok())
    .unwrap_or_else(|| crate::settings_api::defaults_of(key))
}

fn backup_config(conn: &Connection) -> Value {
    setting(conn, "backup")
}

fn bad(status: StatusCode, detail: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"detail": detail.into()})))
}

fn storage(state: &AppState) -> Result<Db, (StatusCode, Json<Value>)> {
    state
        .db
        .clone()
        .ok_or_else(|| bad(StatusCode::SERVICE_UNAVAILABLE, "数据库未就绪"))
}

fn engine_enabled(db: &Db) -> bool {
    db.conn
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT value FROM engine_state WHERE key='enabled'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .map(|value| value == "true")
        .unwrap_or(false)
}

async fn options(State(state): State<AppState>) -> Json<Value> {
    let Some(db) = state.db.clone() else {
        return Json(json!({"available":false,"reason":"数据库未就绪"}));
    };
    let conn = db.conn.lock().expect("db");
    let cfg = backup_config(&conn);
    let groups: Vec<&str> = SETTINGS_GROUPS.to_vec();
    let tables: Vec<&str> = BACKUP_TABLES.to_vec();
    let (auto_daily, last_run_ts) = (
        cfg["auto_daily"].as_bool().unwrap_or(true),
        cfg["last_run_ts"].as_i64().unwrap_or(0),
    );
    let include_logs = cfg["include_logs"].as_bool().unwrap_or(false);
    let files = list_files(&db, include_logs);
    drop(conn);
    let (file_count, bytes) = usage(&db);
    Json(json!({
        "settings_groups": groups,
        "tables": tables,
        "files": files,
        "default_tables": DEFAULT_TABLES,
        "usage": {"files": file_count, "bytes": bytes},
        "auto": {
            "enabled": auto_daily,
            "last_run_ts": last_run_ts,
            "interval_sec": AUTO_INTERVAL_SEC,
            "next_run_ts": next_run_ts(last_run_ts),
        },
        "available": true,
    }))
}

async fn jobs(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let db = storage(&state)?;
    let conn = db.conn.lock().expect("db");
    let mut stmt = conn
        .prepare("SELECT id,ts,name,scope,file_path,size,sha256,encrypted,status,message,created_by FROM backup_jobs ORDER BY ts DESC LIMIT 100")
        .map_err(|error| bad(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            let scope: Option<String> = row.get(3)?;
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                scope,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, bool>(7).unwrap_or(false),
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
            ))
        })
        .map_err(|error| bad(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let mut items = Vec::new();
    for row in rows.flatten() {
        let (
            id,
            ts,
            name,
            scope_raw,
            file_path,
            size,
            sha256,
            encrypted,
            status,
            message,
            created_by,
        ) = row;
        let scope: Value = scope_raw
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or(Value::Null);
        let path = PathBuf::from(&file_path);
        let exists = path.is_file();
        let sha256_ok = exists.then(|| sha256_file(&path).ok() == Some(sha256.clone()));
        items.push(json!({
            "id": id,
            "ts": ts,
            "name": name,
            "scope": scope,
            "size": size,
            "sha256": sha256,
            "encrypted": encrypted,
            "status": status,
            "message": message,
            "created_by": created_by,
            "exists": exists,
            "file_path": file_path,
            "sha256_ok": sha256_ok,
            "progress": scope["progress"].as_i64().unwrap_or(if status == "ok" { 100 } else { 0 }),
            "duration_ms": scope["duration_ms"].as_i64().unwrap_or(0),
            "error": scope["error"].as_str().unwrap_or(""),
        }));
    }
    let total: i64 = items
        .iter()
        .map(|item| item["size"].as_i64().unwrap_or(0))
        .sum();
    Ok(Json(
        json!({"items":items,"usage":{"files":items.len(),"bytes":total},"available":true}),
    ))
}

async fn create(
    State(state): State<AppState>,
    Json(mut input): Json<CreateIn>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // 外部请求不能自称自动备份：任务来源只由内部调度器决定。
    input.created_by = None;
    create_backup(&state, input).map(Json)
}

fn create_backup(state: &AppState, input: CreateIn) -> Result<Value, (StatusCode, Json<Value>)> {
    create_backup_inner(state, input)
}

fn create_backup_inner(
    state: &AppState,
    input: CreateIn,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let db = storage(state)?;
    let conn = db.conn.lock().expect("db");
    let cfg = backup_config(&conn);
    let encrypt = input
        .encrypt
        .unwrap_or_else(|| cfg["encrypt"].as_bool().unwrap_or(true));
    // 任务来源：默认手工，只有内部调度器会传 auto。
    let created_by = input.created_by.as_deref().unwrap_or("manual").to_string();
    let groups: Vec<String> = input
        .settings_groups
        .unwrap_or_else(|| {
            SETTINGS_GROUPS
                .iter()
                .map(|value| value.to_string())
                .collect()
        })
        .into_iter()
        .filter(|group| SETTINGS_GROUPS.contains(&group.as_str()))
        .collect();
    let tables: Vec<String> = input
        .tables
        .unwrap_or_else(|| {
            DEFAULT_TABLES
                .iter()
                .map(|value| value.to_string())
                .collect()
        })
        .into_iter()
        .filter(|table| BACKUP_TABLES.contains(&table.as_str()))
        .collect();
    let files: Vec<String> = input.files.unwrap_or_default();
    let started = crate::now_millis();
    let ts = started / 1000;
    drop(conn);

    let label = safe_label(&input.name, ts);
    let mut entries = Vec::new();
    let mut row_counts = Map::new();
    let conn = db.conn.lock().expect("db");
    for group in &groups {
        let value = setting(&conn, group);
        entries.push(Entry {
            name: format!("settings/{group}.json"),
            data: serde_json::to_vec_pretty(&value).unwrap_or_else(|_| b"{}".to_vec()),
        });
    }
    for table in &tables {
        let rows = table_rows(&conn, table)?;
        row_counts.insert(table.clone(), json!(rows.len()));
        entries.push(Entry {
            name: format!("tables/{table}.json"),
            data: serde_json::to_vec(&rows).unwrap_or_else(|_| b"[]".to_vec()),
        });
    }
    drop(conn);
    let home = home_dir(&db);
    for rel in &files {
        let Some(path) = safe_join(&home, rel) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        let Ok(data) = std::fs::read(&path) else {
            continue;
        };
        entries.push(Entry {
            name: format!("files/{rel}"),
            data,
        });
    }
    let manifest = json!({
        "version": 1,
        "created_at": ts,
        "created_by": created_by,
        "settings_groups": groups,
        "tables": tables,
        "files": files,
        "encrypted": encrypt,
        "row_counts": row_counts,
    });
    entries.push(Entry {
        name: "manifest.json".into(),
        data: serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    });
    let zip =
        archive::build(&entries).map_err(|error| bad(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let (blob, suffix) = if encrypt {
        let token = encrypt_bytes(state, &zip)?;
        (token.into_bytes(), ".zip.enc")
    } else {
        (zip, ".zip")
    };
    let digest = hex::encode(Sha256::digest(&blob));
    let dir = backups_dir(&db);
    std::fs::create_dir_all(&dir).map_err(|error| {
        bad(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("创建备份目录失败: {error}"),
        )
    })?;
    let filename = format!("nanoquant-{label}-{ts}{suffix}");
    let path = dir.join(&filename);
    if let Err(error) = std::fs::write(&path, &blob) {
        let _ = std::fs::remove_file(&path);
        return Err(bad(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("写入备份失败: {error}"),
        ));
    }
    let mirror = mirror_backup(&path, &blob);
    let finished = crate::now_millis();
    let scope = json!({
        "manifest": manifest,
        "progress": 100,
        "started_at": started,
        "finished_at": finished,
        "duration_ms": finished - started,
        "mirror": mirror,
    });
    let job_id = insert_job(
        &db,
        ts,
        &label,
        &scope,
        &path,
        blob.len() as i64,
        &digest,
        encrypt,
        "ok",
        &format!("配置组 {} 个, 表 {} 张", groups.len(), tables.len()),
        &created_by,
    )?;
    apply_retention(&db, cfg["retention"].as_u64().unwrap_or(10) as usize);
    state.events.record(
        "info",
        "backup",
        &format!("备份完成 #{job_id} ({:.1} KB)", blob.len() as f64 / 1024.0),
        json!({"job_id": job_id, "size": blob.len(), "encrypted": encrypt}),
    );
    Ok(json!({
        "ok": true,
        "job_id": job_id,
        "path": path.to_string_lossy(),
        "mirror": mirror,
        "size": blob.len(),
        "sha256": digest,
        "encrypted": encrypt,
        "manifest": manifest,
    }))
}

async fn upload(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let db = storage(&state)?;
    let filename = query
        .get("filename")
        .cloned()
        .unwrap_or_else(|| "upload.zip".to_string());
    if body.is_empty() {
        return Err(bad(StatusCode::BAD_REQUEST, "空文件"));
    }
    if body.len() > MAX_UPLOAD_BYTES {
        return Err(bad(StatusCode::BAD_REQUEST, "文件过大（上限 100 MB）"));
    }
    let data = body.to_vec();
    let (entries, encrypted) = match archive::read(&data) {
        Ok(entries) => (entries, false),
        Err(_) => {
            let plain = decrypt_bytes(&state, &data)
                .map_err(|error| bad(StatusCode::BAD_REQUEST, format!("无法识别为有效备份文件：既不含 manifest.json，也无法用本机主密钥解密（{error}）")))?;
            let entries = archive::read(&plain).map_err(|error| {
                bad(
                    StatusCode::BAD_REQUEST,
                    format!("解密后仍不是有效备份包: {error}"),
                )
            })?;
            (entries, true)
        }
    };
    let manifest = entries
        .iter()
        .find(|entry| entry.name == "manifest.json")
        .ok_or_else(|| bad(StatusCode::BAD_REQUEST, "备份包 manifest 缺失"))?;
    let manifest: Value = serde_json::from_slice(&manifest.data).map_err(|error| {
        bad(
            StatusCode::BAD_REQUEST,
            format!("manifest 无法解析: {error}"),
        )
    })?;
    if manifest.get("version").is_none() {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "备份包 manifest 缺少 version 字段，可能是损坏或被改动的文件",
        ));
    }
    let ts = crate::now();
    let safe_name = safe_label(&filename, ts);
    let suffix = if encrypted { ".zip.enc" } else { ".zip" };
    let dir = backups_dir(&db);
    std::fs::create_dir_all(&dir).map_err(|error| {
        bad(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("创建备份目录失败: {error}"),
        )
    })?;
    let path = dir.join(format!("upload-{safe_name}-{ts}{suffix}"));
    std::fs::write(&path, &data).map_err(|error| {
        bad(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("上传落盘失败: {error}"),
        )
    })?;
    let digest = hex::encode(Sha256::digest(&data));
    let scope = json!({
        "manifest": manifest,
        "progress": 100,
        "duration_ms": 0,
        "uploaded_filename": filename,
    });
    let job_id = insert_job(
        &db,
        ts,
        &safe_name,
        &scope,
        &path,
        data.len() as i64,
        &digest,
        encrypted,
        "ok",
        &format!("上传 {filename}"),
        "manual",
    )?;
    let retention = db
        .conn
        .lock()
        .ok()
        .map(|conn| backup_config(&conn)["retention"].as_u64().unwrap_or(10) as usize)
        .unwrap_or(10);
    apply_retention(&db, retention);
    state.events.record(
        "info",
        "backup",
        &format!("上传备份 #{job_id} ({filename})"),
        json!({"job_id": job_id, "size": data.len(), "encrypted": encrypted}),
    );
    Ok(Json(json!({
        "ok": true,
        "job_id": job_id,
        "name": safe_name,
        "size": data.len(),
        "sha256": digest,
        "encrypted": encrypted,
        "manifest": manifest,
    })))
}

async fn restore(
    State(state): State<AppState>,
    axum::extract::Path(job_id): axum::extract::Path<i64>,
    Json(input): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    restore_inner(&state, job_id, &input).map(Json)
}

fn restore_inner(
    state: &AppState,
    job_id: i64,
    input: &Value,
) -> Result<Value, (StatusCode, Json<Value>)> {
    let db = storage(state)?;
    if state.engine_control.is_running() && engine_enabled(&db) {
        return Err(bad(StatusCode::CONFLICT, "引擎运行中，拒绝还原"));
    }
    let job = load_job(&db, job_id)?;
    let path = PathBuf::from(&job.file_path);
    if !path.is_file() {
        return Err(bad(StatusCode::NOT_FOUND, "备份文件已丢失"));
    }
    let raw = std::fs::read(&path).map_err(|error| {
        bad(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("读取备份失败: {error}"),
        )
    })?;
    let disk_digest = hex::encode(Sha256::digest(&raw));
    let plain = if job.encrypted {
        decrypt_bytes(state, &raw)
            .map_err(|error| bad(StatusCode::BAD_REQUEST, format!("解密失败: {error}")))?
    } else {
        raw
    };
    let plain_digest = hex::encode(Sha256::digest(&plain));
    if !job.sha256.is_empty() && disk_digest != job.sha256 && plain_digest != job.sha256 {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "备份文件校验失败（sha256 不符，可能被截断或改动）",
        ));
    }
    // 还原前自保：快照失败就中止，不能在没有回退点的情况下覆盖当前数据。
    let pre = create_backup_inner(
        state,
        CreateIn {
            name: format!("pre-restore-{job_id}"),
            settings_groups: None,
            tables: None,
            files: None,
            encrypt: Some(job.encrypted),
            created_by: None,
        },
    )
    .map_err(|error| {
        bad(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "还原前快照失败，已中止还原: {}",
                error.1 .0["detail"].as_str().unwrap_or("")
            ),
        )
    })?;
    let entries = archive::read(&plain)
        .map_err(|error| bad(StatusCode::BAD_REQUEST, format!("备份包损坏: {error}")))?;
    let manifest: Value = entries
        .iter()
        .find(|entry| entry.name == "manifest.json")
        .and_then(|entry| serde_json::from_slice(&entry.data).ok())
        .unwrap_or(Value::Null);
    let restore_settings = input["restore_settings"].as_bool().unwrap_or(true);
    let restore_tables = input["restore_tables"].as_bool().unwrap_or(true);
    let mut restored = Map::new();
    let mut conn = db.conn.lock().expect("db");
    let tx = conn
        .transaction()
        .map_err(|error| bad(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let result = (|| -> Result<(), String> {
        if restore_settings {
            for group in manifest["settings_groups"].as_array().into_iter().flatten() {
                let Some(group) = group.as_str() else {
                    continue;
                };
                let Some(entry) = archive::find(&entries, &format!("settings/{group}.json")) else {
                    continue;
                };
                let value: Value = serde_json::from_slice(&entry.data)
                    .map_err(|error| format!("配置 {group} 无法解析: {error}"))?;
                tx.execute(
                    "INSERT INTO app_settings(key,value,updated_at) VALUES(?1,?2,?3) \
                     ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
                    rusqlite::params![group, serde_json::to_string(&value).map_err(|error| error.to_string())?, crate::now()],
                )
                .map_err(|error| format!("还原配置 {group} 失败: {error}"))?;
                restored.insert(format!("settings:{group}"), json!(1));
            }
        }
        if restore_tables {
            for table in manifest["tables"].as_array().into_iter().flatten() {
                let Some(table) = table.as_str() else {
                    continue;
                };
                if !BACKUP_TABLES.contains(&table) {
                    continue;
                }
                let Some(entry) = archive::find(&entries, &format!("tables/{table}.json")) else {
                    continue;
                };
                let rows: Vec<Value> = serde_json::from_slice(&entry.data)
                    .map_err(|error| format!("表 {table} 数据无法解析: {error}"))?;
                restore_table(&tx, table, &rows)?;
                restored.insert(table.to_string(), json!(rows.len()));
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        return Err(bad(StatusCode::INTERNAL_SERVER_ERROR, error));
    }
    tx.commit()
        .map_err(|error| bad(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    drop(conn);
    state.events.record(
        "warn",
        "backup",
        &format!("已从备份 #{job_id} 还原"),
        json!({"job_id": job_id, "restored": restored, "pre_restore_job_id": pre["job_id"]}),
    );
    Ok(json!({
        "ok": true,
        "restored": restored,
        "note": "已自动创建还原前备份",
        "pre_restore_job_id": pre["job_id"],
    }))
}

async fn download(
    State(state): State<AppState>,
    axum::extract::Path(job_id): axum::extract::Path<i64>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let db = storage(&state)?;
    let job = load_job(&db, job_id)?;
    let path = PathBuf::from(&job.file_path);
    let bytes = std::fs::read(&path).map_err(|_| bad(StatusCode::NOT_FOUND, "备份文件不存在"))?;
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("backup.zip");
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        bytes,
    )
        .into_response())
}

async fn delete_job(
    State(state): State<AppState>,
    axum::extract::Path(job_id): axum::extract::Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let db = storage(&state)?;
    let job = load_job(&db, job_id)?;
    let _ = std::fs::remove_file(&job.file_path);
    let conn = db.conn.lock().expect("db");
    conn.execute("DELETE FROM backup_jobs WHERE id=?1", [job_id])
        .map_err(|error| bad(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    drop(conn);
    state.events.record(
        "info",
        "backup",
        &format!("删除备份 #{job_id}"),
        json!({"job_id": job_id}),
    );
    Ok(Json(json!({"ok": true, "id": job_id})))
}

struct JobRow {
    file_path: String,
    sha256: String,
    encrypted: bool,
}

fn load_job(db: &Db, job_id: i64) -> Result<JobRow, (StatusCode, Json<Value>)> {
    let conn = db.conn.lock().expect("db");
    conn.query_row(
        "SELECT file_path, sha256, encrypted FROM backup_jobs WHERE id=?1",
        [job_id],
        |row| {
            Ok(JobRow {
                file_path: row.get(0)?,
                sha256: row.get(1)?,
                encrypted: row.get::<_, i64>(2).unwrap_or(0) != 0
                    || row.get::<_, bool>(2).unwrap_or(false),
            })
        },
    )
    .map_err(|_| bad(StatusCode::NOT_FOUND, "备份不存在"))
}

fn insert_job(
    db: &Db,
    ts: i64,
    name: &str,
    scope: &Value,
    path: &Path,
    size: i64,
    sha256: &str,
    encrypted: bool,
    status: &str,
    message: &str,
    created_by: &str,
) -> Result<i64, (StatusCode, Json<Value>)> {
    let conn = db.conn.lock().expect("db");
    conn.execute(
        "INSERT INTO backup_jobs(ts,name,scope,file_path,size,sha256,encrypted,status,message,created_by) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        rusqlite::params![
            ts,
            name,
            serde_json::to_string(scope).unwrap_or_default(),
            path.to_string_lossy(),
            size,
            sha256,
            encrypted as i64,
            status,
            message,
            created_by
        ],
    )
    .map_err(|error| bad(StatusCode::INTERNAL_SERVER_ERROR, format!("登记备份任务失败: {error}")))?;
    Ok(conn.last_insert_rowid())
}

fn apply_retention(db: &Db, retention: usize) {
    let conn = db.conn.lock().expect("db");
    let mut rows: Vec<(i64, String)> = conn
        .prepare("SELECT id,file_path FROM backup_jobs ORDER BY ts DESC")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok()
                .map(|iter| iter.flatten().collect())
        })
        .unwrap_or_default();
    if rows.len() <= retention {
        return;
    }
    let expired: Vec<(i64, String)> = rows.split_off(retention.max(1));
    for (id, path) in &expired {
        let _ = std::fs::remove_file(path);
        let _ = conn.execute("DELETE FROM backup_jobs WHERE id=?1", [id]);
    }
}

fn usage(db: &Db) -> (i64, i64) {
    let dir = backups_dir(db);
    let mut files = 0;
    let mut bytes = 0;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().is_file() {
                files += 1;
                bytes += entry.metadata().map(|meta| meta.len() as i64).unwrap_or(0);
            }
        }
    }
    (files, bytes)
}

fn list_files(db: &Db, include_logs: bool) -> Vec<String> {
    let home = home_dir(db);
    let mut roots = vec![data_dir(db)];
    if include_logs {
        roots.push(data_dir(db).join("logs"));
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        walk(&root, &home, &mut out, &mut seen, 0);
    }
    out.sort();
    out.truncate(MAX_LISTED_FILES);
    out
}

fn walk(dir: &Path, home: &Path, out: &mut Vec<String>, seen: &mut HashSet<String>, depth: usize) {
    if depth > 6 || out.len() >= MAX_LISTED_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, home, out, seen, depth + 1);
            continue;
        }
        if !path.is_file() {
            continue;
        }
        if path.components().any(|part| part.as_os_str() == "backups") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if name.starts_with(".master_key") {
            continue;
        }
        if matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("db" | "db-wal" | "db-shm" | "lock" | "heartbeat" | "tmp")
        ) {
            continue;
        }
        if entry
            .metadata()
            .map(|meta| meta.len() > MAX_FILE_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(relative) = path.strip_prefix(home) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if seen.insert(relative.clone()) {
            out.push(relative);
        }
        if out.len() >= MAX_LISTED_FILES {
            return;
        }
    }
}

fn safe_join(home: &Path, relative: &str) -> Option<PathBuf> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    Some(home.join(relative_path))
}

fn safe_label(label: &str, ts: i64) -> String {
    let trimmed: String = label
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || *character == '-' || *character == '_'
        })
        .take(60)
        .collect();
    if trimmed.is_empty() {
        format!("backup-{ts}")
    } else {
        trimmed
    }
}

fn table_rows(conn: &Connection, table: &str) -> Result<Vec<Value>, (StatusCode, Json<Value>)> {
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM \"{table}\""))
        .map_err(|error| bad(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let columns: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| name.to_string())
        .collect();
    let rows = stmt
        .query_map([], |row| {
            let mut item = Map::new();
            for (index, name) in columns.iter().enumerate() {
                let value = match row.get_ref(index) {
                    Ok(rusqlite::types::ValueRef::Null) => Value::Null,
                    Ok(rusqlite::types::ValueRef::Integer(value)) => json!(value),
                    Ok(rusqlite::types::ValueRef::Real(value)) => json!(value),
                    Ok(rusqlite::types::ValueRef::Text(value)) => {
                        json!(String::from_utf8_lossy(value))
                    }
                    Ok(rusqlite::types::ValueRef::Blob(value)) => json!(hex::encode(value)),
                    Err(error) => return Err(error),
                };
                item.insert(name.clone(), value);
            }
            Ok(Value::Object(item))
        })
        .map_err(|error| bad(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    Ok(rows.flatten().collect())
}

fn restore_table(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    rows: &[Value],
) -> Result<(), String> {
    let columns: Vec<String> = {
        let mut stmt = tx
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))
            .map_err(|error| error.to_string())?;
        let iter = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| error.to_string())?;
        let mut columns = Vec::new();
        for column in iter {
            columns.push(column.map_err(|error| error.to_string())?);
        }
        columns
    };
    if columns.is_empty() {
        return Err(format!("表 {table} 不存在"));
    }
    tx.execute(&format!("DELETE FROM \"{table}\""), [])
        .map_err(|error| format!("清空表 {table} 失败: {error}"))?;
    if rows.is_empty() {
        return Ok(());
    }
    let placeholders = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "INSERT INTO \"{table}\" ({}) VALUES ({placeholders})",
        columns
            .iter()
            .map(|column| format!("\"{column}\""))
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut stmt = tx.prepare(&sql).map_err(|error| error.to_string())?;
    for row in rows {
        let values: Vec<SqlValue> = columns
            .iter()
            .map(|column| json_to_sql(row.get(column).unwrap_or(&Value::Null)))
            .collect();
        stmt.execute(rusqlite::params_from_iter(values))
            .map_err(|error| format!("还原表 {table} 失败: {error}"))?;
    }
    Ok(())
}

fn json_to_sql(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(flag) => SqlValue::Integer(i64::from(*flag)),
        Value::Number(number) => number
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| number.as_f64().map(SqlValue::Real))
            .unwrap_or(SqlValue::Null),
        Value::String(text) => SqlValue::Text(text.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

fn fernet_key(state: &AppState) -> Result<String, (StatusCode, Json<Value>)> {
    let key = state
        .auth
        .security
        .lock()
        .map_err(|_| bad(StatusCode::INTERNAL_SERVER_ERROR, "主密钥锁不可用"))?
        .master_key
        .clone();
    // AuthState 里既可能是 32 字节原文，也可能是 base64 文本（与 secret.rs 同口径）。
    if let Ok(text) = std::str::from_utf8(&key) {
        let trimmed = text.trim();
        if base64::engine::general_purpose::URL_SAFE
            .decode(trimmed)
            .map(|bytes| bytes.len() == 32)
            .unwrap_or(false)
        {
            return Ok(trimmed.to_string());
        }
    }
    if key.len() == 32 {
        return Ok(base64::engine::general_purpose::URL_SAFE.encode(key));
    }
    Err(bad(
        StatusCode::INTERNAL_SERVER_ERROR,
        "主密钥既不是 32 字节原文也不是有效 Fernet 密钥",
    ))
}

fn fernet(state: &AppState) -> Result<fernet::Fernet, (StatusCode, Json<Value>)> {
    let key = fernet_key(state)?;
    fernet::Fernet::new(&key)
        .ok_or_else(|| bad(StatusCode::INTERNAL_SERVER_ERROR, "Fernet 主密钥格式无效"))
}

fn encrypt_bytes(state: &AppState, data: &[u8]) -> Result<String, (StatusCode, Json<Value>)> {
    Ok(fernet(state)?.encrypt(data))
}

fn decrypt_bytes(state: &AppState, data: &[u8]) -> Result<Vec<u8>, String> {
    let token = std::str::from_utf8(data).map_err(|_| "密文不是 UTF-8 文本".to_string())?;
    fernet(state)
        .map_err(|error| {
            error.1 .0["detail"]
                .as_str()
                .unwrap_or("主密钥不可用")
                .to_string()
        })?
        .decrypt(token)
        .map_err(|error| format!("解密失败: {error}"))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let data = std::fs::read(path).map_err(|error| error.to_string())?;
    Ok(hex::encode(Sha256::digest(&data)))
}

fn mirror_backup(path: &Path, blob: &[u8]) -> Option<String> {
    let target = std::env::var("NANOQUANT_BACKUP_MIRROR").ok()?;
    if target.trim().is_empty() {
        return None;
    }
    let directory = Path::new(target.trim());
    let result = (|| -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(directory)?;
        let destination = directory.join(path.file_name().unwrap_or_default());
        std::fs::write(&destination, blob)?;
        Ok(destination)
    })();
    match result {
        Ok(path) => Some(path.to_string_lossy().into_owned()),
        Err(error) => {
            tracing::warn!(%error, "备份镜像写入失败（本地备份仍然有效）");
            None
        }
    }
}

/// 读取并校验主密钥（32 字节，base64 URL-safe 文本）。
fn read_master_key(key_file: &Path) -> Result<Vec<u8>, String> {
    let raw_key = std::fs::read_to_string(key_file)
        .map_err(|error| format!("读取主密钥 {} 失败: {error}", key_file.display()))?;
    let master_key = base64::engine::general_purpose::URL_SAFE
        .decode(raw_key.trim())
        .map_err(|_| "主密钥不是有效的 base64 文本".to_string())?;
    if master_key.len() != 32 {
        return Err("主密钥长度不是 32 字节".to_string());
    }
    Ok(master_key)
}

/// 直接创建一次备份包（不经过 HTTP），供安装/更新脚本在无 legacy 内核（已退役） 环境使用。
///
/// 与 `POST /backup/create` 共用 `create_backup_inner`：同样的配置组、表清单、
/// SHA-256 校验与加密规则；主密钥永不入包。
pub fn create_standalone(
    db_path: &str,
    key_file: &Path,
    name: &str,
    encrypt: bool,
) -> Result<Value, String> {
    if !Path::new(db_path).is_file() {
        return Err(format!("数据库不存在: {db_path}"));
    }
    let master_key = read_master_key(key_file)?;
    let db = Db::open(db_path).map_err(|error| format!("打开数据库失败: {error}"))?;
    let state = drill_state(db, master_key);
    create_backup_inner(
        &state,
        CreateIn {
            name: name.to_string(),
            settings_groups: None,
            tables: None,
            files: None,
            encrypt: Some(encrypt),
            created_by: None,
        },
    )
    .map_err(status_detail)
}

/// 备份演练：在数据库副本上执行「创建 → 校验 → 篡改检测 → 还原 → 失败回滚」。
///
/// 只在副本上操作，不碰生产库；`keep=false` 时演练目录在结束时删除。
pub fn drill(db_path: &str, key_file: &Path, keep: bool) -> Result<Value, String> {
    if !Path::new(db_path).is_file() {
        return Err(format!("数据库不存在: {db_path}"));
    }
    let master_key = read_master_key(key_file)?;
    let root = std::env::temp_dir().join(format!("nanoquant-backup-drill-{}", crate::now_millis()));
    let data = root.join("data");
    std::fs::create_dir_all(&data).map_err(|error| format!("创建演练目录失败: {error}"))?;
    let temp_db = data.join("nanoquant.db");
    let temp_db_text = temp_db.to_string_lossy().into_owned();
    let snapshot = (|| -> Result<(), String> {
        let source = Connection::open(db_path).map_err(|error| format!("打开源库失败: {error}"))?;
        source
            .execute("VACUUM INTO ?1", [&temp_db_text])
            .map_err(|error| format!("复制数据库副本失败: {error}"))?;
        Ok(())
    })();
    if let Err(error) = snapshot {
        let _ = std::fs::remove_dir_all(&root);
        return Err(error);
    }
    let db = Db::open(&temp_db_text).map_err(|error| format!("打开副本失败: {error}"))?;
    let state = drill_state(db.clone(), master_key);
    let mut checks: Vec<Value> = Vec::new();
    checks.push(json!({"name":"数据库副本","ok":true,"detail":temp_db_text}));

    let marker = format!("drill-{}", crate::now_millis());
    if let Err(error) = write_marker(&db, &marker) {
        let _ = std::fs::remove_dir_all(&root);
        return Err(error);
    }
    let created = match create_backup_inner(
        &state,
        CreateIn {
            name: "drill".to_string(),
            settings_groups: Some(Vec::new()),
            tables: Some(vec!["app_settings".to_string()]),
            files: None,
            encrypt: Some(true),
            created_by: None,
        },
    ) {
        Ok(value) => value,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&root);
            return Err(status_detail(error));
        }
    };
    let job_id = created["job_id"].as_i64().unwrap_or(0);
    checks.push(json!({
        "name": "备份创建",
        "ok": job_id > 0,
        "detail": format!("job_id={job_id} size={}", created["size"]),
    }));

    let path = PathBuf::from(created["path"].as_str().unwrap_or_default());
    let expected = created["sha256"].as_str().unwrap_or("").to_string();
    let blob = std::fs::read(&path).map_err(|error| format!("读取备份包失败: {error}"))?;
    let digest = hex::encode(Sha256::digest(&blob));
    checks.push(json!({
        "name": "SHA-256 校验",
        "ok": !expected.is_empty() && digest == expected,
        "detail": digest,
    }));

    let mut tampered = blob.clone();
    if let Some(byte) = tampered.first_mut() {
        *byte ^= 0x01;
    }
    let tampered_digest = hex::encode(Sha256::digest(&tampered));
    checks.push(json!({
        "name": "篡改检测",
        "ok": tampered_digest != expected,
        "detail": "翻转首字节后哈希必须变化",
    }));

    let plain = decrypt_bytes(&state, &blob).map_err(|error| format!("解密演练包失败: {error}"))?;
    let entries = archive::read(&plain).map_err(|error| format!("演练包结构损坏: {error}"))?;
    let has_master = entries
        .iter()
        .any(|entry| entry.name.contains(".master_key"));
    checks.push(json!({
        "name": "包内无主密钥",
        "ok": !has_master && entries.iter().any(|entry| entry.name == "manifest.json"),
        "detail": format!("{} 个条目", entries.len()),
    }));

    let _ = write_marker(&db, "changed-after-backup");
    let restored = match restore_inner(
        &state,
        job_id,
        &json!({"restore_settings": true, "restore_tables": true}),
    ) {
        Ok(value) => value,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&root);
            return Err(status_detail(error));
        }
    };
    let restored_marker = read_marker(&db).unwrap_or_default();
    checks.push(json!({
        "name": "还原",
        "ok": restored_marker == marker,
        "detail": format!("marker={restored_marker}"),
    }));

    std::fs::write(&path, b"corrupted-backup")
        .map_err(|error| format!("注入损坏备份失败: {error}"))?;
    let before = read_marker(&db).unwrap_or_default();
    let failed = restore_inner(
        &state,
        job_id,
        &json!({"restore_settings": true, "restore_tables": true}),
    );
    let rejected = matches!(failed, Err((StatusCode::BAD_REQUEST, _)));
    let unchanged = read_marker(&db).unwrap_or_default() == before;
    std::fs::write(&path, &blob).map_err(|error| format!("恢复演练包失败: {error}"))?;
    checks.push(json!({
        "name": "失败回滚",
        "ok": rejected && unchanged,
        "detail": if rejected { "校验失败已中止，库内值未变" } else { "损坏包未被拒绝" },
    }));

    let ok = checks.iter().all(|check| check["ok"] == true);
    let output = json!({
        "ok": ok,
        "checks": checks,
        "backup": {"job_id": job_id, "sha256": expected, "size": created["size"], "encrypted": true},
        "restore": {"job_id": job_id, "restored": restored["restored"]},
        "kept_dir": if keep { json!(root.to_string_lossy()) } else { json!(null) },
    });
    if !keep {
        let _ = std::fs::remove_dir_all(&root);
    }
    Ok(output)
}

fn status_detail(error: (StatusCode, Json<Value>)) -> String {
    error.1 .0["detail"]
        .as_str()
        .unwrap_or("备份演练失败")
        .to_string()
}

fn drill_state(db: Db, master_key: Vec<u8>) -> AppState {
    let auth = db.auth_state(master_key.clone(), "backup-drill".to_string());
    let mode_db = db.clone();
    let engine_control = crate::runtime::EngineControl::new(db.clone(), master_key);
    let events = crate::events::EventBus::new(Some(db.clone()));
    let klines = crate::kline_stream::KlineHub::new(
        std::sync::Arc::new(crate::kline_stream::BinanceWsSource),
        std::sync::Arc::new(move || {
            mode_db
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
                .unwrap_or_else(|| "demo".to_string())
        }),
    );
    AppState {
        started_at: crate::now(),
        auth,
        db: Some(db),
        engine_control,
        events,
        klines,
    }
}

fn write_marker(db: &Db, value: &str) -> Result<(), String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    conn.execute(
        "INSERT INTO app_settings(key,value,updated_at) VALUES('backup_drill_marker',?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        rusqlite::params![json!({"value": value}).to_string(), crate::now()],
    )
    .map_err(|error| format!("写入演练标记失败: {error}"))?;
    Ok(())
}

fn read_marker(db: &Db) -> Option<String> {
    let conn = db.conn.lock().ok()?;
    let raw: String = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='backup_drill_marker'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    serde_json::from_str::<Value>(&raw)
        .ok()?
        .get("value")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drill_passes_on_a_copy_and_detects_tampering() {
        let dir = std::env::temp_dir().join(format!("nq-backup-drill-{}", crate::now_millis()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("nanoquant.db");
        let key_path = dir.join(".master_key");
        crate::bootstrap::initialize(&db_path, &key_path, Some("test-password")).unwrap();
        let output = drill(&db_path.to_string_lossy(), &key_path, false).unwrap();
        assert_eq!(output["ok"], true, "演练应全部通过: {output}");
        let names: Vec<&str> = output["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|check| check["name"].as_str())
            .collect();
        for expected in [
            "数据库副本",
            "备份创建",
            "SHA-256 校验",
            "篡改检测",
            "包内无主密钥",
            "还原",
            "失败回滚",
        ] {
            assert!(names.contains(&expected), "缺少检查项 {expected}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn options_reads_saved_auto_settings() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT);
            INSERT INTO app_settings VALUES('backup', '{"auto_daily":false,"last_run_ts":1000}');"#,
        )
        .unwrap();
        let db = Db {
            path: String::new(),
            conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        };
        let state = test_state(db);
        let Json(body) = options(State(state)).await;
        assert_eq!(body["auto"]["enabled"], false);
        assert_eq!(body["auto"]["next_run_ts"], 87400);
        assert_eq!(body["available"], true);
    }

    #[test]
    fn next_run_is_zero_until_the_first_backup_happens() {
        assert_eq!(next_run_ts(0), 0, "从未跑过不能显示成一个具体的未来时间");
        assert_eq!(next_run_ts(1_000), 1_000 + 24 * 3600);
    }

    #[test]
    fn a_failed_backup_retries_in_one_hour_not_immediately() {
        let now = 10_000_i64;
        let recorded = retry_anchor(now);
        assert!(
            now - recorded < 24 * 3600,
            "回拨后必须仍未到一天，否则下一轮又会触发"
        );
        assert_eq!(recorded + 24 * 3600 - now, 3600, "失败后恰好一小时再试");
    }

    #[test]
    fn auto_backup_runs_once_a_day_and_records_the_anchor() {
        let dir = std::env::temp_dir().join(format!("nq-auto-backup-{}", crate::now_millis()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("nanoquant.db");
        crate::bootstrap::initialize(&db_path, &dir.join(".master_key"), Some("test-password"))
            .unwrap();
        let db = crate::db::Db::open(&db_path.to_string_lossy()).unwrap();
        let state = crate::test_state(Some(db));
        // 备份默认加密：测试自带的默认主密钥不是合法 Fernet 密钥，这里换成 32 字节原文。
        {
            let mut security = state.auth.security.lock().unwrap();
            security.master_key = vec![7u8; 32];
        }

        // 默认开启且从未执行过：到点应当立刻备份一次。
        let first = maybe_auto_backup(&state).expect("自动备份不应报错");
        assert!(first.is_some(), "到点后应当执行一次自动备份");
        let recorded = {
            let db = state.db.clone().unwrap();
            let conn = db.conn.lock().unwrap();
            let cfg = setting(&conn, "backup");
            cfg["last_run_ts"].as_i64().unwrap_or(0)
        };
        assert!(recorded > 0, "应当写回上次执行时间");

        // 紧接着再调一次：未到期，应当跳过。
        let second = maybe_auto_backup(&state).expect("自动备份不应报错");
        assert!(second.is_none(), "一天内不应重复备份");

        // 关掉自动备份后不应再执行。
        {
            let db = state.db.clone().unwrap();
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE app_settings SET value = ?1 WHERE key = 'backup'",
                [json!({"auto_daily": false, "last_run_ts": 0}).to_string()],
            )
            .unwrap();
        }
        let third = maybe_auto_backup(&state).expect("自动备份不应报错");
        assert!(third.is_none(), "关闭自动备份后不应执行");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn safe_labels_and_paths_do_not_escape_the_home_directory() {
        assert_eq!(safe_label("../etc/passwd", 1), "etcpasswd");
        assert!(safe_join(Path::new("/home/nq"), "../secret").is_none());
        assert!(safe_join(Path::new("/home/nq"), "/etc/passwd").is_none());
        assert!(safe_join(Path::new("/home/nq"), "logs/app.log").is_some());
    }

    #[test]
    fn restore_rejects_tables_outside_the_allow_list() {
        // 只允许 BACKUP_TABLES 中的表，防止 manifest 被篡改后写任意表。
        assert!(BACKUP_TABLES.contains(&"app_settings"));
        assert!(!BACKUP_TABLES.contains(&"sqlite_master"));
    }

    #[tokio::test]
    async fn restore_aborts_when_the_pre_restore_snapshot_fails() {
        let root =
            std::env::temp_dir().join(format!("nanoquant_b4_snapshot_{}", crate::now_millis()));
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        // 把备份目录占成普通文件，快照落盘必然失败。
        std::fs::write(data.join("backups"), b"blocked").unwrap();
        let db_path = data.join("nanoquant.db").to_string_lossy().into_owned();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
             CREATE TABLE ai_providers(id INTEGER);
             CREATE TABLE trading_symbols(symbol TEXT);
             CREATE TABLE mantras(id INTEGER);
             CREATE TABLE trades(id INTEGER);
             CREATE TABLE positions(id INTEGER);
             CREATE TABLE backup_jobs(id INTEGER PRIMARY KEY, file_path TEXT, sha256 TEXT, encrypted INTEGER);
             INSERT INTO app_settings VALUES('engine','{\"enabled\":false}',0);",
        )
        .unwrap();
        let zip = archive::build(&[
            Entry {
                name: "manifest.json".into(),
                data: serde_json::to_vec(&json!({
                    "version": 1,
                    "settings_groups": ["engine"],
                    "tables": [],
                }))
                .unwrap(),
            },
            Entry {
                name: "settings/engine.json".into(),
                data: serde_json::to_vec(&json!({"enabled": true})).unwrap(),
            },
        ])
        .unwrap();
        let zip_path = root.join("existing.zip");
        std::fs::write(&zip_path, &zip).unwrap();
        let digest = hex::encode(Sha256::digest(&zip));
        conn.execute(
            "INSERT INTO backup_jobs(id,file_path,sha256,encrypted) VALUES(1,?1,?2,0)",
            rusqlite::params![zip_path.to_string_lossy(), digest],
        )
        .unwrap();
        let db = Db {
            path: db_path,
            conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        };
        let state = test_state(db.clone());
        let result = restore(
            State(state),
            axum::extract::Path(1),
            Json(json!({"restore_settings": true, "restore_tables": true})),
        )
        .await;
        assert!(result.is_err(), "快照失败必须中止还原");
        assert_eq!(result.unwrap_err().0, StatusCode::INTERNAL_SERVER_ERROR);
        let conn = db.conn.lock().unwrap();
        let engine: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='engine'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            engine.contains("\"enabled\":false"),
            "快照失败后库内配置不能被改动"
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn test_state(db: Db) -> AppState {
        let auth = db.auth_state(vec![0; 32], "salt".into());
        let bus = crate::events::EventBus::new(Some(db.clone()));
        let control = crate::runtime::EngineControl::new(db.clone(), vec![0; 32]);
        let klines = crate::kline_stream::KlineHub::new(
            std::sync::Arc::new(crate::kline_stream::BinanceWsSource),
            std::sync::Arc::new(|| "demo".to_string()),
        );
        AppState {
            started_at: 0,
            auth,
            db: Some(db),
            engine_control: control,
            events: bus,
            klines,
        }
    }
}
