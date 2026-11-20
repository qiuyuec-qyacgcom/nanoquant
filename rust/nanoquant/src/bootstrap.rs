use std::path::Path;

use base64::Engine;
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

#[derive(Debug, Default)]
pub struct BootstrapReport {
    pub created_database: bool,
    pub created_master_key: bool,
    pub generated_password: Option<String>,
    pub tables: usize,
}

const SCHEMA_VERSION: &str = "1";
const DEFAULT_SETTINGS: &str = include_str!("default_settings.json");

pub fn initialize(
    db_path: &Path,
    master_key_path: &Path,
    admin_password: Option<&str>,
) -> Result<BootstrapReport, String> {
    let created_database = !db_path.is_file();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建数据目录 {} 失败: {e}", parent.display()))?;
    }
    let conn = Connection::open(db_path).map_err(|e| format!("打开数据库失败: {e}"))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .map_err(|e| format!("初始化数据库参数失败: {e}"))?;
    crate::schema::migrate(&conn)?;

    let defaults: serde_json::Map<String, Value> =
        serde_json::from_str(DEFAULT_SETTINGS).map_err(|e| format!("默认配置无法解析: {e}"))?;
    let now = crate::now();
    for (key, value) in &defaults {
        let raw = serde_json::to_string(value).map_err(|e| format!("序列化 {key} 失败: {e}"))?;
        conn.execute(
            "INSERT OR IGNORE INTO app_settings(key, value, updated_at) VALUES(?1, ?2, ?3)",
            params![key, raw, now],
        )
        .map_err(|e| format!("写入默认配置 {key} 失败: {e}"))?;
    }

    let existing_security = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='security'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| format!("读取 security 配置失败: {e}"))?;
    let mut security: Value = match existing_security {
        Some(raw) => {
            serde_json::from_str(&raw).map_err(|e| format!("security 配置不是合法 JSON: {e}"))?
        }
        None => defaults
            .get("security")
            .cloned()
            .unwrap_or_else(|| json!({})),
    };
    let has_password = security
        .get("password_hash")
        .and_then(|value| value.as_str())
        .map(|value| !value.is_empty())
        .unwrap_or(false);
    let mut generated_password = None;
    if !has_password {
        let (password, is_default) = match admin_password {
            Some(value) if !value.trim().is_empty() => (value.to_string(), false),
            _ => {
                let value = random_password();
                generated_password = Some(value.clone());
                (value, true)
            }
        };
        let salt = random_hex(16);
        let hash = crate::auth::hash_password(&password, &salt);
        security["username"] = json!("admin");
        security["password_hash"] = json!(hash);
        security["password_is_default"] = json!(is_default);
        security["token_version"] = json!(0);
        if security.get("session_ttl_hours").is_none() {
            security["session_ttl_hours"] = json!(12);
        }
        let raw =
            serde_json::to_string(&security).map_err(|e| format!("序列化 security 失败: {e}"))?;
        conn.execute(
            "INSERT INTO app_settings(key, value, updated_at) VALUES('security', ?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            params![raw, now],
        )
        .map_err(|e| format!("写入 security 配置失败: {e}"))?;
    }

    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES('schema_version', ?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![SCHEMA_VERSION, now],
    )
    .map_err(|e| format!("写入 schema_version 失败: {e}"))?;
    conn.execute(
        "INSERT OR IGNORE INTO engine_state(key, value, updated_at) VALUES('mode', 'demo', ?1)",
        [now],
    )
    .map_err(|e| format!("写入 engine_state.mode 失败: {e}"))?;
    conn.execute(
        "INSERT OR IGNORE INTO engine_state(key, value, updated_at) VALUES('enabled', '0', ?1)",
        [now],
    )
    .map_err(|e| format!("写入 engine_state.enabled 失败: {e}"))?;

    let created_master_key = if master_key_path.is_file() {
        validate_master_key(master_key_path)?;
        false
    } else {
        if let Some(parent) = master_key_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建密钥目录 {} 失败: {e}", parent.display()))?;
        }
        let mut bytes = [0u8; 32];
        rand::rng().fill(&mut bytes);
        let encoded = base64::engine::general_purpose::URL_SAFE.encode(bytes);
        std::fs::write(master_key_path, encoded)
            .map_err(|e| format!("写入主密钥 {} 失败: {e}", master_key_path.display()))?;
        true
    };

    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("统计表数量失败: {e}"))?;
    Ok(BootstrapReport {
        created_database,
        created_master_key,
        generated_password,
        tables: tables as usize,
    })
}

