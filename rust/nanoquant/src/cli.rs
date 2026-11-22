//! 运维 CLI：`nanoquant ops <command>`。
//!
//! 命令行只复用库代码与配置解析，**不启动 HTTP 监听**。每条命令输出可机读的
//! JSON（`--format json`）或逐行 PASS/FAIL，并以退出码表达结论：
//! 0 通过、1 业务失败、2 命令未实装/参数非法、3 运行时命中（zero-python）。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::Config;
use crate::db::Db;
use crate::interpreter_refs::{archive, exceptions, report, scanner};

pub const COMMANDS: &[(&str, &str)] = &[
    ("preflight", "环境只读预检：数据库、配置、凭证、磁盘、时钟"),
    ("reconcile", "回测与实盘对账（--self-test 使用内置样本）"),
    ("reset-password", "重置管理员密码 [--password <p>]"),
    (
        "backup-create",
        "创建备份包（更新前快照，默认加密；无 Python 可用）",
    ),
    ("backup-drill", "创建 → 校验 → 还原 → 失败回滚演练"),
    ("archive-replay", "回放归档 JSONL 到临时库"),
    ("cleanup-history", "清理历史事件/信号（默认 dry-run）"),
    ("final-verdict", "策略判定"),
    ("bench-exec", "执行侧压测"),
    ("p99-bench", "签名 p99 基准"),
    ("p99-real", "实盘链路口径 p99"),
    ("rtt-probe", "RTT 探针"),
    ("verify-exec-core", "执行内核签名逐字节比对"),
    (
        "show-exec-secret",
        "输出当前交易环境的交易所 Secret（部署执行内核用，只打到标准输出）",
    ),
    (
        "seed-symbols",
        "为既有数据库补默认标的池（默认 dry-run，--apply 才写入）",
    ),
    ("mock-exec-core", "模拟执行内核"),
    ("mock-exchange", "模拟交易所 /fapi/*"),
    ("verify-partial-tp", "部分止盈复核"),
    ("contract-diff", "契约对照：与 P0 基线逐端点比对"),
    (
        "zero-python",
        "零解释器门禁：仓库/运行时/镜像/文本/归档分层检索",
    ),
];

struct Options {
    command: String,
    json: bool,
    db_path: String,
    password: Option<String>,
    apply: bool,
    vacuum: bool,
    force: bool,
    self_test: bool,
    keep: bool,
    plain: bool,
    name: Option<String>,
    backtest: Option<String>,
    live: Option<String>,
    mode: Option<String>,
    file: Option<String>,
    manifest: Option<String>,
    scope: String,
    exceptions: Option<String>,
    archive_root: Option<String>,
    max_depth: Option<usize>,
    max_file_bytes: Option<u64>,
    max_hits: Option<usize>,
    changed_file_list: Option<String>,
    out: Option<String>,

    interval: Option<String>,
    symbols: Option<String>,
    bars: Option<i64>,
    folds: Option<i64>,
    train_bars: Option<i64>,
    symbol: Option<String>,
    orders: Option<i64>,
    delay_ms: Option<i64>,
    per_minute: Option<i64>,
    target: Option<String>,
    seconds: Option<i64>,
    port: Option<u16>,
    secret: Option<String>,
    baseline: Option<String>,
}

fn parse(argv: &[String], config: &Config) -> Options {
    let mut command = String::new();
    let mut json = false;
    let mut db_path = config.db_path.clone();
    let mut password = None;
    let mut apply = false;
    let mut vacuum = false;
    let mut force = false;
    let mut self_test = false;
    let mut keep = false;
    let mut plain = false;
    let mut name = None;
    let mut backtest = None;
    let mut live = None;
    let mut mode = None;
    let mut file = None;
    let mut manifest = None;
    let mut scope = "repo".to_string();

    let mut interval = None;
    let mut symbols = None;
    let mut bars = None;
    let mut folds = None;
    let mut train_bars = None;
    let mut symbol = None;
    let mut orders = None;
    let mut delay_ms = None;
    let mut per_minute = None;
    let mut target = None;
    let mut seconds = None;
    let mut port = None;
    let mut secret = None;
    let mut baseline = None;
    let mut exceptions = None;
    let mut archive_root = None;
    let mut max_depth = None;
    let mut max_file_bytes = None;
    let mut max_hits = None;
    let mut changed_file_list = None;
    let mut out = None;
    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        match arg.as_str() {
            "--format" => {
                index += 1;
                json = argv.get(index).map(String::as_str) == Some("json");
            }
            "--db" => {
                index += 1;
                if let Some(value) = argv.get(index) {
                    db_path = value.clone();
                }
            }
            "--password" => {
                index += 1;
                password = argv.get(index).cloned();
            }
            "--apply" => apply = true,
            "--vacuum" => vacuum = true,
            "--force" => force = true,
            "--self-test" => self_test = true,
            "--keep" => keep = true,
            "--plain" => plain = true,
            "--name" => {
                index += 1;
                name = argv.get(index).cloned();
            }
            "--backtest" => {
                index += 1;
                backtest = argv.get(index).cloned();
            }
            "--live" => {
                index += 1;
                live = argv.get(index).cloned();
            }
            "--mode" => {
                index += 1;
                mode = argv.get(index).cloned();
            }
            "--file" => {
                index += 1;
                file = argv.get(index).cloned();
            }
            "--manifest" => {
                index += 1;
                manifest = argv.get(index).cloned();
            }
            "--scope" => {
                index += 1;
                scope = argv
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| "repo".to_string());
            }
            "--exceptions" => {
                index += 1;
                exceptions = argv.get(index).cloned();
            }
            "--archive-root" => {
                index += 1;
                archive_root = argv.get(index).cloned();
            }
            "--max-depth" => {
                index += 1;
                max_depth = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--max-file-bytes" => {
                index += 1;
                max_file_bytes = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--max-hits" => {
                index += 1;
                max_hits = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--changed-file-list" => {
                index += 1;
                changed_file_list = argv.get(index).cloned();
            }
            "--out" => {
                index += 1;
                out = argv.get(index).cloned();
            }
            "--interval" => {
                index += 1;
                interval = argv.get(index).cloned();
            }
            "--symbols" => {
                index += 1;
                symbols = argv.get(index).cloned();
            }
            "--symbol" => {
                index += 1;
                symbol = argv.get(index).cloned();
            }
            "--bars" => {
                index += 1;
                bars = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--folds" => {
                index += 1;
                folds = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--train-bars" => {
                index += 1;
                train_bars = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--orders" => {
                index += 1;
                orders = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--delay-ms" => {
                index += 1;
                delay_ms = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--per-minute" => {
                index += 1;
                per_minute = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--target" => {
                index += 1;
                target = argv.get(index).cloned();
            }
            "--seconds" => {
                index += 1;
                seconds = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--port" => {
                index += 1;
                port = argv.get(index).and_then(|value| value.parse().ok());
            }
            "--secret" => {
                index += 1;
                secret = argv.get(index).cloned();
            }
            "--baseline" => {
                index += 1;
                baseline = argv.get(index).cloned();
            }
            other if other.starts_with("--") => {}
            other if command.is_empty() => command = other.to_string(),
            _ => {}
        }
        index += 1;
    }
    Options {
        command,
        json,
        db_path,
        password,
        apply,
        vacuum,
        force,
        self_test,
        keep,
        plain,
        name,
        backtest,
        live,
        mode,
        file,
        manifest,
        scope,
        interval,
        symbols,
        bars,
        folds,
        train_bars,
        symbol,
        orders,
        delay_ms,
        per_minute,
        target,
        seconds,
        port,
        secret,
        baseline,
        exceptions,
        archive_root,
        max_depth,
        max_file_bytes,
        max_hits,
        changed_file_list,
        out,
    }
}

