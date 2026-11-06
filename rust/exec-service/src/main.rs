//! NanoQuant 执行内核 sidecar（P2：影子模式）
//!
//! 阶段目标：**只签名、不发单**。Python 侧把同一批 intent 同时送给 Rust，
//! 逐字节比对 `query` 与 `signature`，一致后再进入真实下单。
//!
//! 运行：
//!     EXEC_SECRET="<base64 PKCS#8 或 HMAC secret>" \
//!     EXEC_SOCKET=/run/nanoquant/exec.sock \
//!     ./exec-service
//!
//! 协议见 docs/EXEC_PROTOCOL.md。

mod model;
mod sign;

use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};

use model::{Request, Response};
use sign::Signer;

struct Config {
    socket: PathBuf,
    secret: String,
}

impl Config {
    fn from_env() -> Self {
        Self {
            socket: std::env::var("EXEC_SOCKET").unwrap_or_else(|_| "/tmp/nanoquant-exec.sock".into()).into(),
            secret: std::env::var("EXEC_SECRET").expect("EXEC_SECRET 未设置"),
        }
    }
}

async fn handle<S>(stream: S, signer: std::sync::Arc<Signer>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = stream.split();
    let mut lines = BufReader::new(read_half).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(req, &signer),
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

fn dispatch(req: Request, signer: &Signer) -> Response {
    match req.op.as_str() {
        "health" => {
            let mut resp = Response::ok(&req.id);
            resp.algo = Some(signer.algo().to_string());
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
        other => Response::err(&req.id, format!("unknown op: {other}")),
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let cfg = Config::from_env();
    let signer = std::sync::Arc::new(Signer::detect(&cfg.secret));

    if cfg.socket.exists() {
        let _ = std::fs::remove_file(&cfg.socket);
    }
    if let Some(dir) = cfg.socket.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    let listener = UnixListener::bind(&cfg.socket)?;
    eprintln!("exec-service listening on {:?} (algo={})", cfg.socket, signer.algo());

    // Windows 开发机没有 asyncio 的 unix socket, 用回环 TCP 验证同一套协议
    if let Ok(addr) = std::env::var("EXEC_TCP") {
        let signer_tcp = signer.clone();
        tokio::spawn(async move {
            match TcpListener::bind(&addr).await {
                Ok(tcp) => {
                    eprintln!("exec-service also listening on tcp {addr}");
                    loop {
                        if let Ok((stream, _)) = tcp.accept().await {
                            let s = signer_tcp.clone();
                            tokio::spawn(async move { handle(stream, s).await });
                        }
                    }
                }
                Err(exc) => eprintln!("tcp bind {addr} failed: {exc}"),
            }
        });
    }

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
                let signer = signer.clone();
                tokio::spawn(async move { handle(stream, signer).await });
            }
            Err(exc) => eprintln!("accept failed: {exc}"),
        }
    }
}
