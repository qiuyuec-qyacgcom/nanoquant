//! SQLite 访问。只打开现有主库，绝不建表，避免把历史数据旁路成一个空库。

use rusqlite::Connection;

#[derive(Clone)]
pub struct Db {
    pub path: String,
    pub conn: std::sync::Arc<std::sync::Mutex<Connection>>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Db")
    }
}

#[derive(Debug)]
pub enum DbError {
    Missing(String),
    Schema(String),
    Sqlite(String),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Missing(p) => write!(f, "数据库不存在: {p}"),
            DbError::Schema(m) => write!(f, "数据库结构不符: {m}"),
            DbError::Sqlite(m) => write!(f, "数据库错误: {m}"),
        }
    }
}

impl Db {
    /// 打开已有库并校验 `app_settings` 表存在。路径不存在或表缺失都拒绝。
    pub fn open(path: &str) -> Result<Self, DbError> {
        if !std::path::Path::new(path).is_file() {
            return Err(DbError::Missing(path.to_string()));
        }
        let conn = Connection::open(path).map_err(|e| DbError::Sqlite(e.to_string()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let exists: bool = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='app_settings'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        if !exists {
            return Err(DbError::Schema(format!("{path} 缺少 app_settings 表")));
        }
        Ok(Self {
            path: path.to_string(),
            conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("db")
    }

    /// 用库里的 security 组构造鉴权状态；缺字段时回退到安全默认值。
    pub fn auth_state(&self, master_key: Vec<u8>, salt: String) -> crate::auth::AuthState {
        use std::sync::{Arc, Mutex};
        let group = self
            .setting("security")
            .ok()
            .flatten()
            .unwrap_or(serde_json::Value::Null);
        let str_field = |k: &str, d: &str| {
            group
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or(d)
                .to_string()
        };
        let security = crate::auth::Security {
            username: str_field("username", "admin"),
            password_hash: str_field("password_hash", ""),
            password_is_default: group
                .get("password_is_default")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            token_version: group
                .get("token_version")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            session_ttl_hours: group
                .get("session_ttl_hours")
                .and_then(|v| v.as_i64())
                .unwrap_or(12),
            master_key,
            salt,
        };
        let limit = group
            .get("login_fail_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(8) as usize;
        let window = group
            .get("login_fail_window_sec")
            .and_then(|v| v.as_i64())
            .unwrap_or(600);
        crate::auth::AuthState {
            security: Arc::new(Mutex::new(security)),
            throttle: Arc::new(Mutex::new(crate::auth::LoginThrottle::new(limit, window))),
            db: Some(self.clone()),
        }
    }

    /// 原子更新认证相关字段，保留 security 组中的其它配置。
    pub fn update_security_password(
        &self,
        password_hash: &str,
        password_is_default: bool,
        token_version: i64,
    ) -> Result<(), DbError> {
        let mut conn = self.lock();
        let tx = conn
            .transaction()
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let raw: String = tx
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'security'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let mut value: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| DbError::Schema(format!("security 配置不是合法 JSON: {e}")))?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| DbError::Schema("security 配置必须是 JSON 对象".into()))?;
        object.insert(
            "password_hash".into(),
            serde_json::Value::String(password_hash.into()),
        );
        object.insert(
            "password_is_default".into(),
            serde_json::Value::Bool(password_is_default),
        );
        object.insert(
            "token_version".into(),
            serde_json::Value::Number(token_version.into()),
        );
        tx.execute(
            "UPDATE app_settings SET value = ?1, updated_at = ?2 WHERE key = 'security'",
            rusqlite::params![
                serde_json::to_string(&value).map_err(|e| DbError::Schema(e.to_string()))?,
                crate::now()
            ],
        )
        .map_err(|e| DbError::Sqlite(e.to_string()))?;
        tx.commit().map_err(|e| DbError::Sqlite(e.to_string()))?;
        Ok(())
    }

    /// 读取一个配置组的 JSON 值；组不存在返回 None。
    pub fn setting(&self, group: &str) -> Result<Option<serde_json::Value>, DbError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT value FROM app_settings WHERE key = ?1")
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        let mut rows = stmt
            .query([group])
            .map_err(|e| DbError::Sqlite(e.to_string()))?;
        match rows.next().map_err(|e| DbError::Sqlite(e.to_string()))? {
            None => Ok(None),
            Some(row) => {
                let raw: String = row.get(0).map_err(|e| DbError::Sqlite(e.to_string()))?;
                let value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
                Ok(Some(value))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_missing_database_and_names_the_path() {
        let path = "this-database-does-not-exist.db";
        let err = Db::open(path).unwrap_err();
        assert!(err.to_string().contains(path), "错误信息应包含路径: {err}");
    }

    #[test]
    fn reads_security_group_from_real_database() {
        let path = format!("{}/../../data/nanoquant.db", env!("CARGO_MANIFEST_DIR"));
        let db = Db::open(&path).expect("现有主库应能打开");
        let value = db.setting("security").unwrap().expect("security 组应存在");
        assert!(value.get("username").is_some(), "security 组应含 username");
        assert!(value
            .get("password_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .starts_with("pbkdf2_sha256$"));
    }

    #[test]
    fn refuses_database_without_settings_table() {
        let path = std::env::temp_dir().join("nq-empty.db");
        let _ = std::fs::remove_file(&path);
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE other(id INTEGER);")
            .unwrap();
        let err = Db::open(path.to_str().unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("app_settings"),
            "应指出缺表: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