pub fn run(argv: &[String], config: &Config) -> i32 {
    let argv = if argv.first().map(String::as_str) == Some("ops") {
        &argv[1..]
    } else {
        argv
    };
    let wants_help = argv
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg.is_empty());
    let options = parse(argv, config);
    if wants_help && options.command.is_empty() {
        print_help();
        return 0;
    }
    if options.command.is_empty() {
        print_help();
        return 2;
    }
    if !COMMANDS.iter().any(|(name, _)| *name == options.command) {
        eprintln!("未知命令: {}", options.command);
        print_help();
        return 2;
    }
    let result = match options.command.as_str() {
        "preflight" => preflight(&options),
        "reconcile" => reconcile(&options),
        "reset-password" => reset_password(&options),
        "cleanup-history" => cleanup_history(&options),
        "archive-replay" => archive_replay(&options),
        "backup-create" => backup_create(&options),
        "backup-drill" => backup_drill(&options),
        "final-verdict" => final_verdict(&options),
        "verify-partial-tp" => verify_partial_tp(&options),
        "p99-bench" => p99_bench(&options),
        "rtt-probe" => rtt_probe_command(&options),
        "bench-exec" => bench_exec_command(&options),
        "p99-real" => p99_real_command(&options),
        "mock-exchange" => mock_service(&options, true),
        "mock-exec-core" => mock_service(&options, false),
        "verify-exec-core" => verify_exec_core_command(&options),
        "show-exec-secret" => show_exec_secret(&options, config),
        "seed-symbols" => seed_symbols(&options),
        "contract-diff" => contract_diff_command(&options, config),
        "zero-python" => zero_python(&options, config),
        _ => Err((
            2,
            format!(
                "命令 {} 尚未在本阶段实装（禁止用固定成功响应通过验收）",
                options.command
            ),
        )),
    };
    match result {
        Ok((code, value)) => {
            if options.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).unwrap_or_default()
                );
            } else {
                print_human(&value);
            }
            code
        }
        Err((code, detail)) => {
            if options.json {
                println!("{}", json!({"ok": false, "error": detail}));
            } else {
                eprintln!("FAIL: {detail}");
            }
            code
        }
    }
}

fn print_help() {
    println!("NanoQuant 运维 CLI（不启动 HTTP）");
    println!();
    println!("用法: nanoquant ops <command> [options]");
    println!();
    println!("命令:");
    for (name, help) in COMMANDS {
        println!("  {name:<20} {help}");
    }
    println!();
    println!("通用选项: --format json  --db <path>  --help");
}

fn print_human(value: &Value) {
    // 密钥类命令要的是「纯文本一行」，脚本直接捕获就能用。
    if let Some(secret) = value.get("raw_secret").and_then(Value::as_str) {
        println!("{secret}");
        return;
    }
    if value.get("seeded").is_some() {
        if let Some(detail) = value.get("detail").and_then(Value::as_str) {
            println!("{detail}");
            return;
        }
    }
    if value.get("graded").is_some() {
        print_zero_python_human(value);
        return;
    }
    if let Some(checks) = value["checks"].as_array() {
        for check in checks {
            println!(
                "{}  {}: {}",
                if check["ok"] == true { "PASS" } else { "FAIL" },
                check["name"].as_str().unwrap_or(""),
                check["detail"].as_str().unwrap_or("")
            );
        }
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
        );
    }
}

fn print_zero_python_human(value: &Value) {
    println!("scope: {}", value["scope"].as_str().unwrap_or(""));
    println!("exit_code: {}", value["exit_code"].as_i64().unwrap_or(0));
    if let Some(graded) = value["graded"].as_object() {
        println!(
            "exec={} text={} naming={} artifact={} unregistered_text={}",
            graded.get("exec").and_then(Value::as_u64).unwrap_or(0),
            graded.get("text").and_then(Value::as_u64).unwrap_or(0),
            graded.get("naming").and_then(Value::as_u64).unwrap_or(0),
            graded.get("artifact").and_then(Value::as_u64).unwrap_or(0),
            value["unregistered_text"].as_u64().unwrap_or(0),
        );
    }
    if let Some(hits) = value["hits"].as_array() {
        for hit in hits {
            let waived = hit["waived_by"]
                .as_str()
                .map(|code| format!(" waived_by={code}"))
                .unwrap_or_default();
            println!(
                "{}:{}:{} [{}]{}",
                hit["file"].as_str().unwrap_or(""),
                hit["line"].as_u64().unwrap_or(0),
                hit["rule"].as_str().unwrap_or(""),
                hit["grade"].as_str().unwrap_or(""),
                waived
            );
        }
    }
    if value["archive"].is_object() {
        for (name, key) in [
            ("missing_marker", "missing_marker"),
            ("misplaced", "misplaced"),
            ("layer_conflict", "layer_conflict"),
            ("command_block", "command_block"),
        ] {
            if let Some(items) = value["archive"][key].as_array() {
                for item in items {
                    let summary = item
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| item.to_string());
                    println!("archive.{name}: {summary}");
                }
            }
        }
        println!(
            "archive.files={} exec_in_archive={}",
            value["archive"]["files"].as_u64().unwrap_or(0),
            value["archive"]["exec_in_archive"].as_u64().unwrap_or(0),
        );
    }
}

/// 为既有数据库补默认标的池。
///
/// bootstrap 只在「新建数据库」时写默认标的池，老库的池一直是空的：
/// 引擎扫不到标的 → 每轮信号审计都是空的。这里对老库做一次幂等回填：
/// 池为空才写入，非空一律不动（尊重用户自己增删过的池子）。
fn seed_symbols(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let db = open_db(&options.db_path)?;
    let conn = db
        .conn
        .lock()
        .map_err(|_| (1, "数据库连接锁被占用".to_string()))?;
    let existing: i64 = conn
        .query_row("SELECT COUNT(*) FROM trading_symbols", [], |row| row.get(0))
        .map_err(|error| (1, format!("读取标的池失败: {error}")))?;
    if existing > 0 {
        return Ok((
            0,
            json!({
                "ok": true,
                "seeded": 0,
                "dry_run": !options.apply,
                "skipped": true,
                "existing": existing,
                "detail": format!("标的池已有 {existing} 个标的，未做任何改动"),
            }),
        ));
    }
    if !options.apply {
        return Ok((
            0,
            json!({
                "ok": true,
                "seeded": 0,
                "dry_run": true,
                "skipped": false,
                "existing": existing,
                "detail": "标的池为空；加 --apply 执行写入".to_string(),
            }),
        ));
    }
    let seeded =
        crate::bootstrap::seed_default_symbols(&conn, crate::now()).map_err(|error| (1, error))?;
    Ok((
        0,
        json!({
            "ok": true,
            "seeded": seeded,
            "dry_run": false,
            "skipped": false,
            "existing": existing,
            "detail": format!("已写入 {seeded} 个默认标的（币安 U 本位规格）"),
        }),
    ))
}

fn open_db(path: &str) -> Result<Db, (i32, String)> {
    Db::open(path).map_err(|error| (1, format!("无法打开数据库 {path}: {error}")))
}

fn preflight(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let db = open_db(&options.db_path)?;
    let security = db.setting("security").ok().flatten().unwrap_or(Value::Null);
    let mut checks: Vec<Value> = Vec::new();
    checks.push(json!({
        "name": "数据库",
        "ok": true,
        "detail": format!("{} 可读", options.db_path),
    }));
    let has_password = security["password_hash"]
        .as_str()
        .map(|value| !value.is_empty())
        .unwrap_or(false);
    checks.push(json!({
        "name": "管理员",
        "ok": has_password,
        "detail": if has_password { "密码哈希存在".to_string() } else { "缺少密码哈希".to_string() },
    }));
    let conn = db
        .conn
        .lock()
        .map_err(|_| (1, "数据库锁不可用".to_string()))?;
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    checks.push(json!({
        "name": "表结构",
        "ok": tables >= 20,
        "detail": format!("{tables} 张表（基线 23）"),
    }));

    let mode: String = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key='mode'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "demo".to_string());
    let master_ok = master_key_path(options).is_some_and(|path| path.is_file());
    checks.push(json!({
        "name": "主密钥",
        "ok": master_ok,
        "detail": if master_ok { "文件存在".to_string() } else { "未找到主密钥文件".to_string() },
    }));
    let data_dir = Path::new(&options.db_path)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let writable = std::fs::create_dir_all(data_dir).is_ok()
        && std::fs::write(data_dir.join(".preflight.tmp"), b"ok")
            .map(|_| std::fs::remove_file(data_dir.join(".preflight.tmp")).is_ok())
            .unwrap_or(false);
    checks.push(json!({
        "name": "数据目录",
        "ok": writable,
        "detail": if writable { format!("{} 可写", data_dir.display()) } else { format!("{} 不可写", data_dir.display()) },
    }));
    let clock_ok = crate::now() > 1_600_000_000;
    checks.push(json!({
        "name": "时钟",
        "ok": clock_ok,
        "detail": format!("ts={}", crate::now()),
    }));
    checks.push(json!({
        "name": "交易环境",
        "ok": mode == "demo" || mode == "live",
        "detail": mode,
    }));
    let ok = checks.iter().all(|check| check["ok"] == true);
    Ok((
        if ok { 0 } else { 1 },
        json!({"ok": ok, "mode": mode, "checks": checks}),
    ))
}

