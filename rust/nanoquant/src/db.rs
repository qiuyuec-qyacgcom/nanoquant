//! SQLite 访问。只打开现有主库，绝不建表，避免把历史数据旁路成一个空库。

use rusqlite::{Connection, OpenFlags};

/// 只读查询连接：文件库走独立连接，内存库或打开失败时回退主连接锁。
pub enum DbRead<'a> {
    Shared(std::sync::MutexGuard<'a, Connection>),
    Owned(Connection),
}

impl std::ops::Deref for DbRead<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        match self {
            DbRead::Shared(guard) => guard,
            DbRead::Owned(conn) => conn,
        }
    }
}

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

    /// 只读查询入口。文件库新开一个 `query_only` 连接，避免和写事务抢主连接锁；
    /// 内存库、文件不存在、打开失败或 schema 不匹配时回退主连接，绝不 panic。
    pub fn read(&self) -> Result<DbRead<'_>, String> {
        if self.path.is_empty()
            || self.path == ":memory:"
            || !std::path::Path::new(&self.path).is_file()
        {
            return self.shared();
        }
        let Ok(conn) = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) else {
            return self.shared();
        };
        if conn
            .execute_batch("PRAGMA query_only=ON; PRAGMA busy_timeout=5000;")
            .is_err()
        {
            return self.shared();
        }
        let has_settings = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='app_settings'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        if !has_settings {
            return self.shared();
        }
        Ok(DbRead::Owned(conn))
    }

    fn shared(&self) -> Result<DbRead<'_>, String> {
        self.conn
            .lock()
            .map(DbRead::Shared)
            .map_err(|_| "数据库锁不可用".to_string())
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

    fn seeded_file_db(name: &str) -> (std::path::PathBuf, Db) {
        let path = std::env::temp_dir().join(format!("nq-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER); \
             CREATE TABLE probe(id INTEGER PRIMARY KEY, label TEXT); \
             INSERT INTO app_settings VALUES('engine','{}',0); \
             INSERT INTO probe VALUES(1,'alpha');",
        )
        .unwrap();
        drop(conn);
        let db = Db::open(path.to_str().unwrap()).unwrap();
        (path, db)
    }

    #[test]
    fn read_uses_separate_connection_for_file_db() {
        let (path, db) = seeded_file_db("read-separate");
        let read = db.read().expect("文件库应能开独立只读连接");
        assert!(matches!(&read, DbRead::Owned(_)), "文件库应使用独立连接");
        let label: String = read
            .query_row("SELECT label FROM probe WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(label, "alpha");
        drop(read);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_falls_back_for_memory_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE probe(id INTEGER PRIMARY KEY, label TEXT); \
             INSERT INTO probe VALUES(1,'mem');",
        )
        .unwrap();
        let db = Db {
            path: String::new(),
            conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        };
        let read = db.read().expect("内存库应回退主连接");
        assert!(matches!(&read, DbRead::Shared(_)), "内存库应回退主连接");
        let label: String = read
            .query_row("SELECT label FROM probe WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(label, "mem");
    }

    #[test]
    fn read_connection_is_query_only() {
        let (path, db) = seeded_file_db("read-query-only");
        let read = db.read().expect("文件库应能开独立只读连接");
        assert!(matches!(&read, DbRead::Owned(_)), "文件库应使用独立连接");
        assert!(
            read.execute("INSERT INTO probe(id, label) VALUES(2, 'nope')", [])
                .is_err(),
            "只读连接必须拒绝写入"
        );
        drop(read);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_does_not_block_on_main_connection() {
        let (path, db) = seeded_file_db("read-not-block");
        let guard = db.conn.lock().unwrap();
        let read = db.read().expect("主连接持锁时仍应取出独立连接");
        assert!(matches!(&read, DbRead::Owned(_)), "文件库应使用独立连接");
        let label: String = read
            .query_row("SELECT label FROM probe WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(label, "alpha", "主连接持锁不应阻塞只读查询");
        drop(read);
        drop(guard);
        let _ = std::fs::remove_file(&path);
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
