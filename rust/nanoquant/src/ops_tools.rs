//! 运维探针与模拟器：p99 基准、RTT 采样、执行侧压测、sidecar/mock 交易所。
//!
//! 全部命令只在本机运行，不需要外部服务；涉及真实测试网凭证的 `p99-real`
//! 在缺少凭证时明确失败，不伪造采样。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

// ---------------------------------------------------------------- 统计

pub fn percentile(values: &[f64], q: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut ordered = values.to_vec();
    ordered.sort_by(f64::total_cmp);
    let position = (ordered.len() - 1) as f64 * q;
    let low = position.floor() as usize;
    let high = (low + 1).min(ordered.len() - 1);
    let weight = position - low as f64;
    ordered[low] * (1.0 - weight) + ordered[high] * weight
}

pub fn stats(name: &str, samples: &[f64]) -> Value {
    let mean = if samples.is_empty() {
        0.0
    } else {
        samples.iter().sum::<f64>() / samples.len() as f64
    };
    json!({
        "name": name,
        "count": samples.len(),
        "p50": round3(percentile(samples, 0.50)),
        "p95": round3(percentile(samples, 0.95)),
        "p99": round3(percentile(samples, 0.99)),
        "max": round3(samples.iter().copied().fold(0.0_f64, f64::max)),
        "mean": round3(mean),
    })
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

// ---------------------------------------------------------------- p99-bench

const BENCH_SECRET: &str = "bench-secret-key";

/// 本地可控段：签名、回执序列化、SQLite 落库。
pub fn p99_bench(orders: usize) -> Result<Value, String> {
    if orders == 0 {
        return Err("orders 必须大于 0".to_string());
    }
    let mut sign_samples = Vec::with_capacity(orders);
    let mut serialize_samples = Vec::with_capacity(orders);
    let mut persist_samples = Vec::with_capacity(orders);

    for index in 0..orders {
        let query = format!(
            "symbol=BTCUSDT&side=BUY&type=MARKET&quantity=0.002&newClientOrderId=nqbench{index:06}&recvWindow=5000&timestamp={}",
            1_793_700_000_000_i64 + index as i64
        );
        let started = Instant::now();
        let signature = crate::sign::sign_query(BENCH_SECRET, &query);
        let url = format!("/fapi/v1/order?{query}&signature={signature}");
        sign_samples.push(elapsed_ms(started));
        if url.is_empty() {
            return Err("签名为空".to_string());
        }
    }

    let receipt = json!({
        "orderId": 1234567890_i64,
        "symbol": "BTCUSDT",
        "status": "FILLED",
        "clientOrderId": "nqbench000001",
        "price": "0.00",
        "avgPrice": "78526.20",
        "origQty": "0.002",
        "executedQty": "0.002",
        "cumQuote": "157.0524",
        "timeInForce": "GTC",
        "type": "MARKET",
        "reduceOnly": false,
        "closePosition": false,
        "side": "BUY",
        "positionSide": "BOTH",
        "stopPrice": "0.00",
        "workingType": "CONTRACT_PRICE",
        "priceProtect": false,
        "origType": "MARKET",
        "time": 1_793_700_000_000_i64,
        "updateTime": 1_793_700_000_123_i64,
    });
    let keys = [
        "orderId",
        "status",
        "avgPrice",
        "executedQty",
        "cumQuote",
        "updateTime",
        "clientOrderId",
    ];
    for _ in 0..orders {
        let started = Instant::now();
        let mut compact = serde_json::Map::new();
        for key in keys {
            if let Some(value) = receipt.get(key) {
                if !value.is_null() {
                    compact.insert(key.to_string(), value.clone());
                }
            }
        }
        let _ = serde_json::to_string(&compact);
        let _ = serde_json::to_string(&receipt);
        serialize_samples.push(elapsed_ms(started));
    }

    let dir = std::env::temp_dir().join(format!("nq-p99-{}", crate::now_millis()));
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建临时目录失败: {error}"))?;
    let db_path = dir.join("bench.db");
    let conn =
        rusqlite::Connection::open(&db_path).map_err(|error| format!("打开基准库失败: {error}"))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
         CREATE TABLE orders (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             ts INTEGER, symbol TEXT, side TEXT, order_type TEXT,
             qty REAL, price REAL, status TEXT, purpose TEXT,
             client_order_id TEXT, exchange_order_id TEXT, reduce_only INTEGER,
             mode TEXT, latency_ms INTEGER, raw TEXT
         );
         CREATE TABLE positions (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             symbol TEXT, side TEXT, qty REAL, entry_price REAL, mark_price REAL,
             leverage REAL, margin REAL, sl_price REAL, tp_price REAL, peak_price REAL,
             opened_at INTEGER, mode TEXT, status TEXT, entry_slippage_bps REAL
         );",
    )
    .map_err(|error| format!("建基准表失败: {error}"))?;
    let raw = serde_json::to_string(&receipt).unwrap_or_default();
    for index in 0..orders {
        let started = Instant::now();
        conn.execute(
            "INSERT INTO orders (ts,symbol,side,order_type,qty,price,status,purpose,\
             client_order_id,exchange_order_id,reduce_only,mode,latency_ms,raw) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            rusqlite::params![
                1_793_700_000_i64 + index as i64,
                "BTCUSDT",
                "BUY",
                "MARKET",
                0.002,
                78526.2,
                "FILLED",
                "entry",
                format!("nq{index}"),
                "1",
                0,
                "demo",
                12,
                raw
            ],
        )
        .map_err(|error| format!("插入订单基准失败: {error}"))?;
        conn.execute(
            "INSERT INTO positions (symbol,side,qty,entry_price,mark_price,leverage,margin,\
             sl_price,tp_price,peak_price,opened_at,mode,status,entry_slippage_bps) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            rusqlite::params![
                "BTCUSDT",
                "long",
                0.002,
                78526.2,
                78526.2,
                20,
                7.85,
                77000.0,
                80000.0,
                78526.2,
                1_793_700_000_i64 + index as i64,
                "demo",
                "open",
                0.8
            ],
        )
        .map_err(|error| format!("插入持仓基准失败: {error}"))?;
        persist_samples.push(elapsed_ms(started));
    }
    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);

    Ok(json!({
        "ok": true,
        "orders": orders,
        "sign": stats("sign", &sign_samples),
        "serialize": stats("serialize", &serialize_samples),
        "persist": stats("persist", &persist_samples),
    }))
}

