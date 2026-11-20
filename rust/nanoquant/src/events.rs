//! 运行事件总线：落库 + 内存环形缓冲 + WebSocket 扇出。
//!
//! 与 legacy 内核（已退役） `services/events.py` 同口径：
//! - `record()` 写一条事件（落 `engine_events` 表 + 进环形缓冲 + 扇出）；
//! - `publish()` 只扇出自定义消息，不落库；
//! - 扇出消息形状固定为 `{"type": "event", "data": {...}}`，前端按
//!   `data.category` / `data.level` 分流。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::db::Db;

/// 环形缓冲保留的事件条数，与 legacy 内核（已退役） 的 `deque(maxlen=400)` 一致。
const RING_CAPACITY: usize = 400;
/// 广播通道容量：订阅者跟不上时丢最旧的消息，不阻塞事件生产者。
const BROADCAST_CAPACITY: usize = 500;

#[derive(Clone)]
pub struct EventBus {
    inner: Arc<BusInner>,
}

struct BusInner {
    db: Option<Db>,
    tx: broadcast::Sender<Value>,
    ring: Mutex<VecDeque<Value>>,
    seq: AtomicI64,
}

impl EventBus {
    pub fn new(db: Option<Db>) -> Self {
        Self::with_capacity(db, BROADCAST_CAPACITY)
    }

    /// 指定广播容量。测试用小容量验证慢订阅者的丢帧行为。
    pub fn with_capacity(db: Option<Db>, capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self {
            inner: Arc::new(BusInner {
                db,
                tx,
                ring: Mutex::new(VecDeque::with_capacity(RING_CAPACITY)),
                seq: AtomicI64::new(0),
            }),
        }
    }

    /// 订阅事件流。慢订阅者会收到 `RecvError::Lagged`，由调用方决定丢弃还是断开。
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.inner.tx.subscribe()
    }

    /// 记录一条事件：落库 + 环形缓冲 + 扇出。
    pub fn record(&self, level: &str, category: &str, message: &str, payload: Value) -> Value {
        let seq = self.inner.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let item = json!({
            "seq": seq,
            "level": level,
            "category": category,
            "message": truncate(message, 2000),
            "payload": payload,
            "ts": crate::now(),
        });
        if let Some(db) = self.inner.db.as_ref() {
            if let Err(error) = persist(db, &item) {
                tracing::warn!(%error, "事件落库失败");
            }
        }
        {
            let mut ring = self.inner.ring.lock().expect("event ring");
            if ring.len() >= RING_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(item.clone());
        }
        match level {
            "warn" | "error" => tracing::warn!(category, message),
            _ => tracing::info!(category, message),
        }
        self.fanout("event", item.clone());
        item
    }

    /// 扇出一条自定义消息（不落库），对应 legacy 内核（已退役） 的 `events.publish`。
    pub fn publish(&self, kind: &str, data: Value) {
        self.fanout(kind, data);
    }

    /// 最近的事件，最新的在前。
    pub fn recent(&self, limit: usize) -> Vec<Value> {
        let ring = self.inner.ring.lock().expect("event ring");
        ring.iter().rev().take(limit).cloned().collect()
    }

    fn fanout(&self, kind: &str, data: Value) {
        // 没有订阅者时 broadcast 会返回 Err，这是正常状态，不算错误。
        let _ = self.inner.tx.send(json!({"type": kind, "data": data}));
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

fn persist(db: &Db, item: &Value) -> Result<(), String> {
    let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
    let payload = item
        .get("payload")
        .map(|value| value.to_string())
        .unwrap_or_else(|| "{}".to_string());
    conn.execute(
        "INSERT INTO engine_events(ts,level,category,message,payload) VALUES(?1,?2,?3,?4,?5)",
        (
            item.get("ts")
                .and_then(Value::as_i64)
                .unwrap_or_else(crate::now),
            item.get("level").and_then(Value::as_str).unwrap_or("info"),
            item.get("category")
                .and_then(Value::as_str)
                .unwrap_or("engine"),
            item.get("message").and_then(Value::as_str).unwrap_or(""),
            payload,
        ),
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::RecvError;

    #[tokio::test]
    async fn recorded_events_reach_subscribers_with_the_legacy_shape() {
        let bus = EventBus::new(None);
        let mut rx = bus.subscribe();
        bus.record("warn", "risk", "回撤超限", json!({"pct": 3.5}));
        let payload = rx.recv().await.unwrap();
        assert_eq!(payload["type"], "event");
        let data = &payload["data"];
        assert_eq!(data["level"], "warn");
        assert_eq!(data["category"], "risk");
        assert_eq!(data["message"], "回撤超限");
        assert_eq!(data["payload"]["pct"], 3.5);
        assert!(data["seq"].as_i64().unwrap() >= 1);
        assert!(data["ts"].as_i64().unwrap() > 0);
    }

    #[tokio::test]
    async fn custom_publish_skips_the_event_shape() {
        let bus = EventBus::new(None);
        let mut rx = bus.subscribe();
        bus.publish("account", json!({"equity": 100.0}));
        let payload = rx.recv().await.unwrap();
        assert_eq!(payload["type"], "account");
        assert_eq!(payload["data"]["equity"], 100.0);
    }

    #[tokio::test]
    async fn recent_returns_newest_first() {
        let bus = EventBus::new(None);
        bus.record("info", "engine", "第一条", json!({}));
        bus.record("info", "engine", "第二条", json!({}));
        let recent = bus.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0]["message"], "第二条");
        assert_eq!(recent[1]["message"], "第一条");
    }

    #[test]
    fn ring_buffer_never_grows_past_capacity() {
        let bus = EventBus::new(None);
        for index in 0..(RING_CAPACITY + 25) {
            bus.record("info", "engine", &format!("事件 {index}"), json!({}));
        }
        let recent = bus.recent(RING_CAPACITY * 2);
        assert_eq!(recent.len(), RING_CAPACITY);
        assert_eq!(recent[0]["message"], format!("事件 {}", RING_CAPACITY + 24));
    }

    #[tokio::test]
    async fn slow_subscribers_are_told_they_lagged_instead_of_blocking_the_bus() {
        let bus = EventBus::with_capacity(None, 8);
        let mut rx = bus.subscribe();
        // 广播通道容量有限，塞满后未读订阅者会落后。
        for index in 0..32 {
            bus.record("info", "engine", &format!("事件 {index}"), json!({}));
        }
        let mut lagged = false;
        for _ in 0..32 {
            match tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
                Ok(Ok(_)) => {}
                Ok(Err(RecvError::Lagged(_))) => {
                    lagged = true;
                    break;
                }
                Ok(Err(RecvError::Closed)) => break,
                Err(_) => break,
            }
        }
        assert!(lagged, "订阅者跟不上时必须收到 Lagged 而不是卡死");
    }

    #[test]
    fn long_messages_are_truncated_before_storage() {
        let bus = EventBus::new(None);
        let item = bus.record("error", "engine", &"x".repeat(5000), json!({}));
        assert_eq!(item["message"].as_str().unwrap().chars().count(), 2000);
    }
}
