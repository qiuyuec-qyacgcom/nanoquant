//! 执行内核 exec-core 长连接客户端：握手 / 心跳 / 指数退避重连 / 七段样本上报。
//! 统一替换此前三份 JSON Lines 客户端（探活 / verify / mock）。
//! 上报队列有界 8192，满则丢弃 + 计数，绝不阻塞下单路径。

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::mpsc;

pub const SEGMENTS: [&str; 7] = ["total", "plan", "sign", "http", "wait", "protect", "persist"];
const QUEUE_BOUND: usize = 8192;
const BATCH_MAX: usize = 64;
const BATCH_FLUSH: Duration = Duration::from_millis(200);
const HEARTBEAT: Duration = Duration::from_secs(5);
const RECONNECT_BASE: Duration = Duration::from_millis(250);
const RECONNECT_CAP: Duration = Duration::from_secs(8);
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(800);

static GLOBAL: std::sync::OnceLock<Arc<ExecClient>> = std::sync::OnceLock::new();

/// 主服务进程内唯一的执行内核客户端（供 engine 等只持有 Db 的路由读取）。
pub fn install(client: Arc<ExecClient>) {
    let _ = GLOBAL.set(client);
}

pub fn global() -> Option<Arc<ExecClient>> {
    GLOBAL.get().cloned()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnState {
    Connected,
    Reconnecting,
    Disconnected,
}

#[derive(Clone)]
pub struct Sample {
    pub segment: &'static str,
    pub ms: f64,
}

type ReadHalf = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
type WriteHalf = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;

struct Connection {
    reader: BufReader<ReadHalf>,
    writer: BufWriter<WriteHalf>,
}

impl Connection {
    async fn open(endpoint: &str) -> Result<Self, String> {
        if let Some(address) = endpoint.strip_prefix("tcp:") {
            let stream = tokio::net::TcpStream::connect(address)
                .await
                .map_err(|e| format!("连接执行内核失败: {e}"))?;
            let (reader, writer) = stream.into_split();
            return Ok(Self {
                reader: BufReader::new(Box::new(reader)),
                writer: BufWriter::new(Box::new(writer)),
            });
        }
        if let Some(path) = endpoint.strip_prefix("unix:") {
            #[cfg(unix)]
            {
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .map_err(|e| format!("连接执行内核失败: {e}"))?;
                let (reader, writer) = stream.into_split();
                return Ok(Self {
                    reader: BufReader::new(Box::new(reader)),
                    writer: BufWriter::new(Box::new(writer)),
                });
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                return Err("Windows 不支持 unix socket，请使用 tcp: 端点".to_string());
            }
        }
        Err(format!("无法识别的执行内核端点: {endpoint}"))
    }

    /// 一请求一响应（id 关联校验）。低频操作（探活/查询/上报批），串行足够。
    async fn roundtrip(&mut self, req: &Value) -> Result<Value, String> {
        let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("写入执行内核失败: {e}"))?;
        self.writer
            .flush()
            .await
            .map_err(|e| format!("刷新执行内核连接失败: {e}"))?;
        let mut resp = String::new();
        let read = tokio::time::timeout(RESPONSE_TIMEOUT, self.reader.read_line(&mut resp))
            .await
            .map_err(|_| "执行内核响应超时".to_string())?
            .map_err(|e| format!("读取执行内核失败: {e}"))?;
        if read == 0 {
            return Err("执行内核连接关闭".into());
        }
        let value: Value = serde_json::from_str(&resp).map_err(|e| e.to_string())?;
        if value["id"] != req["id"] {
            return Err("执行内核响应 id 不匹配".into());
        }
        Ok(value)
    }
}

pub struct ExecClient {
    endpoint: String,
    conn: RwLock<Option<Arc<tokio::sync::Mutex<Connection>>>>,
    state: RwLock<ConnState>,
    last_error: Mutex<Option<String>>,
    tx: mpsc::Sender<Sample>,
    rx: Mutex<Option<mpsc::Receiver<Sample>>>,
    dropped: AtomicU64,
    pub connected_at: Mutex<Option<Instant>>,
    next_id: AtomicU64,
}