// ---------------------------------------------------------------- rtt-probe

fn rtt_base(target: &str) -> Result<&'static str, String> {
    match target {
        "testnet" => Ok("https://testnet.binancefuture.com"),
        "mainnet" => Ok("https://fapi.binance.com"),
        other => Err(format!("未知目标 {other}（可选 testnet|mainnet）")),
    }
}

pub async fn rtt_probe(target: &str, seconds: u64, rate: f64, path: &str) -> Result<Value, String> {
    let base = rtt_base(target)?;
    let interval = Duration::from_secs_f64(1.0 / rate.max(0.1));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| format!("创建 HTTP 客户端失败: {error}"))?;
    let _ = client
        .get(format!("{base}/fapi/v1/time"))
        .send()
        .await
        .map(|response| response.status());
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut samples = Vec::new();
    let mut errors = 0u64;
    while Instant::now() < deadline {
        let started = Instant::now();
        match client.get(format!("{base}{path}")).send().await {
            Ok(response) if response.status().is_success() => samples.push(elapsed_ms(started)),
            _ => errors += 1,
        }
        tokio::time::sleep(interval).await;
    }
    if samples.is_empty() {
        return Err(format!("{target}{path} 无有效采样（errors={errors}）"));
    }
    Ok(json!({
        "ok": true,
        "target": target,
        "path": path,
        "errors": errors,
        "samples": stats("rtt", &samples),
    }))
}

// ---------------------------------------------------------------- 下单请求

fn order_request(
    base: &str,
    side: &str,
    qty: &str,
    reduce_only: bool,
    index: usize,
) -> crate::submit::PreparedRequest {
    let mut params = vec![
        ("symbol".to_string(), "BTCUSDT".to_string()),
        ("side".to_string(), side.to_string()),
        ("type".to_string(), "MARKET".to_string()),
        ("quantity".to_string(), qty.to_string()),
        (
            "newClientOrderId".to_string(),
            format!(
                "nqbench{}{:03}",
                crate::now_millis() % 1_000_000,
                index % 1000
            ),
        ),
    ];
    if reduce_only {
        params.push(("reduceOnly".to_string(), "true".to_string()));
    }
    crate::submit::build_request(
        "POST",
        base,
        "/fapi/v1/order",
        &params,
        "bench-key",
        "bench-secret",
        crate::now_millis(),
        5000,
    )
}

/// 单次下单并解析回执；返回细分耗时。
async fn send_order(
    transport: &crate::submit::Transport,
    request: &crate::submit::PreparedRequest,
) -> Result<f64, String> {
    let started = Instant::now();
    let (status, body) = transport
        .send_raw(request)
        .await
        .map_err(|_| "传输失败".to_string())?;
    let cost = elapsed_ms(started);
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {body}"));
    }
    let parsed: Value =
        serde_json::from_str(&body).map_err(|error| format!("回执无法解析: {error}"))?;
    let text = parsed["status"].as_str().unwrap_or("");
    if text != "FILLED" && text != "NEW" {
        return Err(format!("订单状态异常: {text}"));
    }
    Ok(cost)
}