fn master_key_path(options: &Options) -> Option<PathBuf> {
    let db = Path::new(&options.db_path);
    db.parent().map(|dir| dir.join(".master_key"))
}

/// 输出当前交易环境的交易所 Secret：sidecar 部署脚本要它才能签名。
/// 只打到标准输出，不写日志、不进事件流。
fn show_exec_secret(options: &Options, config: &Config) -> Result<(i32, Value), (i32, String)> {
    let db = open_db(&options.db_path)?;
    let mode = options
        .mode
        .clone()
        .filter(|value| value == "live" || value == "demo")
        .or_else(|| {
            db.conn.lock().ok().and_then(|conn| {
                conn.query_row(
                    "SELECT value FROM engine_state WHERE key='mode'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            })
        })
        .unwrap_or_else(|| "demo".to_string());
    let data = db
        .setting("data")
        .map_err(|error| (1, error.to_string()))?
        .unwrap_or(Value::Null);
    let field = if mode == "live" {
        "live_api_secret_enc"
    } else {
        "demo_api_secret_enc"
    };
    let ciphertext = data[field].as_str().unwrap_or("").trim().to_string();
    if ciphertext.is_empty() {
        return Err((
            1,
            format!("{mode} 环境没有配置交易所 Secret（先在参数页填写 API 凭证）"),
        ));
    }
    let key_raw = std::fs::read_to_string(&config.key_file)
        .map_err(|error| (1, format!("读取主密钥失败: {error}")))?;
    let secret =
        crate::secret::decrypt_secret(key_raw.trim(), &ciphertext).map_err(|error| (1, error))?;
    if secret.trim().is_empty() {
        return Err((
            1,
            format!("{mode} 环境没有配置交易所 Secret（先在参数页填写 API 凭证）"),
        ));
    }
    Ok((0, json!({"ok": true, "mode": mode, "raw_secret": secret})))
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buffer = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buffer);
    hex::encode(buffer)
}

fn reset_password(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let db = open_db(&options.db_path)?;
    let password = match options.password.clone() {
        Some(value) if !value.trim().is_empty() => value,
        _ => {
            let length = 16usize;
            let alphabet: Vec<char> = "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789"
                .chars()
                .collect();
            let mut out = String::with_capacity(length);
            let mut seed = crate::now_millis() as u64;
            for _ in 0..length {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                out.push(alphabet[(seed >> 33) as usize % alphabet.len()]);
            }
            out
        }
    };
    let salt = random_hex(16);
    let hash = crate::auth::hash_password(&password, &salt);
    let mut security = db
        .setting("security")
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}));
    let object = security
        .as_object_mut()
        .ok_or_else(|| (1, "security 配置不是对象".to_string()))?;
    object.insert("password_hash".to_string(), json!(hash));
    object.insert("password_is_default".to_string(), json!(false));
    let version = object
        .get("token_version")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1;
    object.insert("token_version".to_string(), json!(version));
    let conn = db
        .conn
        .lock()
        .map_err(|_| (1, "数据库锁不可用".to_string()))?;
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES('security', ?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        (
            serde_json::to_string(&security).unwrap_or_else(|_| "{}".to_string()),
            crate::now(),
        ),
    )
    .map_err(|error| (1, error.to_string()))?;
    Ok((
        0,
        json!({
            "ok": true,
            "username": security["username"].as_str().unwrap_or("admin"),
            "password": password,
            "token_version": version,
            "notice": "新密码仅在本次终端输出，请立即保存并修改",
        }),
    ))
}

fn cleanup_history(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let db = open_db(&options.db_path)?;
    let system = db.setting("system").ok().flatten().unwrap_or(Value::Null);
    let conn = db
        .conn
        .lock()
        .map_err(|_| (1, "数据库锁不可用".to_string()))?;
    let event_days = system["event_retention_days"].as_i64().unwrap_or(14).max(1);
    let signal_days = system["signal_retention_days"]
        .as_i64()
        .unwrap_or(14)
        .max(1);
    let now = crate::now();
    let event_cutoff = now - event_days * 86_400;
    let signal_cutoff = now - signal_days * 86_400;
    let events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM engine_events WHERE ts < ?1",
            [event_cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let signals: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signals WHERE ts < ?1",
            [signal_cutoff],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let mut deleted = json!({"engine_events": 0, "signals": 0});
    if options.apply {
        if options.vacuum && !options.force && service_reachable() {
            return Err((
                1,
                "服务在线，--vacuum 被拒绝（需先停服或加 --force）".to_string(),
            ));
        }
        deleted["engine_events"] = json!(conn
            .execute("DELETE FROM engine_events WHERE ts < ?1", [event_cutoff])
            .unwrap_or(0));
        deleted["signals"] = json!(conn
            .execute("DELETE FROM signals WHERE ts < ?1", [signal_cutoff])
            .unwrap_or(0));
        if options.vacuum {
            let _ = conn.execute_batch("VACUUM;");
        }
    }
    Ok((
        0,
        json!({
            "ok": true,
            "apply": options.apply,
            "dry_run": !options.apply,
            "cutoff": {"engine_events": event_cutoff, "signals": signal_cutoff},
            "matched": {"engine_events": events, "signals": signals},
            "deleted": deleted,
            "vacuum": options.vacuum && options.apply,
        }),
    ))
}

fn backup_create(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let key_file = master_key_path(options).ok_or((1, "无法定位主密钥路径".to_string()))?;
    let name = options.name.clone().unwrap_or_else(|| "manual".to_string());
    let output =
        crate::backup::create_standalone(&options.db_path, &key_file, &name, !options.plain)
            .map_err(|error| (1, error))?;
    Ok((0, output))
}

fn backup_drill(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let key_file = master_key_path(options).ok_or((1, "无法定位主密钥路径".to_string()))?;
    let output = crate::backup::drill(&options.db_path, &key_file, options.keep)
        .map_err(|error| (1, error))?;
    let ok = output["ok"].as_bool().unwrap_or(false);
    Ok((if ok { 0 } else { 1 }, output))
}

fn run_async<F, T>(future: F) -> Result<T, String>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("创建异步运行时失败: {error}"))
            .map(|runtime| runtime.block_on(future))
    })
    .join()
    .map_err(|_| "异步任务线程异常".to_string())?
}

fn final_verdict(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let db = open_db(&options.db_path)?;
    let interval = options
        .interval
        .clone()
        .unwrap_or_else(|| "15m".to_string());
    let symbols: Vec<String> = options
        .symbols
        .clone()
        .unwrap_or_else(|| "BTCUSDT,ETHUSDT".to_string())
        .split(',')
        .map(str::trim)
        .filter(|symbol| !symbol.is_empty())
        .map(str::to_uppercase)
        .collect();
    if symbols.is_empty() {
        return Err((2, "缺少 --symbols".to_string()));
    }
    let bars = options.bars.unwrap_or(600);
    let folds = options.folds.unwrap_or(3);
    let train_bars = options.train_bars.unwrap_or(600);
    if !(200..=1500).contains(&bars) {
        return Err((2, "bars 必须在 200 到 1500 之间".to_string()));
    }
    if !(2..=6).contains(&folds) {
        return Err((2, "folds 必须在 2 到 6 之间".to_string()));
    }
    crate::backtest::check_interval(&interval).map_err(|detail| (2, detail))?;
    let result = run_async(async move {
        crate::backtest::final_verdict(db, &interval, &symbols, bars, folds, train_bars).await
    })
    .map_err(|error| (1, error))?;
    let output = result.map_err(|error| (1, error))?;
    let ok = output["ok"].as_bool().unwrap_or(false);
    Ok((if ok { 0 } else { 1 }, output))
}

