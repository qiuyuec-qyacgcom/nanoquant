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
mod protection;
mod sign;
mod store;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;

use http::HttpClient;
use model::{Request, Response};
use order::{OrderBook, OrderState};
use protection::{build_pair, LegSpec, ProtectKind};
use sign::Signer;
use store::{LedgerRow, Store};
use tokio::sync::Mutex;

/// 当前 Unix 秒（台账时间戳）。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg_attr(not(unix), allow(dead_code))]
struct Config {
    socket: PathBuf,
    secret: String,
    tcp: Option<String>,
    http: http::HttpConfig,
    /// P3-4：台账落库路径；空 = 纯内存（与 P3-2 行为一致）
    ledger_path: String,
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
            ledger_path: std::env::var("EXEC_LEDGER_DB")
                .unwrap_or_default()
                .trim()
                .to_string(),
        }
    }
}

async fn handle<S>(
    stream: S,
    signer: Arc<Signer>,
    http: Arc<HttpClient>,
    book: Arc<Mutex<OrderBook>>,
    store: Arc<Store>,
) where
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
            Ok(req) => dispatch(req, &signer, &http, &book, &store).await,
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

async fn dispatch(
    req: Request,
    signer: &Signer,
    http: &HttpClient,
    book: &Mutex<OrderBook>,
    store: &Store,
) -> Response {
    match req.op.as_str() {
        "health" => {
            let mut resp = Response::ok(&req.id);
            resp.algo = Some(signer.algo().to_string());
            // P3-1：附带连接池状态（旧调用方忽略该字段即可）
            resp.http = Some(http.status());
            // P3-4：附带台账落库状态
            resp.store = Some(store.status());
            resp
        }
        // P3-4：台账落库统计（只读）
        "store_status" => {
            let mut resp = Response::ok(&req.id);
            resp.store = Some(store.status());
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
            // P3-4：异步落库（入队即返回，满队列丢弃不影响台账内存态）
            store.submit(LedgerRow {
                client_order_id: record.client_order_id.clone(),
                symbol: record.symbol.clone(),
                state: record.state.as_str().to_string(),
                filled_qty: record.filled_qty,
                avg_price: record.avg_price,
                ts: now_secs(),
            });
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
                    // P3-4：迁移成功也落库（upsert + 历史）
                    store.submit(LedgerRow {
                        client_order_id: record.client_order_id.clone(),
                        symbol: record.symbol.clone(),
                        state: record.state.as_str().to_string(),
                        filled_qty: record.filled_qty,
                        avg_price: record.avg_price,
                        ts: now_secs(),
                    });
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
        // P3-3：并行构造 STOP + TP 两笔保护单（只签名，不发送），供 Python 逐字节比对
        "protection_plan" => {
            if req.symbol.trim().is_empty() || req.close_side.trim().is_empty() {
                return Response::err(&req.id, "symbol / close_side 不能为空");
            }
            let ps = req.position_side.trim();
            let position_side = if ps.is_empty() { None } else { Some(ps) };
            let stop_id = if req.stop_client_id.trim().is_empty() {
                "np_stop".to_string()
            } else {
                req.stop_client_id.trim().to_string()
            };
            let take_id = if req.take_client_id.trim().is_empty() {
                "np_take".to_string()
            } else {
                req.take_client_id.trim().to_string()
            };
            let (stop, take) = build_pair(
                signer,
                LegSpec {
                    symbol: &req.symbol,
                    close_side: &req.close_side,
                    kind: ProtectKind::StopMarket,
                    trigger_price: &req.stop_price,
                    client_algo_id: &stop_id,
                    position_side,
                },
                LegSpec {
                    symbol: &req.symbol,
                    close_side: &req.close_side,
                    kind: ProtectKind::TakeProfitMarket,
                    trigger_price: &req.take_price,
                    client_algo_id: &take_id,
                    position_side,
                },
            )
            .await;
            let mut resp = Response::ok(&req.id);
            resp.protections = Some(serde_json::json!([stop, take]));
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
        // P3-3 收尾：真正下发保护单（**默认关闭**，需 EXEC_PLACE_MODE=enabled）
        "protection_place" => {
            let mode = std::env::var("EXEC_PLACE_MODE")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            if mode != "enabled" {
                return Response::err(
                    &req.id,
                    format!("sidecar 代发未启用（EXEC_PLACE_MODE={mode:?}，需 enabled）"),
                );
            }
            if req.symbol.trim().is_empty() || req.close_side.trim().is_empty() {
                return Response::err(&req.id, "symbol / close_side 不能为空");
            }
            let ps = req.position_side.trim();
            let position_side = if ps.is_empty() { None } else { Some(ps) };
            let stop_id = if req.stop_client_id.trim().is_empty() {
                "np_stop".to_string()
            } else {
                req.stop_client_id.trim().to_string()
            };
            let take_id = if req.take_client_id.trim().is_empty() {
                "np_take".to_string()
            } else {
                req.take_client_id.trim().to_string()
            };
            let (stop_req, take_req) = build_pair(
                signer,
                LegSpec {
                    symbol: &req.symbol,
                    close_side: &req.close_side,
                    kind: ProtectKind::StopMarket,
                    trigger_price: &req.stop_price,
                    client_algo_id: &stop_id,
                    position_side,
                },
                LegSpec {
                    symbol: &req.symbol,
                    close_side: &req.close_side,
                    kind: ProtectKind::TakeProfitMarket,
                    trigger_price: &req.take_price,
                    client_algo_id: &take_id,
                    position_side,
                },
            )
            .await;

            // 两笔并行发送；任一「结果未知」都必须如实上报，调用方禁止重发
            let (sr, tr) = tokio::join!(
                http.post_signed("/fapi/v1/algoOrder", &stop_req.query, &stop_req.signature, &req.api_key),
                http.post_signed("/fapi/v1/algoOrder", &take_req.query, &take_req.signature, &req.api_key)
            );
            let mut results = Vec::new();
            let mut unknown = false;
            let mut failed = false;
            for (kind, r) in [("STOP_MARKET", sr), ("TAKE_PROFIT_MARKET", tr)] {
                match r {
                    Ok(body) => results.push(serde_json::json!({
                        "kind": kind, "ok": true, "unknown": false, "body": body
                    })),
                    Err(exc) => {
                        if exc.is_unknown() {
                            unknown = true;
                        } else {
                            failed = true;
                        }
                        results.push(serde_json::json!({
                            "kind": kind, "ok": false,
                            "unknown": exc.is_unknown(),
                            "error": exc.message()
                        }));
                    }
                }
            }
            let mut resp = Response::ok(&req.id);
            if unknown || failed {
                resp.ok = false;
                // unknown=true 时调用方**不得**回退重发
                resp.error = Some(if unknown {
                    "保护单结果未知（禁止重发）；请用交易所持仓/挂单核对".to_string()
                } else {
                    "保护单被交易所拒绝".to_string()
                });
            }
            resp.protections = Some(serde_json::json!(results));
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
    // P3-4：台账落库（单写线程 + 批量事务）。路径为空则纯内存，行为同 P3-2。
    let ledger_store = Arc::new(Store::open(&cfg.ledger_path));
    if ledger_store.enabled() {
        eprintln!("exec-service ledger store: {}", cfg.ledger_path);
    }

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
        let store_tcp = ledger_store.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let s = signer_tcp.clone();
                        let h = http_tcp.clone();
                        let b = book_tcp.clone();
                        let st = store_tcp.clone();
                        tokio::spawn(async move { handle(stream, s, h, b, st).await });
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
                    let st = ledger_store.clone();
                    tokio::spawn(async move { handle(stream, s, h, b, st).await });
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
