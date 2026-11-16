//! NanoQuant 执行内核 sidecar（P2：影子模式）
//!
//! 阶段目标：**只签名、不发单**。Python 侧把同一批 intent 同时送给 Rust，
//! 逐字节比对 `query` 与 `signature`，一致后再进入真实下单。
//!
//! 运行（Linux / macOS，UDS）：
//!     EXEC_SECRET="<base64 PKCS#8 或 HMAC secret>" \
//!     EXEC_SOCKET=/run/nanoquant/exec.sock \
//!     ./exec-service
//!
//! 运行（Windows 开发机 / 容器联调，回环 TCP；也可与 UDS 并存）：
//!     EXEC_SECRET=... EXEC_TCP=127.0.0.1:9101 ./exec-service
//!
//! 协议见 docs/EXEC_PROTOCOL.md。

mod http;
mod model;
mod order;
mod sign;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;

use http::HttpClient;
use model::{Request, Response};
use order::{OrderBook, OrderState};
use sign::Signer;
use tokio::sync::Mutex;

#[cfg_attr(not(unix), allow(dead_code))]
struct Config {
    socket: PathBuf,
    secret: String,
    tcp: Option<String>,
    http: http::HttpConfig,
}

impl Config {
    fn from_env() -> Self {
        Self {
            socket: std::env::var("EXEC_SOCKET")
                .unwrap_or_else(|_| "/tmp/nanoquant-exec.sock".into())
                .into(),
            secret: std::env::var("EXEC_SECRET").expect("EXEC_SECRET 未设置"),
            tcp: std::env::var("EXEC_TCP").ok().filter(|s| !s.trim().is_empty()),
            http: http::HttpConfig::from_env(),
        }
    }
}

async fn handle<S>(stream: S, signer: Arc<Signer>, http: Arc<HttpClient>, book: Arc<Mutex<OrderBook>>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(req, &signer, &http, &book).await,
            Err(exc) => Response::err("unknown", format!("bad json: {exc}")),
        };
        let mut payload = serde_json::to_string(&resp).unwrap_or_default();
        payload.push('\n');
        if write_half.write_all(payload.as_bytes()).await.is_err() {
            break;
        }
        let _ = write_half.flush().await;
    }
}

