//! 币安 K 线 WebSocket 上游：一条连接复用多路订阅，断线指数退避重连。
//!
//! 为什么走 WS 而不是 REST 轮询：REST `/fapi/v1/klines` 每几秒拉一次会把出口 IP
//! 打进 418 封禁；WebSocket 推送没有 REST 权重，蜡烛每次变动即时到达。
//!
//! 设计（与 legacy 内核（已退役） `exchange/kline_stream.py` 同口径）：
//! - 一个上游连接，按需动态 `SUBSCRIBE` / `UNSUBSCRIBE`；
//! - 多个浏览器订阅同一 `(symbol, interval)` 时复用同一条流，扇出到各自的通道；
//! - 断线指数退避重连，重连后自动补订阅；
//! - 交易环境切换（demo/live）时自动切换到对应的流域名。
//!
//! 上游连接用 trait 抽象，测试里换成确定性的假上游，不碰真实网络。

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, watch};

use crate::app_state::AppState;

/// 币安 U 本位永续行情推送（模拟盘=测试网，实盘=主网）。
pub const STREAM_HOSTS: &[(&str, &str)] = &[
    ("demo", "wss://stream.binancefuture.com/ws"),
    ("live", "wss://fstream.binance.com/ws"),
];

/// 同时允许的 `(symbol, interval)` 组合数上限。超了直接拒绝新订阅，
/// 防止一个人把服务端的订阅表撑爆。
pub const MAX_STREAMS: usize = 32;
/// 同一条流上的客户端数上限。
pub const MAX_CLIENTS_PER_STREAM: usize = 128;
/// 单条上游消息的大小上限，超过就丢弃。
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// 每条流的广播通道容量：慢客户端会收到 `Lagged`，不会拖住上游。
const BAR_CHANNEL_CAPACITY: usize = 256;
/// 退避上限，与 legacy 内核（已退役） 的 `min(15.0, backoff * 2)` 一致。
const MAX_BACKOFF: Duration = Duration::from_secs(15);

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 交易环境归一化：只有 live 走主网，其余一律当模拟盘。
pub fn normalize_env(mode: &str) -> &'static str {
    if mode == "live" {
        "live"
    } else {
        "demo"
    }
}

pub fn stream_url(env: &str) -> &'static str {
    STREAM_HOSTS
        .iter()
        .find(|(name, _)| *name == env)
        .map(|(_, url)| *url)
        .unwrap_or(STREAM_HOSTS[0].1)
}

pub fn stream_name(symbol: &str, interval: &str) -> String {
    format!("{}@kline_{}", symbol.to_lowercase(), interval)
}

/// K 线流的只读接口：上游连接情况与当前订阅。
pub fn routes() -> Router<AppState> {
    Router::new().route("/kline/status", get(kline_status))
}

async fn kline_status(State(state): State<AppState>) -> Json<Value> {
    Json(state.klines.status())
}

/// 上游断开后的下一次退避：1s 起翻倍，封顶 15s。
fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// 交易环境变化必须换域名重连，否则模拟盘的流会串到实盘。
fn needs_reconnect(current_env: &str, mode: &str) -> bool {
    normalize_env(mode) != current_env
}

/// 一根 K 线。字段名与 legacy 内核（已退役） 推送的 payload 完全一致，前端零改动。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KlineBar {
    pub symbol: String,
    pub interval: String,
    pub t: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
    pub closed: bool,
    pub next_close_at: i64,
    pub event_time: i64,
}