/// 压测用：每次请求使用独立连接，避免复用到刚关闭的 mock 连接。
async fn send_order_no_pool(
    client: &reqwest::Client,
    request: &crate::submit::PreparedRequest,
) -> Result<f64, String> {
    let method =
        reqwest::Method::from_bytes(request.method.as_bytes()).unwrap_or(reqwest::Method::POST);
    let mut builder = client.request(method, &request.url);
    for (key, value) in &request.headers {
        builder = builder.header(key, value);
    }
    let started = Instant::now();
    let response = builder.send().await.map_err(|_| "传输失败".to_string())?;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let cost = elapsed_ms(started);
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {body}"));
    }
    let parsed: Value =
        serde_json::from_str(&body).map_err(|error| format!("回执无法解析: {error}"))?;
    let text = parsed["status"].as_str().unwrap_or("");
    if text != "FILLED" && text != "NEW" {
        return Err(format!("订单状态异常: {text}"));
    }
    Ok(cost)
}

// ---------------------------------------------------------------- mock-exchange

/// 最小币安合约 mock：`/fapi/v1/time|marginType|leverage|order|allOpenOrders`。
/// 阻塞运行，直到 `running` 置 false 或进程退出。
pub fn mock_exchange(port: u16, delay_ms: u64, running: Arc<AtomicBool>) -> Result<Value, String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|error| format!("监听 127.0.0.1:{port} 失败: {error}"))?;
    let actual = listener
        .local_addr()
        .map(|address| address.port())
        .unwrap_or(port);
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("设置非阻塞失败: {error}"))?;
    let seq = Arc::new(AtomicU64::new(1000));
    let orders = Arc::new(AtomicU64::new(0));
    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let seq = seq.clone();
                let orders = orders.clone();
                std::thread::spawn(move || {
                    let _ = handle_exchange_keepalive(stream, delay_ms, &seq, &orders);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(format!("accept 失败: {error}")),
        }
    }
    Ok(json!({"ok": true, "port": actual, "orders": orders.load(Ordering::Relaxed)}))
}

fn handle_exchange_keepalive(
    mut stream: TcpStream,
    delay_ms: u64,
    seq: &AtomicU64,
    orders: &AtomicU64,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        let mut request_line = String::new();
        match reader.read_line(&mut request_line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(_) => return Ok(()),
        }
        if request_line.trim().is_empty() {
            continue;
        }
        let mut content_length = 0usize;
        let mut wants_close = false;
        loop {
            let mut header = String::new();
            match reader.read_line(&mut header) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(_) => return Ok(()),
            }
            if header.trim().is_empty() {
                break;
            }
            let lower = header.to_ascii_lowercase();
            if let Some(value) = lower
                .strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
            {
                content_length = value;
            }
            if lower.starts_with("connection:") && lower.contains("close") {
                wants_close = true;
            }
        }
        if content_length > 0 {
            let mut body = vec![0u8; content_length.min(64 * 1024)];
            if reader.read_exact(&mut body).is_err() {
                return Ok(());
            }
        }
        if delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(delay_ms));
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        let path = target.split('?').next().unwrap_or(target);
        let query = target.split('?').nth(1).unwrap_or("");
        let param = |key: &str| -> String {
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string())
                .unwrap_or_default()
        };
        let (status, payload) = match (method, path) {
            ("GET", "/fapi/v1/time") => (200, json!({"serverTime": crate::now_millis()})),
            ("GET", "/fapi/v1/ping") => (200, json!({})),
            ("GET", "/fapi/v3/account") => (
                200,
                json!({
                    "totalWalletBalance": "1000",
                    "totalUnrealizedProfit": "0",
                    "totalInitialMargin": "0",
                    "availableBalance": "1000",
                    "totalMarginBalance": "1000",
                }),
            ),
            ("GET", "/fapi/v3/positionRisk") => (200, json!([])),
            ("POST", "/fapi/v1/marginType") => (200, json!({"code": 200, "msg": "success"})),
            ("POST", "/fapi/v1/leverage") => (
                200,
                json!({"leverage": param("leverage").parse::<i64>().unwrap_or(20), "symbol": param("symbol")}),
            ),
            ("POST", "/fapi/v1/order") => {
                orders.fetch_add(1, Ordering::Relaxed);
                let order_id = seq.fetch_add(1, Ordering::Relaxed) + 1;
                (
                    200,
                    json!({
                        "orderId": order_id,
                        "clientOrderId": param("newClientOrderId"),
                        "symbol": param("symbol"),
                        "status": "FILLED",
                        "avgPrice": "100.0",
                        "executedQty": param("quantity"),
                        "cumQuote": "0",
                        "updateTime": crate::now_millis(),
                    }),
                )
            }
            ("DELETE", "/fapi/v1/allOpenOrders") => (200, json!({"code": 200, "msg": "success"})),
            _ => (
                400,
                json!({"code": -1121, "msg": format!("mock: unsupported {method} {path}")}),
            ),
        };
        let body = payload.to_string();
        let connection = if wants_close { "close" } else { "keep-alive" };
        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {connection}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes())?;
        stream.flush()?;
        if wants_close {
            return Ok(());
        }
    }
}