async fn dispatch(req: Request, signer: &Signer, http: &HttpClient, book: &Mutex<OrderBook>) -> Response {
    match req.op.as_str() {
        "health" => {
            let mut resp = Response::ok(&req.id);
            resp.algo = Some(signer.algo().to_string());
            // P3-1：附带连接池状态（旧调用方忽略该字段即可）
            resp.http = Some(http.status());
            resp
        }
        // P3-2：订单台账（幂等登记 / 状态迁移 / 查询 / 快照），全程不发单
        "order_register" => {
            if req.client_order_id.trim().is_empty() {
                return Response::err(&req.id, "client_order_id 不能为空");
            }
            if req.symbol.trim().is_empty() {
                return Response::err(&req.id, "symbol 不能为空");
            }
            let initial = if req.state.trim().is_empty() {
                OrderState::Planned
            } else {
                match OrderState::parse(&req.state) {
                    Some(s) => s,
                    None => return Response::err(&req.id, format!("未知状态: {}", req.state)),
                }
            };
            let mut guard = book.lock().await;
            let (record, dup) = guard.register(&req.client_order_id, &req.symbol, initial);
            let mut resp = Response::ok(&req.id);
            resp.order = Some(record.to_json());
            resp.duplicate = Some(dup);
            resp
        }
        "order_transition" => {
            let Some(to) = OrderState::parse(&req.state) else {
                return Response::err(&req.id, format!("未知状态: {}", req.state));
            };
            let mut guard = book.lock().await;
            match guard.transition(&req.client_order_id, to, req.filled_qty, req.avg_price) {
                Ok(record) => {
                    let mut resp = Response::ok(&req.id);
                    resp.order = Some(record.to_json());
                    resp
                }
                Err(exc) => Response::err(&req.id, exc.0),
            }
        }
        "order_get" => {
            let guard = book.lock().await;
            match guard.get(&req.client_order_id) {
                Some(record) => {
                    let mut resp = Response::ok(&req.id);
                    resp.order = Some(record.to_json());
                    resp
                }
                None => Response::err(&req.id, format!("未知订单: {}", req.client_order_id)),
            }
        }
        "order_snapshot" => {
            let guard = book.lock().await;
            let mut resp = Response::ok(&req.id);
            resp.order = Some(guard.snapshot());
            resp
        }
        // 影子模式：签名并回显，供 Python 侧逐字节比对
        "plan" | "sign" => {
            if req.query.is_empty() {
                return Response::err(&req.id, "query 不能为空");
            }
            let mut resp = Response::ok(&req.id);
            resp.signature = Some(signer.sign(&req.query));
            resp.algo = Some(signer.algo().to_string());
            resp.query = Some(req.query);
            resp
        }
        // P3-1：只读探活连接池（GET /fapi/v1/time），不签名、不下单
        "probe" => {
            let mut resp = Response::ok(&req.id);
            resp.algo = Some(signer.algo().to_string());
            match http.probe().await {
                Ok(ms) => {
                    resp.latency_ms = Some(ms);
                }
                Err(exc) => {
                    resp.ok = false;
                    resp.error = Some(exc);
                }
            }
            resp.http = Some(http.status());
            resp
        }
        other => Response::err(&req.id, format!("unknown op: {other}")),
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cfg = Config::from_env();
    let signer = Arc::new(Signer::detect(&cfg.secret));
    let http = Arc::new(HttpClient::new(cfg.http.clone()));
    // P3-2：订单台账（跨连接共享，进程内唯一权威）
    let book = Arc::new(Mutex::new(OrderBook::new()));

    // P3-1：后台预热连接池（best-effort，失败不影响签名服务）
    if cfg.http.enabled() {
        eprintln!("exec-service http pool enabled: base={}", cfg.http.base_url);
        let warm = http.clone();
        tokio::spawn(async move { warm.warmup().await });
    }

    // 回环 TCP：Windows 无 UDS，容器/联调也用这条；与 UDS 可并存
    if let Some(addr) = cfg.tcp.as_deref() {
        let listener = TcpListener::bind(addr).await?;
        eprintln!("exec-service tcp listening on {addr} (algo={})", signer.algo());
        let signer_tcp = signer.clone();
        let http_tcp = http.clone();
        let book_tcp = book.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let s = signer_tcp.clone();
                        let h = http_tcp.clone();
                        let b = book_tcp.clone();
                        tokio::spawn(async move { handle(stream, s, h, b).await });
                    }
                    Err(exc) => eprintln!("tcp accept failed: {exc}"),
                }
            }
        });
    }

    #[cfg(unix)]
    {
        if cfg.socket.exists() {
            let _ = std::fs::remove_file(&cfg.socket);
        }
        if let Some(dir) = cfg.socket.parent() {
            let _ = std::fs::create_dir_all(dir);
        }

        let listener = UnixListener::bind(&cfg.socket)?;
        eprintln!(
            "exec-service uds listening on {:?} (algo={})",
            cfg.socket,
            signer.algo()
        );

        tokio::spawn({
            let socket = cfg.socket.clone();
            async move {
                let _ = tokio::signal::ctrl_c().await;
                let _ = std::fs::remove_file(socket);
                std::process::exit(0);
            }
        });

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let s = signer.clone();
                    let h = http.clone();
                    let b = book.clone();
                    tokio::spawn(async move { handle(stream, s, h, b).await });
                }
                Err(exc) => eprintln!("uds accept failed: {exc}"),
            }
        }
    }

    #[cfg(not(unix))]
    {
        if cfg.tcp.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Windows 无 UDS 支持：请设置 EXEC_TCP=127.0.0.1:9101",
            ));
        }
        // TCP 监听已在后台任务里跑，这里只保持进程存活
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }
}
