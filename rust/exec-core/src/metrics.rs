//! 七段延迟指标：每段一个环形缓冲（满容量覆盖最旧），p50/p95/p99/max/last。
//! 索引法分位数与主服务 system.rs latency_view 同口径，无样本返回 null。

use crate::model::SampleIn;
use serde_json::{json, Value};
use std::sync::Mutex;

pub const SEGMENTS: [&str; 7] = [
    "total", "plan", "sign", "http", "wait", "protect", "persist",
];

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

struct Ring {
    buf: Vec<f64>,
    cap: usize,
    next: usize,
    last: Option<f64>,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            buf: Vec::new(),
            cap: cap.max(1),
            next: 0,
            last: None,
        }
    }

    fn push(&mut self, ms: f64) {
        self.last = Some(ms);
        if self.buf.len() < self.cap {
            self.buf.push(ms);
        } else {
            self.buf[self.next] = ms;
            self.next = (self.next + 1) % self.cap;
        }
    }

    fn sorted(&self) -> Vec<f64> {
        let mut v = self.buf.clone();
        v.sort_by(f64::total_cmp);
        v
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.next = 0;
        self.last = None;
    }
}

fn percentile(ordered: &[f64], q: f64) -> Option<f64> {
    if ordered.is_empty() {
        return None;
    }
    Some(ordered[((ordered.len() as f64 * q) as usize).min(ordered.len() - 1)])
}

pub struct MetricsHub {
    rings: Mutex<Vec<Ring>>,
    rejected: Mutex<u64>,
    cap: usize,
}

impl MetricsHub {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            rings: Mutex::new((0..SEGMENTS.len()).map(|_| Ring::new(cap)).collect()),
            rejected: Mutex::new(0),
            cap,
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    fn index_of(segment: &str) -> Option<usize> {
        SEGMENTS.iter().position(|s| *s == segment)
    }

    /// 批量写入：未知 segment 拒绝该条并计数，返回 (accepted, dropped)。
    pub fn ingest_batch(&self, samples: &[SampleIn]) -> (u64, u64) {
        let mut accepted = 0u64;
        let mut dropped = 0u64;
        if let Ok(mut rings) = self.rings.lock() {
            for s in samples {
                match Self::index_of(&s.segment) {
                    Some(i) => {
                        rings[i].push(s.ms);
                        accepted += 1;
                    }
                    None => dropped += 1,
                }
            }
        }
        if dropped > 0 {
            if let Ok(mut r) = self.rejected.lock() {
                *r += dropped;
            }
        }
        (accepted, dropped)
    }

    #[allow(dead_code)]
    pub fn total_rejected(&self) -> u64 {
        self.rejected.lock().map(|v| *v).unwrap_or(0)
    }

    /// 七段视图：每段 {count,p50,p95,p99,max,last}，无样本的字段为 null。
    pub fn view(&self) -> Value {
        let rings = self.rings.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = serde_json::Map::new();
        for (i, name) in SEGMENTS.iter().enumerate() {
            let ordered = rings[i].sorted();
            let p = |q: f64| {
                percentile(&ordered, q)
                    .map(round2)
                    .map(Value::from)
                    .unwrap_or(Value::Null)
            };
            out.insert(
                name.to_string(),
                json!({
                    "count": ordered.len(),
                    "p50": p(0.50),
                    "p95": p(0.95),
                    "p99": p(0.99),
                    "max": ordered.last().copied().map(round2).map(Value::from).unwrap_or(Value::Null),
                    "last": rings[i].last.map(round2).map(Value::from).unwrap_or(Value::Null),
                }),
            );
        }
        Value::Object(out)
    }

    pub fn reset(&self) {
        if let Ok(mut rings) = self.rings.lock() {
            for r in rings.iter_mut() {
                r.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_known_values() {
        let mut ring = Ring::new(8);
        for v in [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0] {
            ring.push(v);
        }
        let ordered = ring.sorted();
        assert_eq!(percentile(&ordered, 0.50), Some(5.0)); // idx=(8*0.5)=4 → 5.0
        assert_eq!(percentile(&ordered, 0.95), Some(8.0));
        assert_eq!(percentile(&ordered, 0.99), Some(8.0));
        assert_eq!(percentile(&ordered, 0.0), Some(1.0));
    }

    #[test]
    fn ring_overwrites_oldest() {
        let mut ring = Ring::new(3);
        for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
            ring.push(v);
        }
        let ordered = ring.sorted();
        assert_eq!(ordered, vec![3.0, 4.0, 5.0]);
    }

    #[test]
    fn empty_ring_percentile_is_none() {
        let ring = Ring::new(4);
        assert_eq!(percentile(&ring.sorted(), 0.5), None);
    }

    #[test]
    fn hub_ingest_unknown_segment_rejected() {
        let hub = MetricsHub::new(8);
        let (accepted, dropped) = hub.ingest_batch(&[
            SampleIn {
                segment: "total".into(),
                ms: 1.0,
                ts: 0,
            },
            SampleIn {
                segment: "nope".into(),
                ms: 2.0,
                ts: 0,
            },
        ]);
        assert_eq!((accepted, dropped), (1, 1));
    }

    #[test]
    fn hub_view_null_when_empty_and_values_after_ingest() {
        let hub = MetricsHub::new(8);
        let view = hub.view();
        for seg in SEGMENTS {
            assert!(view[seg]["count"].as_u64() == Some(0));
            assert!(view[seg]["p50"].is_null());
            assert!(view[seg]["max"].is_null());
        }
        hub.ingest_batch(&[SampleIn {
            segment: "sign".into(),
            ms: 3.5,
            ts: 0,
        }]);
        let view = hub.view();
        assert_eq!(view["sign"]["count"], serde_json::json!(1));
        assert_eq!(view["sign"]["p50"], serde_json::json!(3.5));
        assert_eq!(view["sign"]["last"], serde_json::json!(3.5));
        assert!(!view["sign"]["p95"].is_null());
    }

    #[test]
    fn hub_reset_clears_all() {
        let hub = MetricsHub::new(8);
        hub.ingest_batch(&[SampleIn {
            segment: "http".into(),
            ms: 9.0,
            ts: 0,
        }]);
        hub.reset();
        let view = hub.view();
        assert!(view["http"]["count"].as_u64() == Some(0));
    }

    #[test]
    fn hub_concurrent_ingest_is_safe() {
        let hub = std::sync::Arc::new(MetricsHub::new(64));
        let mut handles = Vec::new();
        for t in 0..4u32 {
            let h = hub.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..100u32 {
                    h.ingest_batch(&[SampleIn {
                        segment: "total".into(),
                        ms: (t * 100 + i) as f64,
                        ts: 0,
                    }]);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(hub.view()["total"]["count"].as_u64(), Some(64));
    }
}
