//! 临时运维工具：重置指定库的管理员密码（仅用于迁移期演练副本）。
use pbkdf2::pbkdf2_hmac;
use rusqlite::Connection;
use sha2::Sha256;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("用法: reset_admin_password <db> <password>");
        std::process::exit(2);
    }
    let conn = Connection::open(&args[1]).expect("open db");
    let raw: String = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='security'",
            [],
            |r| r.get(0),
        )
        .expect("security row");
    let mut value: serde_json::Value = serde_json::from_str(&raw).expect("security json");
    let salt = value["salt"]
        .as_str()
        .unwrap_or("nanoquant-static-salt-v1")
        .to_string();
    let mut out = [0u8; 32];
    pbkdf2_hmac::<Sha256>(args[2].as_bytes(), salt.as_bytes(), 120_000, &mut out);
    let hash = format!("pbkdf2_sha256$120000${salt}${}", hex::encode(out));
    value["password_hash"] = serde_json::Value::String(hash);
    value["password_is_default"] = serde_json::Value::Bool(false);
    conn.execute(
        "UPDATE app_settings SET value=?1 WHERE key='security'",
        [serde_json::to_string(&value).unwrap()],
    )
    .expect("update security");
    println!("password reset ok");
}
