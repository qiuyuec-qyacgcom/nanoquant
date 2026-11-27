//! AI 增量价值：被 AI 否决信号的反事实收益、置信度校准、成本/延迟对比。
//!
//! 只读 `signals` / `market_candles` / `trades` / `ai_call_logs`，
//! 不发请求、不写库，供 `/engine/ai-value` 与复盘使用。
//! 反事实 PnL 基于本地已落库 K 线，未计滑点与手续费。

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

/// 反事实评估窗口：AI 否决后观察多久的价格变化。
pub(crate) const DEFAULT_HORIZON_SEC: i64 = 24 * 3600;

/// 置信度分箱：下界、上界（含上界）、标签。
const CONFIDENCE_BINS: [(f64, f64, &str); 4] = [
    (0.0, 0.5, "0.00-0.50"),
    (0.5, 0.7, "0.50-0.70"),
    (0.7, 0.85, "0.70-0.85"),
    (0.85, 1.000_001, "0.85-1.00"),
];

pub(crate) fn stats(conn: &Connection, mode: &str, now: i64, window_sec: i64) -> Value {
    let window = window_sec.max(0);
    let since = now.saturating_sub(window);
    let counterfactual = counterfactual_stats(conn, mode, since, DEFAULT_HORIZON_SEC);
    let confidence = confidence_calibration(conn, mode, since);
    let cost = cost_stats(conn, since);
    let avoided_usd = counterfactual["avoided_loss_usd"].as_f64().unwrap_or(0.0);
    let cost_usd = cost["cost_usd"].as_f64().unwrap_or(0.0);
    let net_usd = avoided_usd - cost_usd;
    json!({
        "window_sec": window,
        "mode": mode,
        "counterfactual": counterfactual,
        "confidence": confidence,
        "cost": cost,
        "value": {
            "avoided_loss_usd": round2(avoided_usd),
            "ai_cost_usd": round2(cost_usd),
            "net_usd": round2(net_usd),
            "value_cost_ratio": if cost_usd > 0.0 {
                round2(avoided_usd / cost_usd)
            } else {
                0.0
            },
        },
        "disclaimer": "反事实 PnL 基于本地已落库 K 线，未计滑点与手续费；仅评估 AI 否决的增量价值。",
    })
}

/// AI 否决信号的反事实收益：按信号方向持有 `horizon_sec` 后的价格变化。
fn counterfactual_stats(conn: &Connection, mode: &str, since: i64, horizon_sec: i64) -> Value {
    let mut vetoed = 0_i64;
    let mut samples = 0_i64;
    let mut skipped = 0_i64;
    let mut hits = 0_i64;
    let mut sum_pct = 0.0_f64;
    let mut avoided_loss_pct = 0.0_f64;

    if let Ok(mut stmt) = conn.prepare(
        "SELECT ts, symbol, direction, decision, ai_action, features FROM signals \
         WHERE mode=?1 AND ts>=?2 ORDER BY ts DESC",
    ) {
        if let Ok(rows) = stmt.query_map(params![mode, since], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        }) {
            for (ts, symbol, direction, decision, ai_action, features) in rows.flatten() {
                let sign = match direction.as_str() {
                    "long" => 1.0,
                    "short" => -1.0,
                    _ => continue,
                };
                if !is_ai_veto(&decision, &ai_action, features.as_deref()) {
                    continue;
                }
                vetoed += 1;
                let Some(entry) = close_at_or_before(conn, &symbol, ts.saturating_mul(1000)) else {
                    skipped += 1;
                    continue;
                };
                let Some(exit) =
                    close_at_or_before(conn, &symbol, ts.saturating_add(horizon_sec) * 1000)
                else {
                    skipped += 1;
                    continue;
                };
                let pct = (exit / entry - 1.0) * 100.0 * sign;
                samples += 1;
                sum_pct += pct;
                if pct < 0.0 {
                    hits += 1;
                    avoided_loss_pct += -pct;
                }
            }
        }
    }

    let notional = average_notional(conn, mode, since);
    let avoided_loss_usd = avoided_loss_pct / 100.0 * notional;
    json!({
        "vetoed_signals": vetoed,
        "samples": samples,
        "skipped": skipped,
        "horizon_sec": horizon_sec,
        "avg_pnl_pct": if samples == 0 {
            0.0
        } else {
            round4(sum_pct / samples as f64)
        },
        "hit_rate_pct": if samples == 0 {
            0.0
        } else {
            round2(hits as f64 / samples as f64 * 100.0)
        },
        "assumed_notional_usd": round2(notional),
        "avoided_loss_usd": round2(avoided_loss_usd),
    })
}