/// 解析上游推送。原始 `/ws` 用 SUBSCRIBE 订阅时 K 线事件在顶层，
/// 组合流才包在 `data` 里，两种都要认。
pub fn parse_kline(raw: &str) -> Option<KlineBar> {
    let message: Value = serde_json::from_str(raw).ok()?;
    let data = match message.get("data") {
        Some(inner) if inner.is_object() => inner.clone(),
        _ => message.clone(),
    };
    if data.get("e").and_then(Value::as_str) != Some("kline") {
        return None;
    }
    let k = data.get("k")?.as_object()?;
    let number = |key: &str| k.get(key).and_then(Value::as_f64);
    Some(KlineBar {
        symbol: k.get("s")?.as_str()?.to_uppercase(),
        interval: k.get("i")?.as_str()?.to_string(),
        t: k.get("t")?.as_i64()?,
        o: number("o")?,
        h: number("h")?,
        l: number("l")?,
        c: number("c")?,
        v: number("v")?,
        closed: k.get("x").and_then(Value::as_bool).unwrap_or(false),
        next_close_at: k.get("T")?.as_i64()? + 1,
        event_time: data
            .get("E")
            .and_then(Value::as_i64)
            .unwrap_or_else(crate::now_millis),
    })
}

#[derive(Debug, PartialEq)]
pub enum HubError {
    /// 订阅的流数量超过上限。
    TooManyStreams,
    /// 同一条流上的客户端数量超过上限。
    TooManyClients,
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HubError::TooManyStreams => write!(f, "K 线订阅数超过上限"),
            HubError::TooManyClients => write!(f, "同一路 K 线的订阅人数超过上限"),
        }
    }
}

/// 上游连接。用 trait 抽象是为了让测试不依赖真实网络。
pub trait Upstream: Send {
    fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), String>>;
    fn next_message<'a>(&'a mut self) -> BoxFuture<'a, Option<Result<String, String>>>;
}

/// 建立上游连接的工厂。
pub trait KlineSource: Send + Sync {
    fn connect(&self, url: &str) -> BoxFuture<'static, Result<Box<dyn Upstream>, String>>;
}

type StreamKey = (String, String);

struct StreamEntry {
    tx: broadcast::Sender<KlineBar>,
    clients: usize,
}

#[derive(Default)]
struct HubStatus {
    connected: bool,
    env: String,
    last_message_at: Option<Instant>,
}

struct HubInner {
    source: Arc<dyn KlineSource>,
    mode: Arc<dyn Fn() -> String + Send + Sync>,
    streams: Mutex<HashMap<StreamKey, StreamEntry>>,
    notify: watch::Sender<u64>,
    status: Mutex<HubStatus>,
    started: AtomicBool,
    stop_tx: watch::Sender<bool>,
}

/// K 线订阅中枢。对外只暴露订阅 / 退订 / 状态三件事。
#[derive(Clone)]
pub struct KlineHub {
    inner: Arc<HubInner>,
}

impl KlineHub {
    pub fn new(source: Arc<dyn KlineSource>, mode: Arc<dyn Fn() -> String + Send + Sync>) -> Self {
        let (notify, _) = watch::channel(0u64);
        let (stop_tx, _) = watch::channel(false);
        Self {
            inner: Arc::new(HubInner {
                source,
                mode,
                streams: Mutex::new(HashMap::new()),
                notify,
                status: Mutex::new(HubStatus::default()),
                started: AtomicBool::new(false),
                stop_tx,
            }),
        }
    }

    /// 订阅一路 K 线。返回的接收端落后时会收到 `Lagged`，由调用方决定丢帧还是断连。
    pub fn subscribe(
        &self,
        symbol: &str,
        interval: &str,
    ) -> Result<broadcast::Receiver<KlineBar>, HubError> {
        let key = (symbol.to_uppercase(), interval.to_string());
        {
            let mut streams = self.inner.streams.lock().expect("streams");
            match streams.get_mut(&key) {
                Some(entry) => {
                    if entry.clients >= MAX_CLIENTS_PER_STREAM {
                        return Err(HubError::TooManyClients);
                    }
                    entry.clients += 1;
                }
                None => {
                    if streams.len() >= MAX_STREAMS {
                        return Err(HubError::TooManyStreams);
                    }
                    let (tx, _) = broadcast::channel(BAR_CHANNEL_CAPACITY);
                    streams.insert(key.clone(), StreamEntry { tx, clients: 1 });
                }
            }
        }
        self.bump_notify();
        self.ensure_manager();
        let streams = self.inner.streams.lock().expect("streams");
        let entry = streams.get(&key).expect("订阅刚登记过，不会消失");
        Ok(entry.tx.subscribe())
    }