fn validate_master_key(path: &Path) -> Result<(), String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("无法读取主密钥 {}: {e}", path.display()))?;
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(raw.trim())
        .map_err(|e| format!("主密钥不是有效的 URL-safe Base64: {e}"))?;
    if decoded.len() != 32 {
        return Err(format!(
            "主密钥必须解码为 32 字节，实际 {} 字节: {}",
            decoded.len(),
            path.display()
        ));
    }
    Ok(())
}

fn random_password() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..16)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

fn random_hex(length: usize) -> String {
    let mut bytes = vec![0u8; length];
    rand::rng().fill(&mut bytes[..]);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use rusqlite::Connection;

    fn workspace(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nq-bootstrap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn table_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn security(conn: &Connection) -> serde_json::Value {
        let raw: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='security'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn empty_directory_creates_schema_admin_and_engine_state() {
        let dir = workspace("empty");
        let db = dir.join("nanoquant.db");
        let key = dir.join(".master_key");
        let report = initialize(&db, &key, Some("test-password")).unwrap();
        assert!(report.created_database);
        assert!(report.created_master_key);
        assert_eq!(report.tables, 23);
        assert_eq!(report.generated_password, None);

        let conn = Connection::open(&db).unwrap();
        assert_eq!(table_count(&conn), 23);
        let schema_version: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(schema_version, "1");
        let security = security(&conn);
        assert_eq!(security["username"], "admin");
        assert_eq!(security["password_is_default"], false);
        assert!(crate::auth::verify_password(
            "test-password",
            security["password_hash"].as_str().unwrap()
        ));
        let mode: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='mode'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mode, "demo");
        let raw = std::fs::read_to_string(&key).unwrap();
        let decoded = base64::engine::general_purpose::URL_SAFE
            .decode(raw.trim())
            .unwrap();
        assert_eq!(decoded.len(), 32);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn random_password_marks_change_required() {
        let dir = workspace("random");
        let db = dir.join("nanoquant.db");
        let key = dir.join(".master_key");
        let report = initialize(&db, &key, None).unwrap();
        let generated = report.generated_password.as_deref().unwrap();
        assert_eq!(generated.chars().count(), 16);
        let conn = Connection::open(&db).unwrap();
        let security = security(&conn);
        assert_eq!(security["password_is_default"], true);
        assert!(crate::auth::verify_password(
            generated,
            security["password_hash"].as_str().unwrap()
        ));
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_initialize_keeps_existing_admin_and_master_key() {
        let dir = workspace("second");
        let db = dir.join("nanoquant.db");
        let key = dir.join(".master_key");
        initialize(&db, &key, Some("first-password")).unwrap();
        let first_key = std::fs::read(&key).unwrap();
        let conn = Connection::open(&db).unwrap();
        let first_hash = security(&conn)["password_hash"]
            .as_str()
            .unwrap()
            .to_string();
        drop(conn);

        let report = initialize(&db, &key, Some("second-password")).unwrap();
        assert!(!report.created_database);
        assert!(!report.created_master_key);
        assert_eq!(report.tables, 23);
        assert_eq!(std::fs::read(&key).unwrap(), first_key);
        let conn = Connection::open(&db).unwrap();
        assert_eq!(
            security(&conn)["password_hash"].as_str().unwrap(),
            first_hash
        );
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
