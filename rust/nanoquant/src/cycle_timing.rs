//! 引擎周期阶段耗时：进程内记录、`engine_state` 环形样本、`/engine/stats` 汇总 p95/p99。
//!
//! 只做纯函数与常量，不直接访问数据库；调用方负责读写 `engine_state`。

use serde_json::{json, Map, Value};

/// 每个阶段保留最近多少轮样本；256 轮 ≈ 15 秒周期的 64 分钟。
pub(crate) const SAMPLE_CAP: usize = 256;

/// 固定展示的阶段顺序。
pub(crate) const PHASES: &[&str] = &["cycle", "snapshot", "persist", "review", "apply", "submit"];

/// 追加一轮样本，返回新的环形数组。
pub(crate) fn append(samples: &mut Vec<Value>, round: &[(&'static str, f64)], cap: usize) -> Value {
    let mut object = Map::new();
    for (name, ms) in round {
        if ms.is_finite() && *ms >= 0.0 {
            object.insert((*name).to_string(), json!(round2(*ms)));
        }
    }
    if !object.is_empty() {
        samples.push(Value::Object(object));
    }
    if samples.len() > cap.max(1) {
        let excess = samples.len() - cap.max(1);
        samples.drain(0..excess);
    }
    Value::Array(samples.clone())
}

/// 把环形样本汇总成每个阶段的 count/p50/p95/p99/max。
pub(crate) fn summary(samples: &[Value]) -> Value {
    let mut out = Map::new();
    for phase in PHASES {
        let mut values: Vec<f64> = samples
            .iter()
            .filter_map(|row| row.get(*phase).and_then(Value::as_f64))
            .collect();
        if values.is_empty() {
            continue;
        }
        values.sort_by(f64::total_cmp);
        out.insert(
            (*phase).to_string(),
            json!({
                "samples": values.len(),
                "p50_ms": percentile(&values, 0.50),
                "p95_ms": percentile(&values, 0.95),
                "p99_ms": percentile(&values, 0.99),
                "max_ms": round2(*values.last().unwrap_or(&0.0)),
            }),
        );
    }
    Value::Object(out)
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 * q).floor() as usize).min(sorted.len() - 1);
    round2(sorted[index])
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_keeps_only_the_latest_rounds() {
        let mut samples = Vec::new();
        for index in 0..4 {
            append(
                &mut samples,
                &[("cycle", index as f64), ("snapshot", 10.0)],
                3,
            );
        }
        assert_eq!(samples.len(), 3, "只保留最近 cap 轮");
        assert_eq!(samples[2]["cycle"], 3.0, "最新一轮在末尾");
    }

    #[test]
    fn summary_reports_p50_p95_and_p99_per_phase() {
        let mut samples = Vec::new();
        for index in 0..100 {
            append(
                &mut samples,
                &[("cycle", index as f64), ("review", 1.0)],
                100,
            );
        }
        let summary = summary(&samples);
        assert_eq!(summary["cycle"]["samples"], 100);
        assert_eq!(summary["cycle"]["p50_ms"], 50.0);
        assert_eq!(summary["cycle"]["p95_ms"], 95.0);
        assert_eq!(summary["cycle"]["p99_ms"], 99.0);
        assert_eq!(summary["cycle"]["max_ms"], 99.0);
        assert_eq!(summary["review"]["p99_ms"], 1.0);
    }

    #[test]
    fn invalid_values_are_dropped_instead_of_poisoning_percentiles() {
        let mut samples = Vec::new();
        append(&mut samples, &[("cycle", f64::NAN)], 10);
        assert!(samples.is_empty());
        append(&mut samples, &[("cycle", -1.0)], 10);
        assert!(samples.is_empty());
        assert_eq!(summary(&[]), json!({}));
    }
}