fn verify_partial_tp(options: &Options) -> Result<(i32, Value), (i32, String)> {
    use crate::manage::{manage, ManageAction, ManagedPosition};

    let db = open_db(&options.db_path)?;
    let strategy = db
        .setting("strategy")
        .ok()
        .flatten()
        .unwrap_or_else(|| crate::settings_api::defaults_of("strategy"));
    let risk = db
        .setting("risk")
        .ok()
        .flatten()
        .unwrap_or_else(|| crate::settings_api::defaults_of("risk"));
    let mut cfg = crate::runtime::EngineRuntime::manage_config(&strategy, &risk);
    let symbol = options
        .symbol
        .clone()
        .unwrap_or_else(|| "BTCUSDT".to_string())
        .to_uppercase();
    let mut checks: Vec<Value> = Vec::new();

    if cfg.partial_tp_at_r <= 0.0 {
        cfg.partial_tp_at_r = 1.0;
    }
    let risk = 5.0;
    let entry = 100.0;
    let mut base = ManagedPosition {
        side: "long".to_string(),
        entry_price: entry,
        mark_price: entry + cfg.partial_tp_at_r * risk,
        peak_price: entry + cfg.partial_tp_at_r * risk,
        sl_price: entry - risk,
        initial_sl: entry - risk,
        tp_price: 0.0,
        qty: 1.0,
        partial_taken: false,
        score: 0.0,
        held_minutes: 0.0,
    };
    let expected_qty = {
        let raw = base.qty * cfg.partial_tp_ratio;
        let stepped = if cfg.qty_step > 0.0 {
            (raw / cfg.qty_step + 1e-12).floor() * cfg.qty_step
        } else {
            raw
        };
        if stepped < cfg.min_qty - 1e-12 {
            0.0
        } else {
            stepped
        }
    };
    let action = manage(&base, &cfg);
    let partial_qty = match action {
        ManageAction::PartialTakeProfit { qty } => Some(qty),
        _ => None,
    };
    checks.push(json!({
        "name": "R 达到阈值触发部分止盈",
        "ok": partial_qty.is_some_and(|qty| (qty - expected_qty).abs() < 1e-9),
        "detail": format!("R={} qty={:?} expect={expected_qty}", cfg.partial_tp_at_r, partial_qty),
    }));

    let mut no_roi = cfg.clone();
    no_roi.roi_table.clear();
    base.partial_taken = true;
    let action = manage(&base, &no_roi);
    let breakeven_ok = matches!(
        action,
        ManageAction::MoveToBreakeven { new_sl } if (new_sl - entry * 1.0004).abs() < 1e-9
    );
    checks.push(json!({
        "name": "剩余仓位止损上移保本",
        "ok": breakeven_ok,
        "detail": format!("{:?}", action).replace('\n', " "),
    }));

    let mut doubled = base.clone();
    doubled.mark_price = entry + cfg.partial_tp_at_r * risk * 2.0;
    doubled.peak_price = doubled.mark_price;
    let action = manage(&doubled, &no_roi);
    checks.push(json!({
        "name": "已部分止盈不再触发第二次",
        "ok": !matches!(action, ManageAction::PartialTakeProfit { .. }),
        "detail": format!("{:?}", action).replace('\n', " "),
    }));

    let mut roi_rows = Vec::new();
    for (minutes, target) in &cfg.roi_table {
        let mut hit_row = base.clone();
        hit_row.sl_price = 0.0;
        hit_row.tp_price = 0.0;
        hit_row.held_minutes = *minutes;
        hit_row.mark_price = entry * (1.0 + (target + 0.01) / 100.0);
        let hit = matches!(
            manage(&hit_row, &cfg),
            ManageAction::Close { ref reason } if reason == "roi_target"
        );
        let mut miss_row = hit_row.clone();
        miss_row.mark_price = entry * (1.0 + (target - 0.01) / 100.0);
        let miss = !matches!(
            manage(&miss_row, &cfg),
            ManageAction::Close { ref reason } if reason == "roi_target"
        );
        roi_rows.push(json!({
            "minutes": minutes,
            "target_pct": target,
            "at_target_closes": hit,
            "below_target_holds": miss,
        }));
    }
    let roi_ok = !roi_rows.is_empty()
        && roi_rows
            .iter()
            .all(|row| row["at_target_closes"] == true && row["below_target_holds"] == true);
    checks.push(json!({
        "name": "时间阶梯止盈比例",
        "ok": roi_ok,
        "detail": format!("{} 档", roi_rows.len()),
    }));

    let pool = db
        .conn
        .lock()
        .ok()
        .and_then(|conn| {
            let total: i64 = conn
                .query_row("SELECT COUNT(*) FROM trading_symbols", [], |row| row.get(0))
                .ok()?;
            let present: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM trading_symbols WHERE symbol=?1",
                    [&symbol],
                    |row| row.get(0),
                )
                .ok()?;
            Some((total, present))
        })
        .unwrap_or((0, 0));
    if pool.0 > 0 && pool.1 == 0 {
        checks.push(json!({
            "name": "标的在池",
            "ok": false,
            "detail": format!("{symbol} 不在标的池（仅用于标注，未发起实盘下单）"),
        }));
    }

    let ok = checks.iter().all(|check| check["ok"] == true);
    Ok((
        if ok { 0 } else { 1 },
        json!({
            "ok": ok,
            "symbol": symbol,
            "source": "内置固定输入，复用 manage 同口径规则（不下单）",
            "checks": checks,
            "roi_ladder": roi_rows,
        }),
    ))
}

fn p99_bench(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let orders = options.orders.unwrap_or(3000).max(1) as usize;
    let output = crate::ops_tools::p99_bench(orders).map_err(|error| (1, error))?;
    Ok((0, output))
}

fn rtt_probe_command(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let target = options
        .target
        .clone()
        .unwrap_or_else(|| "testnet".to_string());
    let seconds = options.seconds.unwrap_or(60).max(1) as u64;
    let result = run_async(async move {
        crate::ops_tools::rtt_probe(&target, seconds, 4.0, "/fapi/v1/time").await
    })
    .map_err(|error| (1, error))?;
    result.map_err(|error| (1, error)).map(|output| (0, output))
}

fn bench_exec_command(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let orders = options.orders.unwrap_or(200).max(1) as usize;
    let delay = options.delay_ms.unwrap_or(0).max(0) as u64;
    let result = run_async(async move { crate::ops_tools::bench_exec(orders, delay).await })
        .map_err(|error| (1, error))?;
    let output = result.map_err(|error| (1, error))?;
    let ok = output["ok"].as_bool().unwrap_or(false);
    Ok((if ok { 0 } else { 1 }, output))
}

fn p99_real_command(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let key_file = master_key_path(options).ok_or((1, "无法定位主密钥路径".to_string()))?;
    let db_path = options.db_path.clone();
    let orders = options.orders.unwrap_or(15).max(1) as usize;
    let per_minute = options.per_minute.unwrap_or(5).max(1) as f64;
    let result = run_async(async move {
        crate::ops_tools::p99_real(&db_path, &key_file, orders, per_minute).await
    })
    .map_err(|error| (1, error))?;
    let output = result.map_err(|error| (1, error))?;
    let ok = output["ok"].as_bool().unwrap_or(false);
    Ok((if ok { 0 } else { 1 }, output))
}