/// 只把「AI 主动否决」算进来：insight 已标注 ai_veto，或 rejected 且 AI 动作是 hold/reject。
fn is_ai_veto(decision: &str, ai_action: &str, features: Option<&str>) -> bool {
    if let Some(features) = features {
        if let Ok(value) = serde_json::from_str::<Value>(features) {
            if value.pointer("/insight/vetoed_by").and_then(Value::as_str) == Some("ai_veto") {
                return true;
            }
        }
    }
    decision == "rejected" && matches!(ai_action.trim(), "hold" | "reject")
}

fn close_at_or_before(conn: &Connection, symbol: &str, ts_ms: i64) -> Option<f64> {
    conn.query_row(
        "SELECT close FROM market_candles WHERE symbol=?1 AND close_time<=?2 \
         ORDER BY close_time DESC LIMIT 1",
        params![symbol, ts_ms],
        |row| row.get::<_, f64>(0),
    )
    .optional()
    .ok()
    .flatten()
    .filter(|value| value.is_finite() && *value > 0.0)
}

fn average_notional(conn: &Connection, mode: &str, since: i64) -> f64 {
    conn.query_row(
        "SELECT COALESCE(AVG(notional), 0) FROM trades \
         WHERE mode=?1 AND closed_at>=?2 AND notional>0",
        params![mode, since],
        |row| row.get::<_, f64>(0),
    )
    .unwrap_or(0.0)
}

/// AI 置信度校准：按置信度分箱比较「声称的胜率」与「实际胜率」。
fn confidence_calibration(conn: &Connection, mode: &str, since: i64) -> Value {
    // [samples, sum_confidence, wins, sum_pnl_pct]
    let mut bins = vec![[0.0_f64; 4]; CONFIDENCE_BINS.len()];
    let mut samples = 0_i64;
    let mut brier_sum = 0.0_f64;

    if let Ok(mut stmt) = conn.prepare(
        "SELECT ai_confidence, pnl, pnl_pct FROM trades \
         WHERE mode=?1 AND closed_at>=?2 AND ai_confidence>0",
    ) {
        if let Ok(rows) = stmt.query_map(params![mode, since], |row| {
            Ok((
                row.get::<_, f64>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, Option<f64>>(2)?,
            ))
        }) {
            for (confidence, pnl, pnl_pct) in rows.flatten() {
                if !(0.0..=1.0).contains(&confidence) {
                    continue;
                }
                let index = bin_index(confidence);
                let win = i64::from(pnl > 0.0);
                bins[index][0] += 1.0;
                bins[index][1] += confidence;
                bins[index][2] += win as f64;
                bins[index][3] += pnl_pct.unwrap_or(0.0);
                samples += 1;
                let outcome = win as f64;
                brier_sum += (confidence - outcome).powi(2);
            }
        }
    }

    let mut bin_rows = Vec::new();
    let mut weighted_gap = 0.0_f64;
    for (index, (_, _, label)) in CONFIDENCE_BINS.iter().enumerate() {
        let [count, sum_confidence, wins, sum_pnl_pct] = bins[index];
        if count == 0.0 {
            bin_rows.push(json!({
                "label": label,
                "samples": 0,
                "mean_confidence": 0.0,
                "win_rate_pct": 0.0,
                "avg_pnl_pct": 0.0,
                "gap_pp": 0.0,
            }));
            continue;
        }
        let mean_confidence = sum_confidence / count;
        let win_rate = wins / count * 100.0;
        let gap_pp = mean_confidence * 100.0 - win_rate;
        weighted_gap += gap_pp.abs() * count;
        bin_rows.push(json!({
            "label": label,
            "samples": count as i64,
            "mean_confidence": round4(mean_confidence),
            "win_rate_pct": round2(win_rate),
            "avg_pnl_pct": round4(sum_pnl_pct / count),
            "gap_pp": round2(gap_pp),
        }));
    }

    json!({
        "samples": samples,
        "bins": bin_rows,
        "expected_calibration_error_pp": if samples == 0 {
            0.0
        } else {
            round2(weighted_gap / samples as f64)
        },
        "brier_score": if samples == 0 {
            0.0
        } else {
            round4(brier_sum / samples as f64)
        },
    })
}

fn bin_index(confidence: f64) -> usize {
    CONFIDENCE_BINS
        .iter()
        .position(|(low, high, _)| confidence >= *low && confidence < *high)
        .unwrap_or(CONFIDENCE_BINS.len() - 1)
}

