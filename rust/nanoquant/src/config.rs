use std::path::{Path, PathBuf};
use std::{env, fs};

pub struct Config {
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub db_path: String,
    pub key_file: PathBuf,
    pub host: String,
    pub port: u16,
    pub admin_password: Option<String>,
    pub frontend: Option<PathBuf>,
}

impl Config {
    pub fn from_env(args: &[String]) -> Self {
        let home = env::var_os("NANOQUANT_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from("."))
            });
        let data_dir = env::var_os("NANOQUANT_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("data"));
        let db_path = env::var("NANOQUANT_DB")
            .unwrap_or_else(|_| data_dir.join("nanoquant.db").to_string_lossy().into_owned());
        let key_file = env::var_os("NANOQUANT_KEY_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join(".master_key"));
        let host = env::var("NANOQUANT_HOST").unwrap_or_else(|_| "0.0.0.0".into());
        let port = resolve_port(env::var("NANOQUANT_PORT").ok().as_deref(), &data_dir);
        let admin_password = admin_password_from_args(args).or_else(|| {
            env::var("NANOQUANT_ADMIN_PASSWORD")
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
        let frontend = env::var_os("NANOQUANT_FRONTEND")
            .map(PathBuf::from)
            .or_else(|| {
                let dist = home.join("frontend/dist");
                dist.is_dir().then_some(dist)
            });
        Self {
            home,
            data_dir,
            db_path,
            key_file,
            host,
            port,
            admin_password,
            frontend,
        }
    }
}

pub fn resolve_port(explicit: Option<&str>, data_dir: &Path) -> u16 {
    explicit
        .and_then(|value| value.trim().parse().ok())
        .or_else(|| {
            fs::read_to_string(data_dir.join(".port"))
                .ok()
                .and_then(|value| value.trim().parse().ok())
        })
        .unwrap_or(8899)
}

pub fn admin_password_from_args(args: &[String]) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == "--admin-password" || pair[0] == "--password")
        .map(|pair| pair[1].clone())
        .filter(|value| !value.trim().is_empty())
}

/// 日志级别白名单：`off` 也允许，用于关闭订阅。
fn normalize_log_level(value: &str) -> Option<String> {
    let level = value.trim().to_ascii_lowercase();
    matches!(
        level.as_str(),
        "trace" | "debug" | "info" | "warn" | "error" | "off"
    )
    .then_some(level)
}

fn default_log_level() -> Option<String> {
    let settings: serde_json::Value =
        serde_json::from_str(include_str!("default_settings.json")).ok()?;
    settings
        .get("system")?
        .get("log_level")?
        .as_str()
        .map(str::to_string)
}

fn database_log_level(db_path: &Path) -> Option<String> {
    use rusqlite::{Connection, OpenFlags};
    if !db_path.is_file() {
        return None;
    }
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let raw: String = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'system'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parsed.get("log_level")?.as_str().map(str::to_string)
}

/// 解析 tracing 级别：数据库 `system.log_level` 优先，其次编译期默认配置；
/// 缺键或非法值一律回退 `info`。
pub fn resolve_log_level(db_path: &Path) -> String {
    database_log_level(db_path)
        .as_deref()
        .and_then(normalize_log_level)
        .or_else(|| default_log_level().as_deref().and_then(normalize_log_level))
        .unwrap_or_else(|| "info".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nq-config-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn explicit_port_wins_over_port_file() {
        let dir = temp_dir("explicit");
        fs::write(dir.join(".port"), "9010").unwrap();
        assert_eq!(resolve_port(Some("9020"), &dir), 9020);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn port_file_is_used_when_env_is_missing() {
        let dir = temp_dir("file");
        fs::write(dir.join(".port"), "9010\n").unwrap();
        assert_eq!(resolve_port(None, &dir), 9010);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn port_defaults_to_8899() {
        let dir = temp_dir("default");
        assert_eq!(resolve_port(None, &dir), 8899);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn admin_password_argument_is_parsed() {
        let args = vec![
            "nanoquant".to_string(),
            "--admin-password".to_string(),
            "secret".to_string(),
        ];
        assert_eq!(admin_password_from_args(&args).as_deref(), Some("secret"));
        assert_eq!(admin_password_from_args(&["nanoquant".to_string()]), None);
    }

    #[test]
    fn log_level_defaults_to_info_when_database_is_missing() {
        let dir = temp_dir("log-level-missing");
        let path = dir.join("nq.db");
        assert_eq!(resolve_log_level(&path), "info");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_level_reads_system_group_and_rejects_invalid_values() {
        let dir = temp_dir("log-level-db");
        let path = dir.join("nq.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE app_settings(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO app_settings(key,value,updated_at) VALUES('system','{\"log_level\":\"debug\"}',0)",
                [],
            )
            .unwrap();
        }
        assert_eq!(resolve_log_level(&path), "debug");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE app_settings SET value='{\"log_level\":\"verbose\"}' WHERE key='system'",
                [],
            )
            .unwrap();
        }
        assert_eq!(resolve_log_level(&path), "info", "非法值必须回退 info");
        let _ = fs::remove_dir_all(&dir);
    }
}