fn mock_service(options: &Options, exchange: bool) -> Result<(i32, Value), (i32, String)> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let running = Arc::new(AtomicBool::new(true));
    if let Some(seconds) = options.seconds {
        if seconds > 0 {
            let flag = running.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(seconds as u64));
                flag.store(false, Ordering::Relaxed);
            });
        }
    }
    let port = options.port.unwrap_or(0);
    if exchange {
        let delay = options.delay_ms.unwrap_or(0).max(0) as u64;
        let output =
            crate::ops_tools::mock_exchange(port, delay, running).map_err(|error| (1, error))?;
        Ok((0, output))
    } else {
        let secret = options
            .secret
            .clone()
            .filter(|value| !value.trim().is_empty())
            .ok_or((2, "缺少 --secret".to_string()))?;
        let output = crate::ops_tools::mock_exec_core(port, &secret, running)
            .map_err(|error| (1, error))?;
        Ok((0, output))
    }
}

fn contract_diff_command(
    options: &Options,
    config: &Config,
) -> Result<(i32, Value), (i32, String)> {
    let baseline = options
        .baseline
        .clone()
        .ok_or((2, "缺少 --baseline <file>".to_string()))?;
    let output = crate::ops_tools::contract_diff(&config.home, Path::new(&baseline))
        .map_err(|error| (1, error))?;
    let ok = output["ok"].as_bool().unwrap_or(false);
    Ok((if ok { 0 } else { 1 }, output))
}

fn verify_exec_core_command(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let output =
        crate::ops_tools::verify_exec_core(options.target.as_deref(), options.secret.as_deref())
            .map_err(|error| (1, error))?;
    let ok = output["ok"].as_bool().unwrap_or(false);
    Ok((if ok { 0 } else { 1 }, output))
}

fn service_reachable() -> bool {
    use std::net::ToSocketAddrs;
    let Ok(mut addresses) = "127.0.0.1:8899".to_socket_addrs() else {
        return false;
    };
    addresses.any(|address| {
        std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(200))
            .is_ok()
    })
}

#[derive(Clone, Debug)]
struct ReconcileRow {
    symbol: String,
    side: String,
    entry: f64,
    exit: f64,
    pnl: f64,
}

fn parse_csv(path: &str) -> Result<Vec<ReconcileRow>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("读取 {path} 失败: {error}"))?;
    let mut rows = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split(',').map(str::trim).collect();
        if parts
            .first()
            .is_some_and(|value| value.eq_ignore_ascii_case("symbol"))
        {
            continue;
        }
        if parts.len() < 5 {
            return Err(format!(
                "第 {} 行列数不足（需要 symbol,side,entry,exit,pnl）",
                index + 1
            ));
        }
        let number = |position: usize| -> Result<f64, String> {
            parts[position]
                .parse::<f64>()
                .map_err(|_| format!("第 {} 行第 {} 列不是数字", index + 1, position + 1))
        };
        rows.push(ReconcileRow {
            symbol: parts[0].to_uppercase(),
            side: parts[1].to_lowercase(),
            entry: number(2)?,
            exit: number(3)?,
            pnl: number(4)?,
        });
    }
    Ok(rows)
}