    /// 退订一路 K 线。最后一个订阅者离开时清掉流，上游也会收到 UNSUBSCRIBE。
    pub fn unsubscribe(&self, symbol: &str, interval: &str) {
        let key = (symbol.to_uppercase(), interval.to_string());
        {
            let mut streams = self.inner.streams.lock().expect("streams");
            if let Some(entry) = streams.get_mut(&key) {
                entry.clients = entry.clients.saturating_sub(1);
                if entry.clients == 0 {
                    streams.remove(&key);
                }
            }
        }
        self.bump_notify();
    }

    /// 通知上游管理器订阅表变了。
    ///
    /// 注意：`borrow()` 拿的是读锁，必须先把它放掉再 `send_replace`，
    /// 否则同一个语句里读锁未释放，写锁会一直等下去（自死锁）。
    fn bump_notify(&self) {
        let next = {
            let current = *self.inner.notify.borrow();
            current.wrapping_add(1)
        };
        self.inner.notify.send_replace(next);
    }

    /// 运行状态，字段与 legacy 内核（已退役） `kline_hub.status()` 一致。
    /// 运行状态：上游连接情况、当前订阅、订阅者数量与最后一条消息的年龄。
    pub fn status(&self) -> Value {
        let streams = self.inner.streams.lock().expect("streams");
        let status = self.inner.status.lock().expect("status");
        json!({
            "connected": status.connected,
            "env": status.env,
            "subscriptions": streams
                .keys()
                .map(|(symbol, interval)| format!("{symbol}@{interval}"))
                .collect::<Vec<_>>(),
            "clients": streams.values().map(|entry| entry.clients).sum::<usize>(),
            "last_message_age": status
                .last_message_at
                .map(|at| at.elapsed().as_secs_f64().round() * 10.0 / 10.0),
        })
    }

    /// 停止上游任务（进程退出时调用）。
    pub fn stop(&self) {
        let _ = self.inner.stop_tx.send(true);
    }

    fn ensure_manager(&self) {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let inner = self.inner.clone();
        let stop = self.inner.stop_tx.subscribe();
        let notify = self.inner.notify.subscribe();
        tokio::spawn(async move {
            manager(inner, stop, notify).await;
        });
    }
}