/// 开发用 mock 交易所的连接入口；尚未接到任何自检命令，接线后取消豁免。
#[allow(dead_code)]
fn handle_exchange(
    mut stream: TcpStream,
    delay_ms: u64,
    seq: &AtomicU64,
    orders: &AtomicU64,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        if header.trim().is_empty() {
            break;
        }
        if let Some(value) = header
            .to_ascii_lowercase()
            .strip_prefix("content-length:")
            .and_then(|value| value.trim().parse::<usize>().ok())
        {
            content_length = value;
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length.min(64 * 1024)];
        let _ = reader.read_exact(&mut body);
    }
    if delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(delay_ms));
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let path = target.split('?').next().unwrap_or(target);
    let query = target.split('?').nth(1).unwrap_or("");
    let param = |key: &str| -> String {
        query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.to_string())
            .unwrap_or_default()
    };
    let (status, payload) = match (method, path) {
        ("GET", "/fapi/v1/time") => (200, json!({"serverTime": crate::now_millis()})),
        ("GET", "/fapi/v1/ping") => (200, json!({})),
        ("GET", "/fapi/v3/account") => (
            200,
            json!({
                "totalWalletBalance": "1000",
                "totalUnrealizedProfit": "0",
                "totalInitialMargin": "0",
                "availableBalance": "1000",
                "totalMarginBalance": "1000",
            }),
        ),
        ("GET", "/fapi/v3/positionRisk") => (200, json!([])),
        ("POST", "/fapi/v1/marginType") => (200, json!({"code": 200, "msg": "success"})),
        ("POST", "/fapi/v1/leverage") => (
            200,
            json!({"leverage": param("leverage").parse::<i64>().unwrap_or(20), "symbol": param("symbol")}),
        ),
        ("POST", "/fapi/v1/order") => {
            orders.fetch_add(1, Ordering::Relaxed);
            let order_id = seq.fetch_add(1, Ordering::Relaxed) + 1;
            (
                200,
                json!({
                    "orderId": order_id,
                    "clientOrderId": param("newClientOrderId"),
                    "symbol": param("symbol"),
                    "status": "FILLED",
                    "avgPrice": "100.0",
                    "executedQty": param("quantity"),
                    "cumQuote": "0",
                    "updateTime": crate::now_millis(),
                }),
            )
        }
        ("DELETE", "/fapi/v1/allOpenOrders") => (200, json!({"code": 200, "msg": "success"})),
        _ => (
            400,
            json!({"code": -1121, "msg": format!("mock: unsupported {method} {path}")}),
        ),
    };
    let body = payload.to_string();
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

// ---------------------------------------------------------------- bench-exec

pub async fn bench_exec(orders: usize, delay_ms: u64) -> Result<Value, String> {
    if orders == 0 {
        return Err("orders 必须大于 0".to_string());
    }
    let running = Arc::new(AtomicBool::new(true));
    let stop = running.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", 0)) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = ready_tx.send(Err(format!("mock 交易所监听失败: {error}")));
                return;
            }
        };
        let port = listener
            .local_addr()
            .map(|address| address.port())
            .unwrap_or(0);
        let _ = ready_tx.send(Ok(port));
        let seq = Arc::new(AtomicU64::new(1000));
        let count = Arc::new(AtomicU64::new(0));
        listener
            .set_nonblocking(true)
            .unwrap_or_else(|_| panic!("set_nonblocking"));
        while stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let seq = seq.clone();
                    let count = count.clone();
                    std::thread::spawn(move || {
                        let _ = handle_exchange_keepalive(stream, delay_ms, &seq, &count);
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    let port = ready_rx
        .recv()
        .map_err(|_| "mock 交易所未就绪".to_string())??;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|error| format!("创建 HTTP 客户端失败: {error}"))?;
    let mut open_samples = Vec::with_capacity(orders);
    let mut close_samples = Vec::with_capacity(orders);
    let mut errors = 0usize;
    let mut first_error = String::new();
    for index in 0..orders {
        let request = order_request(&base, "BUY", "0.01", false, index);
        match send_order_no_pool(&client, &request).await {
            Ok(cost) => open_samples.push(cost),
            Err(error) => {
                errors += 1;
                if first_error.is_empty() {
                    first_error = error;
                }
            }
        }
        let request = order_request(&base, "SELL", "0.01", true, index);
        match send_order_no_pool(&client, &request).await {
            Ok(cost) => close_samples.push(cost),
            Err(error) => {
                errors += 1;
                if first_error.is_empty() {
                    first_error = error;
                }
            }
        }
    }
    running.store(false, Ordering::Relaxed);
    let _ = handle.join();
    Ok(json!({
        "ok": errors == 0,
        "delay_ms": delay_ms,
        "orders": orders,
        "errors": errors,
        "first_error": first_error,
        "open": stats("open", &open_samples),
        "close": stats("close", &close_samples),
        "total": stats("total", &open_samples.iter().chain(close_samples.iter()).copied().collect::<Vec<_>>()),
    }))
}

