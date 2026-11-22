//! NanoQuant 执行内核 exec-core v1.0（默认组件，非可选）。
//!
//! 职责：签名校验（sign）+ 七段延迟指标聚合（metrics_*）+ 长连接服务端。
//! 二期预留 order_open / order_close / protection_place（本期收到即报 reserved）。
//!
//! 运行（Linux，UDS）：EXEC_SECRET=... EXEC_SOCKET=/run/nanoquant/exec.sock ./exec-core
//! 运行（Windows 联调）：EXEC_SECRET=... EXEC_TCP=127.0.0.1:9101 ./exec-core
//! 协议见 docs/EXEC_PROTOCOL.md（v1.0）。

mod metrics;
mod model;
mod sign;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;

use metrics::MetricsHub;
use model::{Request, Response};
use sign::Signer;

/// 连接空闲超时：主服务心跳 5s 一帧 health，30s 无帧视为死连接。
const IDLE_TIMEOUT_SECS: u64 = 30;

struct Sessions(AtomicU64);

impl Sessions {
    fn new() -> Self {
        Self(AtomicU64::new(0))
    }
    fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn dec(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
    fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg_attr(not(unix), allow(dead_code))]
struct Config {
    socket: PathBuf,
    secret: String,
    tcp: Option<String>,
    metrics_capacity: usize,
}

impl Config {
    /// 缺 EXEC_SECRET 返回 None（退出码 78 = EX_CONFIG，防 systemd 无限重启），沿用 exec-service 约定。
    fn from_env() -> Option<Self> {
        let secret = std::env::var("EXEC_SECRET").unwrap_or_default();
        if secret.trim().is_empty() {
            return None;
        }
        Some(Self {
            socket: std::env::var("EXEC_SOCKET")
                .unwrap_or_else(|_| "/run/nanoquant/exec.sock".into())
                .into(),
            secret,
            tcp: std::env::var("EXEC_TCP").ok().filter(|s| !s.trim().is_empty()),
            metrics_capacity: std::env::var("EXEC_METRICS_CAPACITY")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|v| *v > 0)
                .unwrap_or(2000),
        })
    }
}

async fn handle<S>(
    stream: S,
    signer: Arc<Signer>,
    hub: Arc<MetricsHub>,
    sessions: Arc<Sessions>,
    started: Instant,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    sessions.inc();
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut lines = BufReader::new(read_half).lines();
    let idle = std::time::Duration::from_secs(IDLE_TIMEOUT_SECS);
    loop {
        let line = match tokio::time::timeout(idle, lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            _ => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(req, &signer, &hub, &sessions, started).await,
            Err(exc) => Response::err("unknown", format!("bad json: {exc}")),
        };
        let mut payload = serde_json::to_string(&resp).unwrap_or_default();
        payload.push('\n');
        if write_half.write_all(payload.as_bytes()).await.is_err() {
            break;
        }
        let _ = write_half.flush().await;
    }
    sessions.dec();
}