/// AI 调用成本与延迟；`ai_call_logs` 没有 mode 列，按全环境统计。
fn cost_stats(conn: &Connection, since: i64) -> Value {
    let mut calls = 0_i64;
    let mut ok_calls = 0_i64;
    let mut cost_usd = 0.0_f64;
    let mut latencies = Vec::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT ok, latency_ms, cost_usd FROM ai_call_logs WHERE ts>=?1")
    {
        if let Ok(rows) = stmt.query_map([since], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        }) {
            for (ok, latency_ms, cost) in rows.flatten() {
                calls += 1;
                if ok != 0 {
                    ok_calls += 1;
                }
                if cost.is_finite() {
                    cost_usd += cost;
                }
                if latency_ms > 0 {
                    latencies.push(latency_ms as f64);
                }
            }
        }
    }
    latencies.sort_by(f64::total_cmp);
    let p95 = if latencies.is_empty() {
        0.0
    } else {
        let index = ((latencies.len() as f64 - 1.0) * 0.95).round() as usize;
        latencies[index.min(latencies.len() - 1)]
    };
    let avg = if latencies.is_empty() {
        0.0
    } else {
        latencies.iter().sum::<f64>() / latencies.len() as f64
    };
    json!({
        "calls": calls,
        "ok_calls": ok_calls,
        "cost_usd": round4(cost_usd),
        "avg_latency_ms": round2(avg),
        "p95_latency_ms": round2(p95),
        "scope": "all_modes",
    })
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE signals(ts INTEGER NOT NULL, symbol TEXT NOT NULL, direction TEXT NOT NULL, \
               decision TEXT NOT NULL, ai_action TEXT NOT NULL, ai_confidence REAL NOT NULL DEFAULT 0, \
               features TEXT, mode TEXT NOT NULL); \
             CREATE TABLE market_candles(symbol TEXT NOT NULL, close_time INTEGER NOT NULL, close REAL NOT NULL); \
             CREATE TABLE trades(pnl REAL NOT NULL, pnl_pct REAL, ai_confidence REAL NOT NULL, \
               notional REAL, closed_at INTEGER NOT NULL, mode TEXT NOT NULL); \
             CREATE TABLE ai_call_logs(ts INTEGER NOT NULL, ok INTEGER NOT NULL, latency_ms INTEGER NOT NULL, cost_usd REAL NOT NULL);",
        )
        .unwrap();
        conn
    }

    fn vetoed_signal(conn: &Connection, ts: i64, direction: &str, features: &str) {
        conn.execute(
            "INSERT INTO signals(ts,symbol,direction,decision,ai_action,ai_confidence,features,mode) \
             VALUES(?1,'BTCUSDT',?2,'rejected','hold',0.8,?3,'demo')",
            params![ts, direction, features],
        )
        .unwrap();
    }

    #[test]
    fn counterfactual_pnl_uses_direction_and_marks_avoided_loss() {
        let conn = conn();
        conn.execute_batch(
            "INSERT INTO market_candles VALUES('BTCUSDT',900000,100.0); \
             INSERT INTO market_candles VALUES('BTCUSDT',87000000,95.0); \
             INSERT INTO trades VALUES(10.0,2.0,0.9,500.0,50000,'demo');",
        )
        .unwrap();
        vetoed_signal(
            &conn,
            1_000,
            "long",
            r#"{"insight":{"vetoed_by":"ai_veto"}}"#,
        );

        let value = stats(&conn, "demo", 100_000, 1_000_000);

        assert_eq!(value["counterfactual"]["vetoed_signals"], 1);
        assert_eq!(value["counterfactual"]["samples"], 1);
        assert_eq!(value["counterfactual"]["skipped"], 0);
        assert!(
            (value["counterfactual"]["avg_pnl_pct"].as_f64().unwrap() + 5.0).abs() < 1e-9,
            "做多被否决后跌 5%%，反事实收益应为 -5%%: {value}"
        );
        assert_eq!(value["counterfactual"]["hit_rate_pct"], 100.0);
        assert!(
            (value["counterfactual"]["avoided_loss_usd"]
                .as_f64()
                .unwrap()
                - 25.0)
                .abs()
                < 1e-9,
            "5%% × 500 名义本金 = 25 USD: {value}"
        );
    }

    #[test]
    fn short_veto_wins_when_price_rises() {
        let conn = conn();
        conn.execute_batch(
            "INSERT INTO market_candles VALUES('BTCUSDT',900000,100.0); \
             INSERT INTO market_candles VALUES('BTCUSDT',87000000,104.0);",
        )
        .unwrap();
        vetoed_signal(
            &conn,
            1_000,
            "short",
            r#"{"insight":{"vetoed_by":"ai_veto"}}"#,
        );

        let value = stats(&conn, "demo", 100_000, 1_000_000);

        assert!(
            (value["counterfactual"]["avg_pnl_pct"].as_f64().unwrap() + 4.0).abs() < 1e-9,
            "做空被否决后涨 4%%，反事实收益应为 -4%%: {value}"
        );
        assert_eq!(value["counterfactual"]["hit_rate_pct"], 100.0);
    }

    #[test]
    fn missing_candles_are_skipped_instead_of_zeroed() {
        let conn = conn();
        vetoed_signal(
            &conn,
            1_000,
            "long",
            r#"{"insight":{"vetoed_by":"ai_veto"}}"#,
        );

        let value = stats(&conn, "demo", 100_000, 1_000_000);

        assert_eq!(value["counterfactual"]["vetoed_signals"], 1);
        assert_eq!(value["counterfactual"]["samples"], 0);
        assert_eq!(value["counterfactual"]["skipped"], 1);
        assert_eq!(value["counterfactual"]["avg_pnl_pct"], 0.0);
    }

    #[test]
    fn opened_or_non_ai_rejections_are_not_counted() {
        let conn = conn();
        conn.execute(
            "INSERT INTO signals(ts,symbol,direction,decision,ai_action,ai_confidence,features,mode) \
             VALUES(1000,'BTCUSDT','long','opened','open_long',0.9,'{}','demo')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO signals(ts,symbol,direction,decision,ai_action,ai_confidence,features,mode) \
             VALUES(1001,'BTCUSDT','long','rejected','none',0.0,?1,'demo')",
            [json!({"insight": {"vetoed_by": "funding_veto"}}).to_string()],
        )
        .unwrap();

        let value = stats(&conn, "demo", 100_000, 1_000_000);

        assert_eq!(value["counterfactual"]["vetoed_signals"], 0);
    }

    #[test]
    fn confidence_calibration_reports_bins_and_errors() {
        let conn = conn();
        conn.execute_batch(
            "INSERT INTO trades VALUES(10.0,2.0,0.9,500.0,50000,'demo'); \
             INSERT INTO trades VALUES(-5.0,-1.0,0.9,500.0,50000,'demo'); \
             INSERT INTO trades VALUES(8.0,1.5,0.55,500.0,50000,'demo'); \
             INSERT INTO trades VALUES(-6.0,-1.2,0.55,500.0,50000,'demo');",
        )
        .unwrap();

        let value = stats(&conn, "demo", 100_000, 1_000_000);
        let confidence = &value["confidence"];

        assert_eq!(confidence["samples"], 4);
        assert_eq!(confidence["bins"][1]["samples"], 2);
        assert_eq!(confidence["bins"][1]["win_rate_pct"], 50.0);
        assert_eq!(confidence["bins"][3]["samples"], 2);
        assert_eq!(confidence["bins"][3]["gap_pp"], 40.0);
        assert!(
            (confidence["expected_calibration_error_pp"]
                .as_f64()
                .unwrap()
                - 22.5)
                .abs()
                < 1e-9,
            "ECE 应为 (40+5)/2 = 22.5pp: {confidence}"
        );
        assert!(
            (confidence["brier_score"].as_f64().unwrap() - 0.3313).abs() < 1e-3,
            "Brier 分数必须反映置信度偏离: {confidence}"
        );
    }

    #[test]
    fn cost_stats_sums_calls_cost_and_latency() {
        let conn = conn();
        conn.execute_batch(
            "INSERT INTO ai_call_logs VALUES(90000,1,500,0.01); \
             INSERT INTO ai_call_logs VALUES(91000,1,1500,0.02); \
             INSERT INTO ai_call_logs VALUES(92000,0,0,0.0);",
        )
        .unwrap();

        let value = stats(&conn, "demo", 100_000, 86_400);
        let cost = &value["cost"];

        assert_eq!(cost["calls"], 3);
        assert_eq!(cost["ok_calls"], 2);
        assert_eq!(cost["cost_usd"], 0.03);
        assert_eq!(cost["p95_latency_ms"], 1500.0);
        assert_eq!(cost["scope"], "all_modes");
    }
}