async fn manager(
    inner: Arc<HubInner>,
    mut stop: watch::Receiver<bool>,
    mut notify: watch::Receiver<u64>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *stop.borrow() {
            break;
        }
        let names = stream_names_of(&inner);
        if names.is_empty() {
            // 没有订阅时不占着上游连接，等有人订阅再连。
            tokio::select! {
                _ = stop.changed() => break,
                _ = notify.changed() => {},
                _ = tokio::time::sleep(Duration::from_millis(300)) => {},
            }
            continue;
        }
        let env = normalize_env(&(inner.mode)());
        let url = stream_url(env);
        let connect = inner.source.connect(url);
        let connected = tokio::select! {
            biased;
            _ = stop.changed() => break,
            result = connect => result,
        };
        let mut upstream = match connected {
            Ok(upstream) => upstream,
            Err(error) => {
                tracing::warn!(%error, url, "K 线流连接失败");
                if wait_backoff(&mut stop, backoff).await {
                    break;
                }
                backoff = next_backoff(backoff);
                continue;
            }
        };
        {
            let mut status = inner.status.lock().expect("status");
            status.connected = true;
            status.env = env.to_string();
        }
        backoff = Duration::from_secs(1);
        let mut subscribed: HashSet<String> = HashSet::new();
        if upstream.send_text(subscribe_command(&names)).await.is_ok() {
            subscribed = names.into_iter().collect();
        }
        tracing::info!(url, streams = subscribed.len(), "K 线流已连接");
        loop {
            enum Step {
                Message(Option<Result<String, String>>),
                Changed,
                Tick,
                Stop,
            }
            let step = tokio::select! {
                biased;
                _ = stop.changed() => Step::Stop,
                message = upstream.next_message() => Step::Message(message),
                changed = notify.changed() => {
                    if changed.is_err() { Step::Stop } else { Step::Changed }
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => Step::Tick,
            };
            match step {
                Step::Stop => break,
                Step::Message(Some(Ok(text))) => dispatch(&inner, &text),
                Step::Message(Some(Err(error))) => {
                    tracing::warn!(%error, "K 线流断开");
                    break;
                }
                Step::Message(None) => break,
                Step::Changed => {
                    let now = stream_names_of(&inner);
                    let added: Vec<String> = now
                        .iter()
                        .filter(|name| !subscribed.contains(*name))
                        .cloned()
                        .collect();
                    let removed: Vec<String> = subscribed
                        .iter()
                        .filter(|name| !now.contains(*name))
                        .cloned()
                        .collect();
                    if !added.is_empty() {
                        if upstream.send_text(subscribe_command(&added)).await.is_ok() {
                            subscribed.extend(added);
                        }
                    }
                    if !removed.is_empty() {
                        if upstream
                            .send_text(unsubscribe_command(&removed))
                            .await
                            .is_ok()
                        {
                            for name in &removed {
                                subscribed.remove(name);
                            }
                        }
                    }
                    if now.is_empty() {
                        // 没人订阅了就断开上游，别空占着一条连接。
                        break;
                    }
                }
                Step::Tick => {
                    // 交易环境变了要换域名重连，否则模拟盘的流会串到实盘。
                    if needs_reconnect(&env, &(inner.mode)()) {
                        tracing::info!("交易环境变化，重连 K 线流");
                        break;
                    }
                }
            }
        }
        {
            let mut status = inner.status.lock().expect("status");
            status.connected = false;
            status.env.clear();
        }
        if stream_names_of(&inner).is_empty() {
            // 订阅清空后不用等退避，直接回到等待状态。
            continue;
        }
        if wait_backoff(&mut stop, backoff).await {
            break;
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    tracing::info!("K 线流任务已停止");
}

fn stream_names_of(inner: &HubInner) -> Vec<String> {
    let streams = inner.streams.lock().expect("streams");
    streams
        .keys()
        .map(|(symbol, interval)| stream_name(symbol, interval))
        .collect()
}

fn dispatch(inner: &HubInner, raw: &str) {
    let Some(bar) = parse_kline(raw) else {
        return;
    };
    let key = (bar.symbol.clone(), bar.interval.clone());
    {
        let streams = inner.streams.lock().expect("streams");
        if let Some(entry) = streams.get(&key) {
            // 没有接收者时 send 会失败，这只是「当前没人订这一路」。
            let _ = entry.tx.send(bar);
        }
    }
    let mut status = inner.status.lock().expect("status");
    status.last_message_at = Some(Instant::now());
}

async fn wait_backoff(stop: &mut watch::Receiver<bool>, backoff: Duration) -> bool {
    tokio::select! {
        _ = stop.changed() => true,
        _ = tokio::time::sleep(backoff) => false,
    }
}

fn subscribe_command(streams: &[String]) -> String {
    json!({
        "method": "SUBSCRIBE",
        "params": streams,
        "id": crate::now_millis(),
    })
    .to_string()
}

fn unsubscribe_command(streams: &[String]) -> String {
    json!({
        "method": "UNSUBSCRIBE",
        "params": streams,
        "id": crate::now_millis(),
    })
    .to_string()
}

/// 真实上游：币安 futures WebSocket。
pub struct BinanceWsSource;

impl KlineSource for BinanceWsSource {
    fn connect(&self, url: &str) -> BoxFuture<'static, Result<Box<dyn Upstream>, String>> {
        let url = url.to_string();
        Box::pin(async move {
            let (ws, _response) = tokio_tungstenite::connect_async(&url)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Box::new(TungsteniteUpstream { ws }) as Box<dyn Upstream>)
        })
    }
}

