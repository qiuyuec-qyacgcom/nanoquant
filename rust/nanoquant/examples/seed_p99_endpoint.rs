//! 迁移期临时工具：向演练库写入 mock 交易所端点与假凭证，供 `ops p99-real` 链路演练。
//!
//! 只写演练副本；不读取、不打印任何真实凭证。
use rusqlite::Connection;
use serde_json::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("用法: seed_p99_endpoint <db> <master_key_file> <base_url>");
        std::process::exit(2);
    }
    let key = std::fs::read_to_string(&args[2]).expect("read master key");
    let fernet = fernet::Fernet::new(key.trim()).expect("valid fernet key");
    let api_key = fernet.encrypt(b"mock-key");
    let api_secret = fernet.encrypt(b"mock-secret");
    let conn = Connection::open(&args[1]).expect("open db");
    let current: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'data'",
            [],
            |row| row.get(0),
        )
        .ok();
    let mut data: Value = current
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    data["demo_base_url"] = Value::String(args[3].clone());
    data["demo_api_key_enc"] = Value::String(api_key);
    data["demo_api_secret_enc"] = Value::String(api_secret);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();
    conn.execute(
        "INSERT OR REPLACE INTO app_settings(key, value, updated_at) VALUES('data', ?1, ?2)",
        rusqlite::params![
            serde_json::to_string(&data).expect("serialize settings"),
            now
        ],
    )
    .expect("write app_settings");
    println!("seeded demo endpoint {}", args[3]);
}