// ---------------------------------------------------------------- mock sidecar

fn algo_of(secret: &str) -> &'static str {
    if crate::sign::sign_query(secret, "probe").len() == 88 {
        "ed25519"
    } else {
        "hmac-sha256"
    }
}

pub fn mock_exec_sidecar(
    port: u16,
    secret: &str,
    running: Arc<AtomicBool>,
) -> Result<Value, String> {
    if secret.trim().is_empty() {
        return Err("--secret 不能为空".to_string());
    }
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|error| format!("监听 127.0.0.1:{port} 失败: {error}"))?;
    let actual = listener
        .local_addr()
        .map(|address| address.port())
        .unwrap_or(port);
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("设置非阻塞失败: {error}"))?;
    let algo = algo_of(secret);
    while running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let secret = secret.to_string();
                std::thread::spawn(move || {
                    let _ = handle_sidecar(stream, &secret, algo);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(format!("accept 失败: {error}")),
        }
    }
    Ok(json!({"ok": true, "port": actual, "algo": algo}))
}

fn handle_sidecar(mut stream: TcpStream, secret: &str, algo: &str) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let reader = BufReader::new(stream.try_clone()?);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                let response = json!({"id": "unknown", "ok": false, "error": "bad json"});
                stream.write_all(format!("{response}\n").as_bytes())?;
                stream.flush()?;
                continue;
            }
        };
        let id = request["id"].as_str().unwrap_or("").to_string();
        let op = request["op"].as_str().unwrap_or("");
        let query = request["query"].as_str().unwrap_or("");
        let response = match op {
            "health" => json!({"id": id, "ok": true, "algo": algo}),
            "plan" | "sign" if !query.is_empty() => json!({
                "id": id,
                "ok": true,
                "signature": crate::sign::sign_query(secret, query),
                "algo": algo,
                "query": query,
            }),
            "plan" | "sign" => json!({"id": id, "ok": false, "error": "query 不能为空"}),
            other => json!({"id": id, "ok": false, "error": format!("unknown op: {other}")}),
        };
        stream.write_all(format!("{response}\n").as_bytes())?;
        stream.flush()?;
    }
    Ok(())
}

fn sidecar_roundtrip(target: &str, request: &Value) -> Result<Value, String> {
    let mut stream: Box<dyn ReadWrite> = if let Some(address) = target.strip_prefix("tcp:") {
        Box::new(
            TcpStream::connect(address).map_err(|error| format!("连接 {target} 失败: {error}"))?,
        )
    } else if target.starts_with("unix:") {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;
            let path = &target[5..];
            Box::new(
                UnixStream::connect(path)
                    .map_err(|error| format!("连接 {target} 失败: {error}"))?,
            )
        }
        #[cfg(not(unix))]
        {
            return Err("当前平台不支持 unix socket".to_string());
        }
    } else {
        return Err(format!(
            "目标格式应为 tcp:host:port 或 unix:path（收到 {target}）"
        ));
    };
    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(|error| format!("写入 sidecar 失败: {error}"))?;
    stream.flush().map_err(|error| error.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|error| format!("读取 sidecar 失败: {error}"))?;
    serde_json::from_str(&line).map_err(|error| format!("sidecar 回包非法: {error}"))
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