struct TungsteniteUpstream {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}

impl Upstream for TungsteniteUpstream {
    fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.ws
                .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn next_message<'a>(&'a mut self) -> BoxFuture<'a, Option<Result<String, String>>> {
        Box::pin(async move {
            loop {
                match self.ws.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) => {
                        if text.len() > MAX_MESSAGE_BYTES {
                            continue;
                        }
                        return Some(Ok(text.to_string()));
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(data))) => {
                        if data.len() > MAX_MESSAGE_BYTES {
                            continue;
                        }
                        return Some(Ok(String::from_utf8_lossy(&data).to_string()));
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                        return None
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => return Some(Err(error.to_string())),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_status_route_reports_an_idle_hub() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let app = Router::new()
            .nest("/api/v1", routes())
            .with_state(crate::test_state(None));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/kline/status")
                    .body(Body::from(""))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["connected"], false, "没有订阅时上游不会连");
        assert_eq!(body["clients"], 0);
        assert_eq!(body["subscriptions"].as_array().unwrap().len(), 0);
    }

    /// 假上游：记录收到的指令，按脚本吐消息。
    struct FakeUpstream {
        commands: Arc<Mutex<Vec<String>>>,
        inbox: tokio::sync::mpsc::Receiver<Option<String>>,
    }

    impl Upstream for FakeUpstream {
        fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), String>> {
            self.commands.lock().expect("commands").push(text);
            Box::pin(async move { Ok(()) })
        }

        fn next_message<'a>(&'a mut self) -> BoxFuture<'a, Option<Result<String, String>>> {
            Box::pin(async move {
                match self.inbox.recv().await {
                    Some(Some(text)) => Some(Ok(text)),
                    Some(None) | None => None,
                }
            })
        }
    }

    struct FakeSource {
        commands: Arc<Mutex<Vec<String>>>,
        inbox: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Option<String>>>>>,
        fail: bool,
    }

    impl KlineSource for FakeSource {
        fn connect(&self, _url: &str) -> BoxFuture<'static, Result<Box<dyn Upstream>, String>> {
            let commands = self.commands.clone();
            let inbox = self.inbox.clone();
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    return Err("连接被拒绝".to_string());
                }
                let (tx, rx) = tokio::sync::mpsc::channel(64);
                *inbox.lock().expect("inbox") = Some(tx);
                Ok(Box::new(FakeUpstream {
                    commands,
                    inbox: rx,
                }) as Box<dyn Upstream>)
            })
        }
    }

    type FakeSourceParts = (
        Arc<FakeSource>,
        Arc<Mutex<Vec<String>>>,
        Arc<Mutex<Option<tokio::sync::mpsc::Sender<Option<String>>>>>,
    );

    fn fake_source(fail: bool) -> FakeSourceParts {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let inbox = Arc::new(Mutex::new(None));
        let source = Arc::new(FakeSource {
            commands: commands.clone(),
            inbox: inbox.clone(),
            fail,
        });
        (source, commands, inbox)
    }

    /// 等上游连上，拿到它的消息入口。
    async fn wait_for_upstream(
        inbox: &Arc<Mutex<Option<tokio::sync::mpsc::Sender<Option<String>>>>>,
    ) -> tokio::sync::mpsc::Sender<Option<String>> {
        for _ in 0..100 {
            if let Some(sender) = inbox.lock().expect("inbox").clone() {
                return sender;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("上游没有连上");
    }

    fn kline_message(symbol: &str, interval: &str, price: f64) -> String {
        json!({
            "e": "kline",
            "E": 1_700_000_000_000i64,
            "s": symbol,
            "k": {
                "t": 1_700_000_000_000i64,
                "T": 1_700_000_059_999i64,
                "s": symbol,
                "i": interval,
                "o": price - 1.0,
                "c": price,
                "h": price + 1.0,
                "l": price - 2.0,
                "v": 12.5,
                "x": false,
            }
        })
        .to_string()
    }

    fn hub(source: Arc<dyn KlineSource>) -> KlineHub {
        KlineHub::new(source, Arc::new(|| "demo".to_string()))
    }

    #[test]
    fn parses_a_binance_kline_event_into_the_frontend_shape() {
        let bar = parse_kline(&kline_message("BTCUSDT", "1m", 100.0)).expect("应解析出 K 线");
        assert_eq!(bar.symbol, "BTCUSDT");
        assert_eq!(bar.interval, "1m");
        assert_eq!(bar.c, 100.0);
        assert_eq!(bar.o, 99.0);
        assert_eq!(bar.h, 101.0);
        assert_eq!(bar.l, 98.0);
        assert_eq!(bar.v, 12.5);
        assert!(!bar.closed);
        assert_eq!(bar.next_close_at, 1_700_000_060_000);
        assert_eq!(bar.event_time, 1_700_000_000_000);
    }

    #[test]
    fn ignores_non_kline_and_malformed_messages() {
        assert!(parse_kline(r#"{"e":"aggTrade","s":"BTCUSDT"}"#).is_none());
        assert!(parse_kline("not json").is_none());
        assert!(parse_kline(r#"{"e":"kline","k":{"s":"BTCUSDT"}}"#).is_none());
    }

    #[test]
    fn combined_stream_payloads_are_accepted() {
        let raw = json!({
            "stream": "btcusdt@kline_1m",
            "data": {
                "e": "kline",
                "E": 1_700_000_000_000i64,
                "k": {
                    "t": 1i64, "T": 2i64, "s": "btcusdt", "i": "1m",
                    "o": 1.0, "h": 2.0, "l": 0.5, "c": 1.5, "v": 3.0, "x": true
                }
            }
        })
        .to_string();
        let bar = parse_kline(&raw).expect("组合流也要认");
        assert_eq!(bar.symbol, "BTCUSDT");
        assert!(bar.closed);
    }

    #[tokio::test]
    async fn subscribers_receive_bars_pushed_by_the_upstream() {
        let (source, _commands, inbox) = fake_source(false);
        let hub = hub(source);
        let mut rx = hub.subscribe("btcusdt", "1m").expect("订阅应成功");
        let sender = wait_for_upstream(&inbox).await;
        sender
            .send(Some(kline_message("BTCUSDT", "1m", 88.0)))
            .await
            .expect("喂消息");
        let bar = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("不应超时")
            .expect("应收到 K 线");
        assert_eq!(bar.c, 88.0);
        hub.stop();
    }

    #[tokio::test]
    async fn a_new_subscription_reaches_the_upstream_without_reconnecting() {
        let (source, commands, _inbox) = fake_source(false);
        let hub = hub(source);
        let _first = hub.subscribe("BTCUSDT", "1m").unwrap();
        for _ in 0..50 {
            if !commands.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _second = hub.subscribe("ETHUSDT", "1m").unwrap();
        for _ in 0..50 {
            if commands
                .lock()
                .unwrap()
                .iter()
                .any(|command| command.contains("ethusdt@kline_1m"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let commands = commands.lock().unwrap().clone();
        assert!(
            commands
                .iter()
                .any(|command| command.contains("btcusdt@kline_1m")),
            "首个订阅要发 SUBSCRIBE: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|command| command.contains("ethusdt@kline_1m")),
            "新增订阅要补发 SUBSCRIBE: {commands:?}"
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.contains("\"SUBSCRIBE\""))
                .count(),
            2,
            "第二条流应增量订阅，而不是重连: {commands:?}"
        );
        hub.stop();
    }

    #[tokio::test]
    async fn dropping_the_last_subscriber_unsubscribes_upstream() {
        let (source, commands, _inbox) = fake_source(false);
        let hub = hub(source);
        let rx = hub.subscribe("BTCUSDT", "1m").unwrap();
        for _ in 0..50 {
            if !commands.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        hub.unsubscribe("BTCUSDT", "1m");
        drop(rx);
        for _ in 0..50 {
            if commands
                .lock()
                .unwrap()
                .iter()
                .any(|command| command.contains("UNSUBSCRIBE"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let commands = commands.lock().unwrap().clone();
        assert!(
            commands
                .iter()
                .any(|command| command.contains("UNSUBSCRIBE")),
            "没人订阅时要退订上游: {commands:?}"
        );
        hub.stop();
    }

    #[tokio::test]
    async fn a_failed_connection_is_retried_after_a_backoff() {
        let (source, _commands, _inbox) = fake_source(true);
        let hub = hub(source);
        let _rx = hub.subscribe("BTCUSDT", "1m").unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(hub.status()["connected"], json!(false));
        assert!(!hub.status()["subscriptions"].as_array().unwrap().is_empty());
        hub.stop();
    }

    #[tokio::test]
    async fn subscription_count_is_capped() {
        let (source, _commands, _inbox) = fake_source(false);
        let hub = hub(source);
        for index in 0..MAX_STREAMS {
            hub.subscribe(&format!("SYM{index}USDT"), "1m")
                .expect("上限内允许订阅");
        }
        assert_eq!(
            hub.subscribe("EXTRAUSDT", "1m").unwrap_err(),
            HubError::TooManyStreams,
            "超过上限必须拒绝，不能把服务撑爆"
        );
        hub.stop();
    }

    #[tokio::test]
    async fn clients_on_one_stream_are_capped() {
        let (source, _commands, _inbox) = fake_source(false);
        let hub = hub(source);
        for _ in 0..MAX_CLIENTS_PER_STREAM {
            hub.subscribe("BTCUSDT", "1m").expect("上限内允许订阅");
        }
        assert_eq!(
            hub.subscribe("BTCUSDT", "1m").unwrap_err(),
            HubError::TooManyClients
        );
        hub.stop();
    }

    #[test]
    fn stream_urls_follow_the_trading_environment() {
        assert_eq!(stream_url("demo"), "wss://stream.binancefuture.com/ws");
        assert_eq!(stream_url("live"), "wss://fstream.binance.com/ws");
        assert_eq!(stream_url("weird"), "wss://stream.binancefuture.com/ws");
        assert_eq!(normalize_env("live"), "live");
        assert_eq!(normalize_env("demo"), "demo");
        assert_eq!(normalize_env("unknown"), "demo");
    }

    #[test]
    fn backoff_doubles_from_one_second_and_caps_at_fifteen() {
        let mut backoff = Duration::from_secs(1);
        let mut sequence = Vec::new();
        for _ in 0..6 {
            sequence.push(backoff.as_secs());
            backoff = next_backoff(backoff);
        }
        assert_eq!(
            sequence,
            [1, 2, 4, 8, 15, 15],
            "退避必须从 1s 起翻倍并封顶 15s"
        );
    }

    #[test]
    fn environment_switch_requests_a_reconnect() {
        assert!(
            needs_reconnect("demo", "live"),
            "demo → live 必须换域名重连"
        );
        assert!(needs_reconnect("live", "demo"));
        assert!(!needs_reconnect("demo", "demo"), "环境没变不能无谓重连");
        assert!(
            !needs_reconnect("demo", "paper"),
            "非 live 都归一到 demo，不应重连"
        );
    }
}
