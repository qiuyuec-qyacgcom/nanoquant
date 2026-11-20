use std::path::{Path, PathBuf};
use std::{env, fs};

#[allow(dead_code)]
pub struct Config {
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub db_path: String,
    pub log_dir: PathBuf,
    pub key_file: PathBuf,
    pub host: String,
    pub port: u16,
    pub admin_password: Option<String>,
    pub frontend: Option<PathBuf>,
    pub exec_socket: Option<String>,
    pub exec_secret: Option<String>,
    pub exec_tcp: Option<String>,
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
        let log_dir = env::var_os("NANOQUANT_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("logs"));
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
            log_dir,
            key_file,
            host,
            port,
            admin_password,
            frontend,
            exec_socket: env::var("EXEC_SOCKET").ok().filter(|v| !v.is_empty()),
            exec_secret: env::var("EXEC_SECRET").ok().filter(|v| !v.is_empty()),
            exec_tcp: env::var("EXEC_TCP").ok().filter(|v| !v.is_empty()),
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
}