fn reconcile(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let (backtest, live) = if options.self_test {
        let rows = vec![
            ReconcileRow {
                symbol: "BTCUSDT".to_string(),
                side: "long".to_string(),
                entry: 100.0,
                exit: 110.0,
                pnl: 10.0,
            },
            ReconcileRow {
                symbol: "ETHUSDT".to_string(),
                side: "short".to_string(),
                entry: 200.0,
                exit: 190.0,
                pnl: 10.0,
            },
        ];
        (rows.clone(), rows)
    } else {
        let backtest = options
            .backtest
            .as_deref()
            .ok_or((2, "缺少 --backtest <csv>".to_string()))?;
        let live = options
            .live
            .as_deref()
            .ok_or((2, "缺少 --live <csv>".to_string()))?;
        (
            parse_csv(backtest).map_err(|error| (1, error))?,
            parse_csv(live).map_err(|error| (1, error))?,
        )
    };
    let mut matched = 0usize;
    let mut unmatched = 0usize;
    let mut bps_sum = 0.0;
    let mut bps_count = 0usize;
    let mut reason_mismatch = 0usize;
    for row in &backtest {
        match live
            .iter()
            .find(|live| live.symbol == row.symbol && live.side == row.side)
        {
            Some(live) => {
                matched += 1;
                for (backtest_price, live_price) in [(row.entry, live.entry), (row.exit, live.exit)]
                {
                    if live_price.abs() > 0.0 {
                        bps_sum += ((backtest_price - live_price) / live_price * 10_000.0).abs();
                        bps_count += 1;
                    }
                }
                if row.pnl.signum() != live.pnl.signum() {
                    reason_mismatch += 1;
                }
            }
            None => unmatched += 1,
        }
    }
    let live_only = live
        .iter()
        .filter(|live| {
            !backtest
                .iter()
                .any(|row| row.symbol == live.symbol && row.side == live.side)
        })
        .count();
    unmatched += live_only;
    let spread_bps = if bps_count > 0 {
        bps_sum / bps_count as f64
    } else {
        0.0
    };
    let match_rate = if backtest.is_empty() {
        0.0
    } else {
        matched as f64 / backtest.len() as f64 * 100.0
    };
    let ok = unmatched == 0 && reason_mismatch == 0 && spread_bps <= 5.0;
    Ok((
        if ok { 0 } else { 1 },
        json!({
            "ok": ok,
            "match_rate_pct": round2(match_rate),
            "spread_bps": round2(spread_bps),
            "reason_mismatch": reason_mismatch,
            "unmatched": unmatched,
            "backtest_rows": backtest.len(),
            "live_rows": live.len(),
        }),
    ))
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn json_to_sql(value: &Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as SqlValue;
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(flag) => SqlValue::Integer(i64::from(*flag)),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                SqlValue::Integer(value)
            } else {
                SqlValue::Real(number.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(text) => SqlValue::Text(text.clone()),
        other => SqlValue::Text(serde_json::to_string(other).unwrap_or_default()),
    }
}

fn archive_replay(options: &Options) -> Result<(i32, Value), (i32, String)> {
    let file = options
        .file
        .clone()
        .ok_or((2, "缺少 --file <archive.jsonl.gz>".to_string()))?;
    let raw = std::fs::read(&file).map_err(|error| (1, format!("读取归档失败: {error}")))?;
    let mut decoder = flate2::read::GzDecoder::new(&raw[..]);
    let mut content = String::new();
    std::io::Read::read_to_string(&mut decoder, &mut content)
        .map_err(|error| (1, format!("解压归档失败: {error}")))?;
    let name = Path::new(&file)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let table = if name.starts_with("engine_events-") {
        "engine_events"
    } else if name.starts_with("signals-") {
        "signals"
    } else {
        return Err((2, "归档文件名无法识别表类型".to_string()));
    };
    let conn = rusqlite::Connection::open(&options.db_path)
        .map_err(|error| (1, format!("打开临时库 {} 失败: {error}", options.db_path)))?;
    let mut columns_stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| (1, error.to_string()))?;
    let columns: Vec<String> = columns_stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| (1, error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| (1, error.to_string()))?;
    let mut imported = 0usize;
    let mut min_ts = i64::MAX;
    let mut max_ts = i64::MIN;
    let mut sample = Vec::new();
    for line in content.lines() {
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(object) = row.as_object() else {
            continue;
        };
        let fields: Vec<&String> = columns
            .iter()
            .filter(|column| object.contains_key(column.as_str()))
            .collect();
        if fields.is_empty() {
            continue;
        }
        let names = fields
            .iter()
            .map(|column| column.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let placeholders = (1..=fields.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("INSERT OR REPLACE INTO {table}({names}) VALUES({placeholders})");
        let values: Vec<rusqlite::types::Value> = fields
            .iter()
            .map(|column| json_to_sql(&object[column.as_str()]))
            .collect();
        if conn
            .execute(&sql, rusqlite::params_from_iter(values))
            .is_ok()
        {
            imported += 1;
            if sample.len() < 3 {
                sample.push(row.clone());
            }
            if let Some(ts) = object.get("ts").and_then(Value::as_i64) {
                min_ts = min_ts.min(ts);
                max_ts = max_ts.max(ts);
            }
        }
    }
    Ok((
        0,
        json!({
            "ok": true,
            "table": table,
            "imported": imported,
            "time_range": if imported > 0 {
                json!({"from": min_ts, "to": max_ts})
            } else {
                json!(null)
            },
            "sample": sample,
        }),
    ))
}

fn zero_python(options: &Options, config: &Config) -> Result<(i32, Value), (i32, String)> {
    let scope = options.scope.as_str();
    const VALID_SCOPES: &[&str] = &["repo", "runtime", "image", "text", "docs", "all"];
    if !VALID_SCOPES.contains(&scope) {
        return Ok((
            2,
            json!({
                "ok": false,
                "scope": scope,
                "rules_version": crate::interpreter_refs::rules::rules_version(),
                "error": format!("非法 scope：{scope}（允许 repo/runtime/image/text/docs/all）"),
                "result": "hit",
                "exit_code": 2,
            }),
        ));
    }
    let root = config.home.clone();
    let archive_root_value = options
        .archive_root
        .clone()
        .unwrap_or_else(|| "docs/archive/history".to_string());
    let mut checks = Vec::new();
    let mut repo_hits: Vec<Value> = Vec::new();
    let mut process: Vec<String> = Vec::new();
    let mut image: Vec<Value> = Vec::new();
    let mut image_error = String::new();
    let mut scan_report = scanner::ScanReport::default();
    let mut registry = exceptions::Registry::default();
    let mut archive_report = archive::ArchiveReport::default();

    if matches!(scope, "repo" | "all") {
        repo_hits = scan_repo(&root);
        checks
            .push(json!({"check": "repo", "exit_code": if repo_hits.is_empty() { 0 } else { 2 }}));
    }
    if matches!(scope, "runtime" | "all") {
        process = python_processes();
        checks
            .push(json!({"check": "runtime", "exit_code": if process.is_empty() { 0 } else { 3 }}));
    }
    let manifest = options
        .manifest
        .clone()
        .or_else(|| std::env::var("NANOQUANT_IMAGE_MANIFEST").ok());
    if matches!(scope, "image" | "all") {
        match image_hits(manifest.as_deref()) {
            Ok(hits) => image = hits,
            Err(error) => image_error = error,
        }
        let image_code = if image.is_empty() && image_error.is_empty() {
            0
        } else {
            4
        };
        checks.push(json!({"check": "image", "exit_code": image_code}));
    }
    if matches!(scope, "text" | "all") {
        let exceptions_path = options
            .exceptions
            .clone()
            .unwrap_or_else(|| ".github/ci/python-refs-exceptions.json".to_string());
        registry = exceptions::load(&root, Path::new(&exceptions_path));
        let mut scan_options = scanner::ScanOptions::new(&root);
        if let Some(value) = options.max_depth {
            scan_options.max_depth = value;
        }
        if let Some(value) = options.max_hits {
            scan_options.max_hits = value;
        }
        if let Some(value) = options.max_file_bytes {
            scan_options.max_file_bytes = value;
        }
        scan_options.archive_root = Some(root.join(&archive_root_value));
        scan_report = match options.changed_file_list.as_deref() {
            Some(list) => scanner::scan_changed_files(&scan_options, Path::new(list)),
            None => scanner::scan(&scan_options),
        };
        exceptions::apply(&registry, &mut scan_report);
        let unregistered = scan_report
            .hits
            .iter()
            .filter(|hit| hit.waived_by.is_none())
            .count();
        let text_code = if unregistered > 0 || !scan_report.unreadable.is_empty() {
            5
        } else if !registry.is_valid() {
            6
        } else {
            0
        };
        checks.push(json!({"check": "text", "exit_code": text_code, "unregistered": unregistered}));
    }
    if matches!(scope, "docs" | "all") {
        archive_report = archive::validate(&root, &root.join(&archive_root_value));
        let docs_code = if archive_report.exec_in_archive > 0 {
            5
        } else if archive_report.is_valid() {
            0
        } else {
            7
        };
        checks.push(json!({"check": "docs", "exit_code": docs_code}));
    }

    let mut code: i32 = 0;
    for candidate in [2_i32, 3, 4, 5, 6, 7] {
        if checks
            .iter()
            .any(|check| check["exit_code"].as_i64() == Some(i64::from(candidate)))
        {
            code = candidate;
            break;
        }
    }

    if let Some(out) = options.out.as_deref() {
        if !scanner::out_path_is_allowed(&root, Path::new(out)) {
            return Ok((
                2,
                json!({
                    "ok": false,
                    "scope": scope,
                    "error": format!("--out 不得位于扫描根内的交付面：{out}"),
                    "result": "hit",
                    "exit_code": 2,
                }),
            ));
        }
    }

    let scan_hits = serde_json::to_value(&scan_report.hits).unwrap_or_else(|_| json!([]));
    let hits_value = match scope {
        "text" => scan_hits,
        "all" => merge_repo_and_text_hits(&repo_hits, &scan_hits),
        _ => json!(repo_hits),
    };
    let report_value = report::build(
        scope,
        &hits_value,
        &scan_report,
        &process,
        &image,
        &image_error,
        &registry.to_json(),
        &archive_report.to_json(),
        &json!(checks),
        code,
    );

    if let Some(out) = options.out.as_deref() {
        let text = serde_json::to_string_pretty(&report_value)
            .unwrap_or_else(|_| report_value.to_string());
        if let Err(error) = std::fs::write(out, text) {
            return Err((1, format!("写入 {out} 失败: {error}")));
        }
    }
    Ok((code, report_value))
}

fn hit_value(root: &Path, path: &Path, rule: &str, line: usize) -> Value {
    let file = path
        .strip_prefix(root)
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().to_string());
    json!({
        "file": file,
        "line": line,
        "rule": rule,
        "grade": "exec",
        "matched_rules": [rule],
        "snippet_hash": "",
        "waived_by": null
    })
}

fn hit_key(hit: &Value) -> (String, u64, String) {
    (
        hit["file"].as_str().unwrap_or("").to_string(),
        hit["line"].as_u64().unwrap_or(0),
        hit["rule"].as_str().unwrap_or("").to_string(),
    )
}

fn merge_repo_and_text_hits(repo: &[Value], text: &Value) -> Value {
    let mut merged = std::collections::BTreeMap::new();
    for hit in repo {
        merged.insert(hit_key(hit), hit.clone());
    }
    if let Some(hits) = text.as_array() {
        for hit in hits {
            merged.entry(hit_key(hit)).or_insert_with(|| hit.clone());
        }
    }
    json!(merged.into_values().collect::<Vec<_>>())
}

fn scan_repo(root: &Path) -> Vec<Value> {
    let mut hits = Vec::new();
    walk_repo(root, root, 0, &mut hits);
    hits
}

fn walk_repo(root: &Path, dir: &Path, depth: usize, hits: &mut Vec<Value>) {
    if depth > 8 || hits.len() >= 500 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            ".git" | "target" | "node_modules" | "dist" | "data" | ".codeartsdoer"
        ) {
            continue;
        }
        if path.is_dir() {
            if matches!(name.as_str(), ".venv" | "__pycache__") {
                hits.push(hit_value(root, &path, "python_env", 0));
                continue;
            }
            walk_repo(root, &path, depth + 1, hits);
        } else if name.ends_with(".py") {
            hits.push(hit_value(root, &path, "python_script", 0));
        } else if name.starts_with("requirements") && name.ends_with(".txt") {
            hits.push(hit_value(root, &path, "requirements", 0));
        } else if is_scannable(&name) {
            scan_text_file(root, &path, hits);
        }
    }
}

fn is_scannable(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".sh")
        || lower.ends_with(".ps1")
        || lower.ends_with(".yml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".service")
        || lower.ends_with(".toml")
        || lower.ends_with(".cfg")
        || lower.ends_with(".ini")
        || lower.starts_with("dockerfile")
        || lower.starts_with("docker-compose")
        || lower == "makefile"
        || name == "README.md"
        || lower == "runbook.md"
        || lower == "mainnet_drill.md"
}