impl ExecClient {
    pub fn new(endpoint: &str) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(QUEUE_BOUND);
        Arc::new(Self {
            endpoint: endpoint.to_string(),
            conn: RwLock::new(None),
            state: RwLock::new(ConnState::Disconnected),
            last_error: Mutex::new(None),
            tx,
            rx: Mutex::new(Some(rx)),
            dropped: AtomicU64::new(0),
            connected_at: Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    fn set_state(&self, state: ConnState) {
        let mut guard = self.state.write().unwrap_or_else(|e| e.into_inner());
        *guard = state;
    }

    fn set_error(&self, error: String) {
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = Some(error);
        }
    }

    fn next_request_id(&self) -> String {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("nq-{id}")
    }

    fn current_conn(&self) -> Option<Arc<tokio::sync::Mutex<Connection>>> {
        self.conn
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn clear_conn(&self) {
        let mut guard = self.conn.write().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }

    pub async fn handshake(&self) -> Result<Value, String> {
        let mut conn = match Connection::open(&self.endpoint).await {
            Ok(conn) => conn,
            Err(error) => {
                self.set_error(error.clone());
                self.set_state(ConnState::Disconnected);
                return Err(error);
            }
        };
        let request = json!({
            "id": self.next_request_id(),
            "op": "hello",
            "version": env!("CARGO_PKG_VERSION"),
        });
        let response = match conn.roundtrip(&request).await {
            Ok(response) => response,
            Err(error) => {
                self.set_error(error.clone());
                self.set_state(ConnState::Disconnected);
                return Err(error);
            }
        };
        if response["ok"] != true {
            let error = response["error"]
                .as_str()
                .unwrap_or("执行内核握手失败")
                .to_string();
            self.set_error(error.clone());
            self.set_state(ConnState::Disconnected);
            return Err(error);
        }
        if response["kernel_version"].as_str().unwrap_or("").is_empty() {
            let error = "执行内核握手失败：kernel_version 为空".to_string();
            self.set_error(error.clone());
            self.set_state(ConnState::Disconnected);
            return Err(error);
        }
        {
            let mut slot = self.conn.write().unwrap_or_else(|e| e.into_inner());
            *slot = Some(Arc::new(tokio::sync::Mutex::new(conn)));
        }
        self.set_state(ConnState::Connected);
        if let Ok(mut slot) = self.last_error.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.connected_at.lock() {
            *slot = Some(Instant::now());
        }
        Ok(response)
    }

    /// 启动心跳维护：每 5s 一次 health；失败后进入指数退避重连（250ms 起，上限 8s）。
    pub fn spawn_maintainer(self: &Arc<Self>) {
        let client = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT).await;
                if client.state() != ConnState::Connected {
                    client.reconnect().await;
                    continue;
                }
                if let Err(error) = client.probe().await {
                    client.set_error(error);
                    client.set_state(ConnState::Reconnecting);
                    client.reconnect().await;
                }
            }
        });
    }

    async fn reconnect(&self) {
        let mut delay = RECONNECT_BASE;
        loop {
            self.set_state(ConnState::Reconnecting);
            match self.handshake().await {
                Ok(_) => return,
                Err(error) => self.set_error(error),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(RECONNECT_CAP);
        }
    }

    /// 启动上报批处理：64 条/批或 200ms 刷新；连接不可用时丢弃并计数。
    pub fn spawn_reporter(self: &Arc<Self>) {
        let receiver = self.rx.lock().ok().and_then(|mut slot| slot.take());
        let Some(mut receiver) = receiver else {
            return;
        };
        let client = self.clone();
        tokio::spawn(async move {
            let mut batch = Vec::with_capacity(BATCH_MAX);
            loop {
                let deadline = tokio::time::Instant::now() + BATCH_FLUSH;
                loop {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    tokio::select! {
                        sample = receiver.recv() => {
                            match sample {
                                Some(sample) => {
                                    batch.push(sample);
                                    if batch.len() >= BATCH_MAX {
                                        break;
                                    }
                                }
                                None => {
                                    flush(&client, &mut batch).await;
                                    return;
                                }
                            }
                        }
                        _ = tokio::time::sleep(remaining) => break,
                    }
                }
                flush(&client, &mut batch).await;
            }
        });
    }

    pub fn record(&self, segment: &'static str, ms: f64) {
        if self.tx.try_send(Sample { segment, ms }).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn request(&self, mut request: Value) -> Result<Value, String> {
        request["id"] = json!(self.next_request_id());
        let Some(conn) = self.current_conn() else {
            return Err("执行内核未连接".to_string());
        };
        let mut guard = conn.lock().await;
        let result = guard.roundtrip(&request).await;
        drop(guard);
        match result {
            Ok(value) => {
                if value["ok"] != true {
                    return Err(value["error"]
                        .as_str()
                        .unwrap_or("执行内核返回错误")
                        .to_string());
                }
                Ok(value)
            }
            Err(error) => {
                self.set_error(error.clone());
                self.set_state(ConnState::Reconnecting);
                self.clear_conn();
                Err(error)
            }
        }
    }

    pub async fn probe(&self) -> Result<(String, u128), String> {
        let started = Instant::now();
        let response = self.request(json!({"op": "health"})).await?;
        let algo = response["algo"].as_str().unwrap_or("").to_string();
        Ok((algo, started.elapsed().as_millis()))
    }

    pub async fn query_segments(&self) -> Result<Value, String> {
        let response = self.request(json!({"op": "metrics_query"})).await?;
        Ok(response.get("segments").cloned().unwrap_or(Value::Null))
    }

    pub async fn reset_metrics(&self) -> Result<(), String> {
        self.request(json!({"op": "metrics_reset"}))
            .await
            .map(|_| ())
    }

    pub fn state(&self) -> ConnState {
        *self.state.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

async fn flush(client: &ExecClient, batch: &mut Vec<Sample>) {
    if batch.is_empty() {
        return;
    }
    let count = batch.len() as u64;
    let samples: Vec<Value> = batch
        .iter()
        .map(|sample| {
            json!({
                "segment": sample.segment,
                "ms": sample.ms,
                "ts": crate::now_millis(),
            })
        })
        .collect();
    batch.clear();
    if client.state() != ConnState::Connected {
        client.dropped.fetch_add(count, Ordering::Relaxed);
        return;
    }
    if client
        .request(json!({"op": "metrics_ingest", "samples": samples}))
        .await
        .is_err()
    {
        client.dropped.fetch_add(count, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// 内嵌 mock 内核：JSON Lines，hello/health/sign/metrics_* 最小实现。
    async fn spawn_mock_core() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let (r, mut w) = tokio::io::split(stream);
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                        let op = req["op"].as_str().unwrap_or("");
                        let id = req["id"].clone();
                        let resp = match op {
                            "hello" => json!({"id": id, "ok": true, "kernel_version": "1.0.0",
                                "capabilities": ["health","sign","metrics_ingest","metrics_query","metrics_reset"],
                                "metrics_capacity": 2000}),
                            "health" => json!({"id": id, "ok": true, "algo": "hmac-sha256",
                                "uptime_ms": 1, "sessions": 1}),
                            "sign" => json!({"id": id, "ok": true, "signature": "x", "algo": "hmac-sha256",
                                "query": req["query"]}),
                            "metrics_ingest" => json!({"id": id, "ok": true, "accepted": req["samples"].as_array().map(|a| a.len()).unwrap_or(0), "dropped": 0}),
                            "metrics_query" => json!({"id": id, "ok": true, "segments": {}}),
                            "metrics_reset" => json!({"id": id, "ok": true}),
                            _ => json!({"id": id, "ok": false, "error": "unknown op"}),
                        };
                        let mut out = serde_json::to_string(&resp).unwrap();
                        out.push('\n');
                        if w.write_all(out.as_bytes()).await.is_err() { break; }
                        let _ = w.flush().await;
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn handshake_success_and_probe() {
        let addr = spawn_mock_core().await;
        let client = ExecClient::new(&format!("tcp:{addr}"));
        let hello = client.handshake().await.unwrap();
        assert_eq!(hello["kernel_version"], json!("1.0.0"));
        assert_eq!(client.state(), ConnState::Connected);
        let (algo, rtt) = client.probe().await.unwrap();
        assert_eq!(algo, "hmac-sha256");
        assert!(rtt < 500);
    }

    #[tokio::test]
    async fn handshake_failure_when_core_absent() {
        let client = ExecClient::new("tcp:127.0.0.1:59999");
        assert!(client.handshake().await.is_err());
        assert_eq!(client.state(), ConnState::Disconnected);
        assert!(client.last_error().is_some());
    }

    #[tokio::test]
    async fn record_never_blocks_and_counts_drops() {
        let addr = spawn_mock_core().await;
        let client = ExecClient::new(&format!("tcp:{addr}"));
        client.handshake().await.unwrap();
        for _ in 0..QUEUE_BOUND + 10 {
            client.record("total", 1.0);
        }
        assert!(client.dropped() >= 10);
    }

    #[tokio::test]
    async fn reporter_batches_and_ingests() {
        let addr = spawn_mock_core().await;
        let client = ExecClient::new(&format!("tcp:{addr}"));
        client.handshake().await.unwrap();
        client.spawn_reporter();
        for i in 0..10u32 {
            client.record("http", i as f64);
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        let segments = client.query_segments().await.unwrap();
        assert!(segments.is_object());
    }
}