async fn dispatch(
    req: Request,
    signer: &Signer,
    hub: &MetricsHub,
    sessions: &Sessions,
    started: Instant,
) -> Response {
    match req.op.as_str() {
        "hello" => {
            let mut resp = Response::ok(&req.id);
            resp.kernel_version = Some(env!("CARGO_PKG_VERSION").to_string());
            resp.capabilities = Some(vec![
                "health".into(),
                "sign".into(),
                "metrics_ingest".into(),
                "metrics_query".into(),
                "metrics_reset".into(),
            ]);
            resp.metrics_capacity = Some(hub.capacity());
            resp
        }
        "health" => {
            let mut resp = Response::ok(&req.id);
            resp.algo = Some(signer.algo().to_string());
            resp.uptime_ms = Some(started.elapsed().as_millis() as u64);
            resp.sessions = Some(sessions.get());
            resp
        }
        "sign" => {
            if req.query.trim().is_empty() {
                return Response::err(&req.id, "empty query");
            }
            let mut resp = Response::ok(&req.id);
            resp.signature = Some(signer.sign(&req.query));
            resp.algo = Some(signer.algo().to_string());
            resp.query = Some(req.query);
            resp
        }
        "metrics_ingest" => {
            let (accepted, dropped) = hub.ingest_batch(&req.samples);
            let mut resp = Response::ok(&req.id);
            resp.accepted = Some(accepted);
            resp.dropped = Some(dropped);
            resp
        }
        "metrics_query" => {
            let mut resp = Response::ok(&req.id);
            resp.segments = Some(hub.view());
            resp
        }
        "metrics_reset" => {
            hub.reset();
            Response::ok(&req.id)
        }
        "order_open" | "order_close" | "protection_place" => {
            Response::err(&req.id, "reserved for phase 2")
        }
        other => Response::err(&req.id, format!("unknown op: {other}")),
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let Some(cfg) = Config::from_env() else {
        eprintln!(
            "exec-core: EXEC_SECRET 未配置，服务不启动。\n\
             请先填好交易所 API 凭证，再执行：sudo bash scripts/setup_core.sh --from-settings"
        );
        std::process::exit(78);
    };
    let signer = Arc::new(Signer::detect(&cfg.secret));
    let hub = Arc::new(MetricsHub::new(cfg.metrics_capacity));
    let sessions = Arc::new(Sessions::new());
    let started = Instant::now();
    eprintln!(
        "exec-core v{} starting (algo={}, metrics_capacity={})",
        env!("CARGO_PKG_VERSION"),
        signer.algo(),
        cfg.metrics_capacity
    );

    if let Some(addr) = cfg.tcp.as_deref() {
        let listener = TcpListener::bind(addr).await?;
        eprintln!("exec-core tcp listening on {addr}");
        let (s, h, ss) = (signer.clone(), hub.clone(), sessions.clone());
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let (s, h, ss) = (s.clone(), h.clone(), ss.clone());
                        tokio::spawn(async move { handle(stream, s, h, ss, started).await });
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
        eprintln!("exec-core uds listening on {:?}", cfg.socket);
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
                    let (s, h, ss) = (signer.clone(), hub.clone(), sessions.clone());
                    tokio::spawn(async move { handle(stream, s, h, ss, started).await });
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
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::MetricsHub;
    use crate::model::{Request, SampleIn};
    use crate::sign::Signer;

    fn hub() -> (
        std::sync::Arc<Signer>,
        std::sync::Arc<MetricsHub>,
        Sessions,
        std::time::Instant,
    ) {
        (
            std::sync::Arc::new(Signer::detect("secret")),
            std::sync::Arc::new(MetricsHub::new(8)),
            Sessions::new(),
            std::time::Instant::now(),
        )
    }

    #[tokio::test]
    async fn hello_reports_capabilities() {
        let (signer, metrics, sessions, started) = hub();
        let resp = dispatch(
            Request {
                id: "1".into(),
                op: "hello".into(),
                version: "0.9".into(),
                query: String::new(),
                samples: vec![],
            },
            &signer,
            &metrics,
            &sessions,
            started,
        )
        .await;
        assert!(resp.ok);
        assert_eq!(resp.kernel_version.as_deref(), Some("1.0.0"));
        assert!(resp
            .capabilities
            .as_deref()
            .unwrap()
            .contains(&"sign".to_string()));
        assert_eq!(resp.metrics_capacity, Some(8));
    }

    #[tokio::test]
    async fn health_reports_algo_and_sessions() {
        let (signer, metrics, sessions, started) = hub();
        let resp = dispatch(
            Request {
                id: "2".into(),
                op: "health".into(),
                version: String::new(),
                query: String::new(),
                samples: vec![],
            },
            &signer,
            &metrics,
            &sessions,
            started,
        )
        .await;
        assert!(resp.ok);
        assert_eq!(resp.algo.as_deref(), Some("hmac-sha256"));
        assert!(resp.uptime_ms.is_some());
        assert_eq!(resp.sessions, Some(0));
    }

    #[tokio::test]
    async fn sign_roundtrip_and_empty_query_rejected() {
        let (signer, metrics, sessions, started) = hub();
        let resp = dispatch(
            Request {
                id: "3".into(),
                op: "sign".into(),
                version: String::new(),
                query: "symbol=BTCUSDT".into(),
                samples: vec![],
            },
            &signer,
            &metrics,
            &sessions,
            started,
        )
        .await;
        assert!(resp.ok);
        assert_eq!(resp.query.as_deref(), Some("symbol=BTCUSDT"));
        assert!(resp.signature.is_some());

        let resp = dispatch(
            Request {
                id: "4".into(),
                op: "sign".into(),
                version: String::new(),
                query: String::new(),
                samples: vec![],
            },
            &signer,
            &metrics,
            &sessions,
            started,
        )
        .await;
        assert!(!resp.ok);
    }

    #[tokio::test]
    async fn metrics_ingest_query_reset_cycle() {
        let (signer, metrics, sessions, started) = hub();
        let resp = dispatch(
            Request {
                id: "5".into(),
                op: "metrics_ingest".into(),
                version: String::new(),
                query: String::new(),
                samples: vec![SampleIn { segment: "http".into(), ms: 12.5, ts: 1 }],
            },
            &signer,
            &metrics,
            &sessions,
            started,
        )
        .await;
        assert!(resp.ok);
        assert_eq!(resp.accepted, Some(1));
        assert_eq!(resp.dropped, Some(0));

        let resp = dispatch(
            Request {
                id: "6".into(),
                op: "metrics_query".into(),
                version: String::new(),
                query: String::new(),
                samples: vec![],
            },
            &signer,
            &metrics,
            &sessions,
            started,
        )
        .await;
        assert!(resp.ok);
        assert_eq!(resp.segments.unwrap()["http"]["count"], serde_json::json!(1));

        let resp = dispatch(
            Request {
                id: "7".into(),
                op: "metrics_reset".into(),
                version: String::new(),
                query: String::new(),
                samples: vec![],
            },
            &signer,
            &metrics,
            &sessions,
            started,
        )
        .await;
        assert!(resp.ok);
    }

    #[tokio::test]
    async fn phase2_ops_reserved_and_unknown_op() {
        let (signer, metrics, sessions, started) = hub();
        for op in ["order_open", "order_close", "protection_place"] {
            let resp = dispatch(
                Request {
                    id: "8".into(),
                    op: op.into(),
                    version: String::new(),
                    query: String::new(),
                    samples: vec![],
                },
                &signer,
                &metrics,
                &sessions,
                started,
            )
            .await;
            assert!(!resp.ok);
            assert_eq!(resp.error.as_deref(), Some("reserved for phase 2"));
        }
        let resp = dispatch(
            Request {
                id: "9".into(),
                op: "wat".into(),
                version: String::new(),
                query: String::new(),
                samples: vec![],
            },
            &signer,
            &metrics,
            &sessions,
            started,
        )
        .await;
        assert!(!resp.ok);
    }
}