fn scan_text_file(root: &Path, path: &Path, hits: &mut Vec<Value>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for (index, line) in text.lines().enumerate() {
        if hits.len() >= 500 {
            return;
        }
        let lower = line.to_lowercase();
        let rules: &[(&str, &str)] = &[
            ("python -m nanoquant", "python_module"),
            ("python -c", "python_inline"),
            ("python3 -c", "python_inline"),
            ("python:", "python_image"),
            ("python3:", "python_image"),
            ("pip install", "pip_install"),
            ("pip3 install", "pip_install"),
            ("import nanoquant", "import_nanoquant"),
            ("NANOQUANT_ROLLBACK_KERNEL", "rollback_switch"),
        ];
        for (needle, rule) in rules {
            if lower.contains(&needle.to_lowercase()) {
                hits.push(hit_value(root, path, rule, index + 1));
                break;
            }
        }
    }
}

fn python_processes() -> Vec<String> {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .output();
        let Ok(output) = output else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .filter(|line| line.to_lowercase().contains("python"))
            .take(20)
            .map(str::to_string)
            .collect()
    }
    #[cfg(not(windows))]
    {
        let output = std::process::Command::new("ps")
            .args(["-eo", "comm="])
            .output();
        let Ok(output) = output else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&output.stdout);
        text.lines()
            .map(str::trim)
            .filter(|line| line.starts_with("python"))
            .take(20)
            .map(str::to_string)
            .collect()
    }
}