pub fn verify_sidecar(target: Option<&str>, secret: Option<&str>) -> Result<Value, String> {
    let mut embedded = None;
    let effective_secret: Option<String> = match (target, secret) {
        (None, None) => Some("verify-sidecar-default-secret".to_string()),
        (None, Some(value)) => Some(value.to_string()),
        (Some(_), value) => value.map(str::to_string),
    };
    let target = match target {
        Some(target) => target.to_string(),
        None => {
            let running = Arc::new(AtomicBool::new(true));
            let stop = running.clone();
            let secret = effective_secret.clone().unwrap_or_default();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let handle = std::thread::spawn(move || {
                let listener = match TcpListener::bind(("127.0.0.1", 0)) {
                    Ok(listener) => listener,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("mock sidecar 监听失败: {error}")));
                        return;
                    }
                };
                let port = listener
                    .local_addr()
                    .map(|address| address.port())
                    .unwrap_or(0);
                let _ = ready_tx.send(Ok(port));
                listener
                    .set_nonblocking(true)
                    .unwrap_or_else(|_| panic!("set_nonblocking"));
                while stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let secret = secret.clone();
                            std::thread::spawn(move || {
                                let _ = handle_sidecar(stream, &secret, "hmac-sha256");
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            });
            let port = ready_rx
                .recv()
                .map_err(|_| "mock sidecar 未就绪".to_string())??;
            embedded = Some((running, handle));
            format!("tcp:127.0.0.1:{port}")
        }
    };
    let mut checks = Vec::new();
    let health = sidecar_roundtrip(&target, &json!({"id": "1", "op": "health"}))?;
    checks.push(json!({
        "name": "health",
        "ok": health["ok"] == true && !health["algo"].as_str().unwrap_or("").is_empty(),
        "detail": health["algo"],
    }));
    let queries = [
        "symbol=BTCUSDT&side=BUY&type=MARKET&quantity=0.001&timestamp=1700000000000",
        "symbol=ETHUSDT&side=SELL&type=STOP_MARKET&triggerPrice=2000&closePosition=true&timestamp=1700000001000",
    ];
    for (index, query) in queries.iter().enumerate() {
        let response = sidecar_roundtrip(
            &target,
            &json!({"id": (index + 2).to_string(), "op": "sign", "query": query}),
        )?;
        let signature = response["signature"].as_str().unwrap_or("");
        let expected = effective_secret
            .as_deref()
            .map(|secret| crate::sign::sign_query(secret, query));
        let signature_ok = expected
            .as_deref()
            .is_none_or(|expected| expected == signature);
        checks.push(json!({
            "name": format!("sign #{}", index + 1),
            "ok": response["ok"] == true && signature_ok && response["query"] == *query,
            "detail": format!("algo={} len={}", response["algo"], signature.len()),
        }));
    }
    let empty = sidecar_roundtrip(&target, &json!({"id": "9", "op": "sign", "query": ""}))?;
    checks.push(json!({
        "name": "空 query 拒绝",
        "ok": empty["ok"] == false,
        "detail": empty["error"],
    }));
    if let Some((running, handle)) = embedded {
        running.store(false, Ordering::Relaxed);
        let _ = handle.join();
    }
    let ok = checks.iter().all(|check| check["ok"] == true);
    Ok(json!({"ok": ok, "target": target, "checks": checks}))
}

// ---------------------------------------------------------------- p99-real

fn decrypt_fernet(key_file: &Path, token: &str) -> Result<String, String> {
    let key =
        std::fs::read_to_string(key_file).map_err(|error| format!("读取主密钥失败: {error}"))?;
    let fernet =
        fernet::Fernet::new(key.trim()).ok_or_else(|| "主密钥不是有效 Fernet 密钥".to_string())?;
    let plain = fernet
        .decrypt(token)
        .map_err(|error| format!("凭证解密失败: {error}"))?;
    String::from_utf8(plain).map_err(|_| "凭证不是 UTF-8 文本".to_string())
}

pub async fn p99_real(
    db_path: &str,
    key_file: &Path,
    orders: usize,
    per_minute: f64,
) -> Result<Value, String> {
    let db = crate::db::Db::open(db_path).map_err(|error| error.to_string())?;
    let data = db
        .setting("data")
        .map_err(|error| error.to_string())?
        .unwrap_or(Value::Null);
    let api_key = data["demo_api_key_enc"]
        .as_str()
        .map(|token| decrypt_fernet(key_file, token))
        .transpose()?;
    let api_secret = data["demo_api_secret_enc"]
        .as_str()
        .map(|token| decrypt_fernet(key_file, token))
        .transpose()?;
    let (Some(api_key), Some(api_secret)) = (api_key, api_secret) else {
        return Err("真实库没有模拟盘凭证（demo_api_key_enc/demo_api_secret_enc）".to_string());
    };
    if api_key.trim().is_empty() || api_secret.trim().is_empty() {
        return Err("模拟盘凭证为空".to_string());
    }
    let base = data["demo_base_url"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("https://testnet.binancefuture.com")
        .trim_end_matches('/')
        .to_string();
    let transport = crate::submit::Transport::new();
    let interval = Duration::from_secs_f64(60.0 / per_minute.max(0.1));
    let mut open_samples = Vec::new();
    let mut close_samples = Vec::new();
    let mut errors = 0usize;
    let mut first_error = String::new();
    for index in 0..orders {
        let params = vec![
            ("symbol".to_string(), "BTCUSDT".to_string()),
            ("side".to_string(), "BUY".to_string()),
            ("type".to_string(), "MARKET".to_string()),
            ("quantity".to_string(), "0.002".to_string()),
            (
                "newClientOrderId".to_string(),
                format!("nqp99{}{:03}", crate::now_millis() % 10_000_000, index),
            ),
        ];

        let request = crate::submit::build_request(
            "POST",
            &base,
            "/fapi/v1/order",
            &params,
            &api_key,
            &api_secret,
            crate::now_millis(),
            5000,
        );
        match send_order(&transport, &request).await {
            Ok(cost) => open_samples.push(cost),
            Err(error) => {
                errors += 1;
                if first_error.is_empty() {
                    first_error = error;
                }
                tokio::time::sleep(interval).await;
                continue;
            }
        }
        let params = vec![
            ("symbol".to_string(), "BTCUSDT".to_string()),
            ("side".to_string(), "SELL".to_string()),
            ("type".to_string(), "MARKET".to_string()),
            ("quantity".to_string(), "0.002".to_string()),
            ("reduceOnly".to_string(), "true".to_string()),
            (
                "newClientOrderId".to_string(),
                format!("nqp99c{}{:03}", crate::now_millis() % 10_000_000, index),
            ),
        ];

        let request = crate::submit::build_request(
            "POST",
            &base,
            "/fapi/v1/order",
            &params,
            &api_key,
            &api_secret,
            crate::now_millis(),
            5000,
        );
        match send_order(&transport, &request).await {
            Ok(cost) => close_samples.push(cost),
            Err(error) => {
                errors += 1;
                if first_error.is_empty() {
                    first_error = error;
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
    Ok(json!({
        "ok": errors == 0,
        "mode": "demo",
        "base": base,
        "orders": orders,
        "errors": errors,
        "first_error": first_error,
        "open": stats("open", &open_samples),
        "close": stats("close", &close_samples),
    }))
}
// ---------------------------------------------------------------- contract-diff

/// 契约对照：解析 P0 基线与 Rust 源码路由，比较方法与被归一化的路径。
pub fn contract_diff(root: &Path, baseline_path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(baseline_path)
        .map_err(|error| format!("读取基线 {} 失败: {error}", baseline_path.display()))?;
    let baseline: Value =
        serde_json::from_str(&text).map_err(|error| format!("基线不是合法 JSON: {error}"))?;
    let endpoints = baseline["endpoints"]
        .as_array()
        .ok_or_else(|| "基线缺少 endpoints 数组".to_string())?;
    let routes = scan_rust_routes(&root.join("rust/nanoquant/src"))?;
    let mut missing = Vec::new();
    let mut baseline_keys = std::collections::HashSet::new();
    for endpoint in endpoints {
        let raw_method = endpoint["method"].as_str().unwrap_or("");
        let method = if raw_method == "WEBSOCKET" {
            "GET".to_string()
        } else {
            raw_method.to_ascii_uppercase()
        };
        let path = endpoint["full_path"]
            .as_str()
            .or_else(|| endpoint["path"].as_str())
            .unwrap_or("");
        let full = if path.starts_with("/api/") {
            path.to_string()
        } else {
            format!("/api/v1{path}")
        };
        let key = normalize_route(&full);
        baseline_keys.insert((method.clone(), key.clone()));
        if !routes
            .iter()
            .any(|(route_method, route_path, _, _)| *route_method == method && *route_path == key)
        {
            missing.push(json!({
                "method": method,
                "path": full,
                "group": endpoint["group"],
                "auth": endpoint["auth"],
            }));
        }
    }
    let extra: Vec<Value> = routes
        .iter()
        .filter(|(method, path, _, _)| !baseline_keys.contains(&(method.clone(), path.clone())))
        .map(|(method, path, file, line)| {
            json!({"method": method, "path": path, "file": file, "line": line})
        })
        .collect();
    Ok(json!({
        "ok": missing.is_empty(),
        "baseline_count": endpoints.len(),
        "baseline_totals": baseline["totals"],
        "rust_route_count": routes.len(),
        "missing_count": missing.len(),
        "extra_count": extra.len(),
        "missing": missing,
        "extra": extra,
        "baseline_sha256": baseline["endpoints_sha256"],
    }))
}

fn normalize_route(path: &str) -> String {
    let mut out = String::new();
    let mut in_brace = false;
    for character in path.chars() {
        match character {
            '{' => in_brace = true,
            '}' => {
                in_brace = false;
                out.push_str("{}");
            }
            _ if in_brace => {}
            _ => out.push(character),
        }
    }
    out
}

fn scan_rust_routes(dir: &Path) -> Result<Vec<(String, String, String, usize)>, String> {
    let mut routes = Vec::new();
    walk_rust_sources(dir, dir, &mut routes)?;
    Ok(routes)
}

fn walk_rust_sources(
    root: &Path,
    dir: &Path,
    routes: &mut Vec<(String, String, String, usize)>,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|error| format!("读取 {} 失败: {error}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|name| name.to_str()) == Some("target") {
                continue;
            }
            walk_rust_sources(root, &path, routes)?;
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
        // 测试模块里的 Router 是夹具，不是对外契约。
        let text = match text.find("mod tests") {
            Some(position) => &text[..position],
            None => text.as_str(),
        };
        let relative = path
            .strip_prefix(root)
            .map(|value| value.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.to_string_lossy().to_string());
        let mut search = 0;
        while let Some(position) = text[search..].find(".route(") {
            let call_start = search + position;
            let open = call_start + ".route".len();
            let Some(close) = matching_paren(&text, open) else {
                break;
            };
            let call = &text[open..=close];
            if let Some(literal) = first_string_literal(call) {
                let full = if literal.starts_with("/api/") {
                    literal
                } else {
                    format!("/api/v1{literal}")
                };
                let normalized = normalize_route(&full);
                let line = text[..call_start].matches('\n').count() + 1;
                for (needle, method) in [
                    ("get(", "GET"),
                    ("post(", "POST"),
                    ("delete(", "DELETE"),
                    ("put(", "PUT"),
                    ("patch(", "PATCH"),
                    ("patch_route(", "PATCH"),
                ] {
                    if call.contains(needle) {
                        routes.push((
                            method.to_string(),
                            normalized.clone(),
                            relative.clone(),
                            line,
                        ));
                    }
                }
            }
            search = close + 1;
        }
    }
    Ok(())
}

fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes.iter().enumerate().skip(open) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn first_string_literal(call: &str) -> Option<String> {
    let start = call.find('"')? + 1;
    let end = call[start..].find('"')? + start;
    Some(call[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_interpolates_and_handles_empty_input() {
        assert_eq!(percentile(&[], 0.99), 0.0);
        let values = [1.0, 2.0, 3.0, 4.0];
        assert!((percentile(&values, 0.5) - 2.5).abs() < 1e-9);
        assert!((percentile(&values, 1.0) - 4.0).abs() < 1e-9);
    }

    #[test]
    fn p99_bench_reports_all_three_segments() {
        let output = p99_bench(50).unwrap();
        assert_eq!(output["ok"], true);
        for name in ["sign", "serialize", "persist"] {
            assert_eq!(output[name]["count"], 50, "{name} 样本数");
            assert!(output[name]["p99"].as_f64().unwrap_or(0.0) > 0.0);
        }
    }

    #[tokio::test]
    async fn bench_exec_completes_against_the_in_process_mock_exchange() {
        let output = bench_exec(10, 0).await.unwrap();
        assert_eq!(output["ok"], true, "不应有失败单: {output}");
        assert_eq!(output["errors"], 0);
        assert_eq!(output["open"]["count"], 10);
        assert_eq!(output["close"]["count"], 10);
    }

    #[test]
    fn verify_sidecar_passes_against_the_embedded_mock() {
        let output = verify_sidecar(None, None).unwrap();
        assert_eq!(output["ok"], true, "{output}");
        let checks = output["checks"].as_array().unwrap();
        assert!(checks.len() >= 4);
        assert!(checks.iter().all(|check| check["ok"] == true));
    }

    #[test]
    fn mock_exchange_stops_when_the_flag_clears() {
        let running = Arc::new(AtomicBool::new(true));
        let stop = running.clone();
        let handle = std::thread::spawn(move || mock_exchange(0, 0, stop).unwrap());
        std::thread::sleep(Duration::from_millis(100));
        running.store(false, Ordering::Relaxed);
        let output = handle.join().unwrap();
        assert!(output["port"].as_u64().unwrap_or(0) > 0);
    }

    #[test]
    fn mock_sidecar_rejects_an_empty_secret() {
        let running = Arc::new(AtomicBool::new(false));
        assert!(mock_exec_sidecar(0, "", running).is_err());
    }

    #[test]
    fn contract_diff_reports_missing_paths_and_extra_routes() {
        let dir = std::env::temp_dir().join(format!("nq-contract-{}", crate::now_millis()));
        let src = dir.join("rust/nanoquant/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("api.rs"),
            "Router::new()\n    .route(\"/health\", get(health))\n    .route(\"/thing/{id}\", post(thing).delete(drop_thing));\n",
        )
        .unwrap();
        let baseline = dir.join("baseline.json");
        std::fs::write(
            &baseline,
            json!({"endpoints": [
                {"method": "GET", "full_path": "/api/v1/health", "group": "auth", "auth": "public"},
                {"method": "POST", "full_path": "/api/v1/thing/{thing_id}", "group": "x", "auth": "user"},
                {"method": "GET", "full_path": "/api/v1/missing", "group": "x", "auth": "user"}
            ]})
            .to_string(),
        )
        .unwrap();
        let value = contract_diff(&dir, &baseline).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["missing_count"], 1);
        assert_eq!(value["missing"][0]["path"], "/api/v1/missing");
        assert_eq!(value["extra_count"], 1, "DELETE 路由不在基线中");
        assert_eq!(value["extra"][0]["method"], "DELETE");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