fn image_hits(path: Option<&str>) -> Result<Vec<Value>, String> {
    let Some(path) = path else {
        return Err("镜像清单缺失（--manifest 或 NANOQUANT_IMAGE_MANIFEST）".to_string());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("读取镜像清单 {path} 失败: {error}"))?;
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let lower = line.to_lowercase();
        if lower.contains("python") || lower.contains("pip") {
            hits.push(json!({"file": path, "line": index + 1, "rule": "image_python"}));
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            home: PathBuf::from("."),
            data_dir: PathBuf::from("."),
            db_path: String::new(),
            key_file: PathBuf::from(".master_key"),
            host: "127.0.0.1".to_string(),
            port: 8899,
            admin_password: None,
            frontend: None,
        }
    }

    #[test]
    fn help_lists_all_commands() {
        let text = COMMANDS.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        assert_eq!(text.len(), 20);
        for name in [
            "preflight",
            "reconcile",
            "reset-password",
            "backup-create",
            "backup-drill",
            "archive-replay",
            "cleanup-history",
            "final-verdict",
            "bench-exec",
            "p99-bench",
            "p99-real",
            "rtt-probe",
            "verify-exec-core",
            "show-exec-secret",
            "seed-symbols",
            "mock-exec-core",
            "mock-exchange",
            "verify-partial-tp",
            "contract-diff",
            "zero-python",
        ] {
            assert!(text.contains(&name), "缺少命令 {name}");
        }
        assert_eq!(
            run(&["ops".to_string(), "--help".to_string()], &config()),
            0
        );
    }

    #[test]
    fn unknown_command_returns_2() {
        let code = run(&["ops".to_string(), "not-a-command".to_string()], &config());
        assert_eq!(code, 2);
    }

    #[test]
    fn reset_password_updates_hash_and_bumps_token_version() {
        let dir = std::env::temp_dir().join(format!("nq-cli-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("nanoquant.db");
        crate::bootstrap::initialize(&db_path, &dir.join(".master_key"), Some("old-password"))
            .unwrap();
        let mut cfg = config();
        cfg.db_path = db_path.to_string_lossy().into_owned();
        let code = run(
            &[
                "ops".to_string(),
                "reset-password".to_string(),
                "--password".to_string(),
                "new-password".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ],
            &cfg,
        );
        assert_eq!(code, 0);
        let db = Db::open(&cfg.db_path).unwrap();
        let security = db.setting("security").unwrap().unwrap();
        assert!(crate::auth::verify_password(
            "new-password",
            security["password_hash"].as_str().unwrap()
        ));
        assert_eq!(security["token_version"], 1);
        assert_eq!(security["password_is_default"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_symbols_backfills_an_empty_pool_and_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("nq-cli-seed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("nanoquant.db");
        crate::bootstrap::initialize(&db_path, &dir.join(".master_key"), Some("test-password"))
            .unwrap();
        {
            // 模拟老库：建库后清空标的池
            let db = Db::open(db_path.to_string_lossy().as_ref()).unwrap();
            let conn = db.conn.lock().unwrap();
            conn.execute_batch("DELETE FROM trading_symbols").unwrap();
        }
        let mut cfg = config();
        cfg.db_path = db_path.to_string_lossy().into_owned();
        let args = [
            "ops".to_string(),
            "seed-symbols".to_string(),
            "--db".to_string(),
            cfg.db_path.clone(),
            "--format".to_string(),
            "json".to_string(),
        ];
        let apply_args = [
            "ops".to_string(),
            "seed-symbols".to_string(),
            "--apply".to_string(),
            "--db".to_string(),
            cfg.db_path.clone(),
            "--format".to_string(),
            "json".to_string(),
        ];
        let count = |db_path: &str| -> i64 {
            let db = Db::open(db_path).unwrap();
            let conn = db.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM trading_symbols", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(run(&args, &cfg), 0, "dry-run 应成功");
        assert_eq!(count(&cfg.db_path), 0, "dry-run 不应写入");
        assert_eq!(run(&apply_args, &cfg), 0, "空池应该被回填");
        assert_eq!(count(&cfg.db_path), 3, "默认池应为 3 个标的");
        assert_eq!(run(&apply_args, &cfg), 0, "重复执行应仍成功");
        assert_eq!(count(&cfg.db_path), 3, "重复执行不应重复写入");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preflight_passes_on_a_bootstrapped_database() {
        let dir = std::env::temp_dir().join(format!("nq-cli-preflight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("nanoquant.db");
        crate::bootstrap::initialize(&db_path, &dir.join(".master_key"), Some("test-password"))
            .unwrap();
        let mut cfg = config();
        cfg.db_path = db_path.to_string_lossy().into_owned();
        let code = run(
            &[
                "ops".to_string(),
                "preflight".to_string(),
                "--db".to_string(),
                cfg.db_path.clone(),
                "--format".to_string(),
                "json".to_string(),
            ],
            &cfg,
        );
        assert_eq!(code, 0, "空库首启后预检应全部 PASS");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reconcile_self_test_matches_perfectly() {
        let cfg = config();
        let code = run(
            &[
                "ops".to_string(),
                "reconcile".to_string(),
                "--self-test".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ],
            &cfg,
        );
        assert_eq!(code, 0);
    }

    #[test]
    fn archive_replay_imports_gzip_rows() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("nq-cli-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("replay.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE engine_events(id INTEGER PRIMARY KEY, ts INTEGER, level TEXT, \
             category TEXT, message TEXT, payload TEXT);",
        )
        .unwrap();
        drop(conn);
        let file = dir.join("engine_events-20260101.jsonl.gz");
        let file_handle = std::fs::File::create(&file).unwrap();
        let mut encoder =
            flate2::write::GzEncoder::new(file_handle, flate2::Compression::default());
        for (id, ts) in [(1, 100), (2, 200)] {
            writeln!(
                encoder,
                "{}",
                json!({"id": id, "ts": ts, "level": "info", "category": "engine", "message": "m", "payload": "{}"})
            )
            .unwrap();
        }
        encoder.finish().unwrap();
        let mut cfg = config();
        cfg.db_path = db_path.to_string_lossy().into_owned();
        let code = run(
            &[
                "ops".to_string(),
                "archive-replay".to_string(),
                "--file".to_string(),
                file.to_string_lossy().into_owned(),
                "--db".to_string(),
                cfg.db_path.clone(),
                "--format".to_string(),
                "json".to_string(),
            ],
            &cfg,
        );
        assert_eq!(code, 0);
        let conn = rusqlite::Connection::open(&cfg.db_path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM engine_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_partial_tp_passes_with_fixed_inputs() {
        let dir = std::env::temp_dir().join(format!("nq-cli-vtp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("nanoquant.db");
        crate::bootstrap::initialize(&db_path, &dir.join(".master_key"), Some("test-password"))
            .unwrap();
        let mut cfg = config();
        cfg.db_path = db_path.to_string_lossy().into_owned();
        let code = run(
            &[
                "verify-partial-tp".to_string(),
                "--symbol".to_string(),
                "BTCUSDT".to_string(),
                "--db".to_string(),
                cfg.db_path.clone(),
                "--format".to_string(),
                "json".to_string(),
            ],
            &cfg,
        );
        assert_eq!(code, 0, "四项固定输入断言应全部通过");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn final_verdict_rejects_out_of_range_bars_before_loading_quotes() {
        let dir = std::env::temp_dir().join(format!("nq-cli-verdict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("nanoquant.db");
        crate::bootstrap::initialize(&db_path, &dir.join(".master_key"), Some("test-password"))
            .unwrap();
        let mut cfg = config();
        cfg.db_path = db_path.to_string_lossy().into_owned();
        let code = run(
            &[
                "final-verdict".to_string(),
                "--bars".to_string(),
                "10".to_string(),
                "--symbols".to_string(),
                "BTCUSDT".to_string(),
                "--db".to_string(),
                cfg.db_path.clone(),
                "--format".to_string(),
                "json".to_string(),
            ],
            &cfg,
        );
        assert_eq!(code, 2, "越界参数必须在拉行情前拒绝");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_python_repo_scope_finds_python_and_clean_repo_passes() {
        let dir = std::env::temp_dir().join(format!("nq-cli-zero-python-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/start.py"), "print('x')").unwrap();
        let mut cfg = config();
        cfg.home = dir.clone();
        let args = [
            "ops".to_string(),
            "zero-python".to_string(),
            "--scope".to_string(),
            "repo".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ];
        assert_eq!(run(&args, &cfg), 2, "存在 .py 脚本必须命中");
        std::fs::remove_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("README.md"), "# clean\nno executable references").unwrap();
        assert_eq!(run(&args, &cfg), 0, "干净仓库应通过");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_python_repo_scope_skips_agent_metadata() {
        let dir =
            std::env::temp_dir().join(format!("nq-cli-zero-python-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".codeartsdoer/temp/accept")).unwrap();
        std::fs::write(
            dir.join(".codeartsdoer/temp/accept/x.original.ps1"),
            "pip install nanoquant\npython -m nanoquant\n",
        )
        .unwrap();
        let mut cfg = config();
        cfg.home = dir.clone();
        let args = [
            "ops".to_string(),
            "zero-python".to_string(),
            "--scope".to_string(),
            "repo".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ];
        assert_eq!(run(&args, &cfg), 0, "流程元数据目录不属于仓库可执行引用");
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/ops.py"), "print('x')").unwrap();
        assert_eq!(run(&args, &cfg), 2, "真实 .py 资产仍必须命中");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_create_writes_encrypted_package_without_master_key() {
        let dir = std::env::temp_dir().join(format!("nq-cli-backup-create-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("nanoquant.db");
        crate::bootstrap::initialize(&db_path, &dir.join(".master_key"), Some("test-password"))
            .unwrap();
        let mut cfg = config();
        cfg.db_path = db_path.to_string_lossy().into_owned();
        let code = run(
            &[
                "ops".to_string(),
                "backup-create".to_string(),
                "--name".to_string(),
                "pre-update".to_string(),
                "--format".to_string(),
                "json".to_string(),
            ],
            &cfg,
        );
        assert_eq!(code, 0, "无 Python 的更新前备份必须由 Rust 独立完成");
        let backups = dir.join("backups");
        let files: Vec<PathBuf> = std::fs::read_dir(&backups)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect();
        assert!(
            files
                .iter()
                .any(|path| path.extension().is_some_and(|ext| ext == "enc")),
            "默认应生成加密备份包: {files:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_python_image_scope_flags_python_layers_and_missing_manifest() {
        let dir = std::env::temp_dir().join(format!("nq-cli-zero-image-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("manifest.txt");
        std::fs::write(&manifest, "debian:bookworm-slim\n/app/nanoquant\n").unwrap();
        let cfg = config();
        let args = vec![
            "ops".to_string(),
            "zero-python".to_string(),
            "--scope".to_string(),
            "image".to_string(),
            "--manifest".to_string(),
            manifest.to_string_lossy().into_owned(),
            "--format".to_string(),
            "json".to_string(),
        ];
        assert_eq!(run(&args, &cfg), 0, "干净镜像清单应通过");
        std::fs::write(&manifest, "python:3.12-slim\n").unwrap();
        assert_eq!(run(&args, &cfg), 4, "镜像含 python 必须命中");
        let missing = vec![
            "ops".to_string(),
            "zero-python".to_string(),
            "--scope".to_string(),
            "image".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ];
        assert_eq!(run(&missing, &cfg), 4, "缺少镜像清单必须失败而非假通过");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn zero_python_args(scope: &str, extra: &[&str]) -> Vec<String> {
        let mut args = vec![
            "ops".to_string(),
            "zero-python".to_string(),
            "--scope".to_string(),
            scope.to_string(),
            "--format".to_string(),
            "json".to_string(),
        ];
        args.extend(extra.iter().map(|value| value.to_string()));
        args
    }

    fn write_archive(root: &Path, name: &str, body: &str) {
        let path = root.join("docs/archive/history").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content = format!(
            "> 历史记录：已退役，不可执行\n> 源路径：docs/{name}\n> 归档时间：2026-11-20T00:00:00Z\n\n{body}\n"
        );
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn zero_python_text_scope_returns_5_for_unregistered_module_command() {
        let dir = std::env::temp_dir().join(format!("nq-cli-zero-text-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "python -m nanoquant\n").unwrap();
        let mut cfg = config();
        cfg.home = dir.clone();
        assert_eq!(run(&zero_python_args("text", &[]), &cfg), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_python_text_scope_returns_6_for_over_budget_exceptions() {
        let dir = std::env::temp_dir().join(format!("nq-cli-zero-budget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::create_dir_all(dir.join(".codeartsdoer")).unwrap();
        std::fs::write(dir.join("README.md"), "# clean\n").unwrap();
        let grades = ["exec", "text", "naming", "artifact"];
        let entries = (1..=13)
            .map(|index| {
                json!({
                    "code": format!("EX-{index:02}"),
                    "scopes": ["docs/"],
                    "rules": [grades[(index - 1) % grades.len()]],
                    "reason": "测试条目",
                    "owner": "maintainer",
                    "expires_when": "never",
                    "verify": {"kind": "path_exists", "target": "docs"},
                })
            })
            .collect::<Vec<_>>();
        let registry = json!({
            "schema_version": "1.0",
            "budget": {"total": 12, "per_grade": 6},
            "entries": entries,
        });
        let exceptions = dir.join(".codeartsdoer/exceptions.json");
        std::fs::write(
            &exceptions,
            serde_json::to_string_pretty(&registry).unwrap(),
        )
        .unwrap();
        let mut cfg = config();
        cfg.home = dir.clone();
        let exceptions_arg = exceptions.to_string_lossy().into_owned();
        assert_eq!(
            run(
                &zero_python_args("text", &["--exceptions", exceptions_arg.as_str()]),
                &cfg
            ),
            6
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_python_docs_scope_returns_5_for_unmarked_archive_command() {
        let dir = std::env::temp_dir().join(format!("nq-cli-zero-docs5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_archive(&dir, "docs/a.md", "~~~text\npython -m nanoquant\n~~~");
        let mut cfg = config();
        cfg.home = dir.clone();
        assert_eq!(run(&zero_python_args("docs", &[]), &cfg), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_python_docs_scope_returns_7_for_missing_marker() {
        let dir = std::env::temp_dir().join(format!("nq-cli-zero-docs7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("docs/archive/history")).unwrap();
        std::fs::write(dir.join("docs/archive/history/a.md"), "# no marker\n").unwrap();
        let mut cfg = config();
        cfg.home = dir.clone();
        assert_eq!(run(&zero_python_args("docs", &[]), &cfg), 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_python_invalid_scope_returns_2() {
        assert_eq!(
            run(&zero_python_args("bogus", &[]), &config()),
            2,
            "非法 scope 必须退出 2"
        );
    }
}
