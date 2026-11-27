//! 信号审计落库。
//!
//! 每轮扫描都要留下可追溯的一行：量化评分、AI 复核结论、没开仓的原因。
//! 引擎 15 秒一轮、标的池十几个，直接 insert 会把表写爆，
//! 所以相同信号合并成一行，用 repeats 记真实扫描次数——
//! 「扫描密度」看的是 repeats 累加，不是行数。

use std::collections::BTreeMap;

use rusqlite::{Connection, Transaction};
use serde_json::{json, Value};

/// 相同信号的合并窗口（秒）。窗口内同一标的同一结论只留一行。
pub const MERGE_WINDOW_SEC: i64 = 300;

/// 一条审计记录。
#[derive(Debug, Clone)]
pub struct AuditSignal {
    pub symbol: String,
    /// long / short / flat。
    pub direction: String,
    pub score: f64,
    pub features: Value,
    /// AI 的动作：open_long / open_short / hold / reject / none。
    pub ai_action: String,
    pub ai_confidence: f64,
    pub ai_reason: String,
    /// opened / rejected / flat / pending。
    pub decision: String,
    pub reject_reason: String,
    /// gate：行情闸门初筛拒绝，未进入评分与 AI 复核；空串表示已进入后续阶段。
    pub stage: String,
    /// AI 自评的数据质量分（dq）。
    pub data_quality: Option<f64>,
    /// AI 估计的盈亏比（rr）。
    pub rr_target: Option<f64>,
    /// AI 估计的有效 K 线根数（vb）。
    pub valid_bars: Option<i64>,
    /// AI 给出的失效价（ip）。
    pub invalidation_price: Option<f64>,
    /// AI 建议的入场价（ep）。
    pub entry_price: Option<f64>,
    /// AI 给出的失效条件文本（inv）。
    pub invalidation_text: Option<String>,
    /// AI 给出的风险提示文本（risk）。
    pub risk_text: Option<String>,
    /// AI 给出的下单数量缩放系数（0~1）。
    pub scale: Option<f64>,
    /// 供应商 HTTP 耗时（毫秒）。
    pub provider_ms: Option<i64>,
    /// 排队等待耗时（毫秒）：总耗时扣除供应商与解析。
    pub queue_ms: Option<i64>,
    /// 整次复核总耗时（毫秒）。
    pub wall_ms: Option<i64>,
    /// 本次复核使用的提示词档位：full / slim。
    pub prompt_profile: Option<String>,
}

impl AuditSignal {
    pub fn new(symbol: &str) -> Self {
        Self {
            symbol: symbol.to_string(),
            direction: "flat".to_string(),
            score: 0.0,
            features: json!({}),
            ai_action: "none".to_string(),
            ai_confidence: 0.0,
            ai_reason: String::new(),
            decision: "flat".to_string(),
            reject_reason: String::new(),
            stage: String::new(),
            data_quality: None,
            rr_target: None,
            valid_bars: None,
            invalidation_price: None,
            entry_price: None,
            invalidation_text: None,
            risk_text: None,
            scale: None,
            provider_ms: None,
            queue_ms: None,
            wall_ms: None,
            prompt_profile: None,
        }
    }

    pub fn with_direction(mut self, direction: &str) -> Self {
        self.direction = direction.to_string();
        self
    }

    pub fn with_score(mut self, score: f64) -> Self {
        self.score = score;
        self
    }

    pub fn with_features(mut self, features: Value) -> Self {
        self.features = features;
        self
    }

    pub fn with_ai(mut self, action: &str, confidence: f64, reason: &str) -> Self {
        self.ai_action = action.to_string();
        self.ai_confidence = confidence;
        self.ai_reason = reason.to_string();
        self
    }

    pub fn with_decision(mut self, decision: &str) -> Self {
        self.decision = decision.to_string();
        self
    }

    pub fn with_reject_reason(mut self, reason: &str) -> Self {
        self.reject_reason = reason.to_string();
        self
    }

    pub fn with_stage(mut self, stage: &str) -> Self {
        self.stage = stage.to_string();
        self
    }

    /// 往 features 里补一个字段（例如 client_order_id），不影响已有读数。
    pub fn with_feature(mut self, key: &str, value: Value) -> Self {
        if let Some(object) = self.features.as_object_mut() {
            object.insert(key.to_string(), value);
        }
        self
    }

    /// 在原有 features 基础上并入 AI 复核扩展字段，供审计与前端查询。
    /// 未取到的字段不写入，避免用空值覆盖已有读数。
    /// 旧 AI 字段一律先清除，保证 features 里的自评字段只反映最新一轮。
    pub fn features_with_meta(&self) -> Value {
        let mut features = self.features.clone();
        // 仅对象形状的 features 会并入 AI 字段；非对象入参原样返回。
        if let Some(object) = features.as_object_mut() {
            for key in AI_META_KEYS {
                object.remove(key);
            }
            if let Some(v) = self.data_quality {
                object.insert("ai_dq".into(), json!(v));
            }
            if let Some(v) = self.rr_target {
                object.insert("ai_rr".into(), json!(v));
            }
            if let Some(v) = self.valid_bars {
                object.insert("ai_vb".into(), json!(v));
            }
            if let Some(v) = self.invalidation_price {
                object.insert("ai_ip".into(), json!(v));
            }
            if let Some(v) = self.entry_price {
                object.insert("ai_ep".into(), json!(v));
            }
            if let Some(v) = self.invalidation_text.as_ref() {
                object.insert("ai_inv".into(), json!(v));
            }
            if let Some(v) = self.risk_text.as_ref() {
                object.insert("ai_risk".into(), json!(v));
            }
            if let Some(v) = self.scale {
                object.insert("ai_scale".into(), json!(v));
            }
            if let Some(wall) = self.wall_ms {
                object.insert(
                    "ai_latency".into(),
                    json!({
                        "wall_ms": wall,
                        "provider_ms": self.provider_ms,
                        "queue_ms": self.queue_ms.unwrap_or(0),
                    }),
                );
            }
            if let Some(profile) = self.prompt_profile.as_ref() {
                object.insert("ai_prompt_profile".into(), json!(profile));
            }
            // 统一决策追踪：一层一行就能看清走到哪、被谁拦、AI 想做什么。
            let mut trace = json!({
                "stage": self.stage,
                "decision": self.decision,
                "ai_action": self.ai_action,
                "gate": classify_gate(&self.reject_reason, &self.stage),
                "reject_reason": self.reject_reason,
                "scale": self.scale,
            });
            if self.reject_reason.contains("ai_skip=high_score")
                || self.ai_reason.contains("ai_skip=high_score")
            {
                if let Some(trace) = trace.as_object_mut() {
                    trace.insert("skip".into(), json!(true));
                }
            }
            object.insert("filter_trace".into(), trace);
            // 结构化洞察：方向、幅度、置信度、有效期、否决方一眼可读。
            object.insert("insight".into(), json!(self.insight()));
        }
        features
    }

    /// 把审计字段收敛成 Insight 对象；`signals.features.insight` 与决策日志共用同一形状。
    pub fn insight(&self) -> crate::insight::Insight {
        crate::insight::Insight {
            direction: self.direction.clone(),
            magnitude: self.score.abs().clamp(0.0, 1.0),
            confidence: self.ai_confidence,
            valid_bars: self.valid_bars.map(|value| value.max(0) as u64),
            vetoed_by: self.veto_source(),
            action: self.normalized_action(),
            reason: if self.reject_reason.trim().is_empty() {
                self.ai_reason.clone()
            } else {
                self.reject_reason.clone()
            },
        }
    }

    fn normalized_action(&self) -> String {
        let action = self.ai_action.trim();
        if !action.is_empty() && action != "none" {
            return action.to_string();
        }
        if self.decision == "opened" {
            return if self.direction == "short" {
                "open_short".to_string()
            } else {
                "open_long".to_string()
            };
        }
        if self.decision == "rejected" {
            "reject".to_string()
        } else {
            "hold".to_string()
        }
    }

    /// 把闸门类别映射成 Insight 的否决方枚举；无拒绝原因时为 None。
    fn veto_source(&self) -> Option<String> {
        let source = match classify_gate(&self.reject_reason, &self.stage) {
            "none" => return None,
            "funding_veto" => "funding_veto",
            "news_veto" => "news_veto",
            "score_split_low" => "score_split_low",
            "ai_fail_open" => "ai_fail_open",
            "ai_dq" | "ai_rr" | "ai_confidence" | "ai_direction" => "ai_veto",
            _ => "risk_gate",
        };
        Some(source.to_string())
    }
}

/// 把拒绝原因归到稳定的闸门类别；前缀匹配保证同一类原因不会因数字变化而漂移。
fn classify_gate(reason: &str, _stage: &str) -> &'static str {
    const PREFIXES: [(&str, &str); 11] = [
        ("资金费率不利", "funding_veto"),
        ("负面事件否决", "news_veto"),
        ("同方向持仓已达上限", "direction_limit"),
        ("高相关同方向持仓过多", "correlation_limit"),
        ("评分低于 AI 分流下限", "score_split_low"),
        ("AI 复核失败", "ai_fail_open"),
        ("AI 数据质量分", "ai_dq"),
        ("AI 盈亏比", "ai_rr"),
        ("AI 置信度", "ai_confidence"),
        ("方向不一致", "ai_direction"),
        ("AI 方向", "ai_direction"),
    ];
    if reason.trim().is_empty() {
        return "none";
    }
    for (prefix, gate) in PREFIXES {
        if reason.starts_with(prefix) {
            return gate;
        }
    }
    "other"
}

/// 拒绝原因归一化：去掉数字与比较符号，只留「是哪一类原因」。
/// 「波动率过低 0.03% < 0.05%」和「波动率过低 0.04% < 0.05%」是同一个原因，
/// 不归一化的话每轮都是新的一行，表会被写爆。
fn reason_key(reason: &str) -> String {
    reason
        .chars()
        .filter(|c| {
            !c.is_ascii_digit()
                && !matches!(
                    c,
                    '.' | '%' | '<' | '>' | '=' | ' ' | ',' | ':' | '_' | '-' | '+' | '/'
                )
        })
        .collect()
}

/// 从已落库的行里读回归一化原因。
fn stored_key(features: &str) -> Option<String> {
    let value: Value = serde_json::from_str(features).ok()?;
    value
        .get("reason_key")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 数值型 AI 自评字段：合并时新行缺失则沿用旧值，避免数据丢失。
const AI_NUMERIC_META_KEYS: [&str; 5] = ["ai_dq", "ai_rr", "ai_vb", "ai_ip", "ai_ep"];

/// 按轮次生效的 AI 自评字段：以最新一轮为准，新行缺失时不得残留旧值。
/// ai_scale 只代表当前轮的下单缩放，跨轮沿用会与 filter_trace.scale 口径不一致；
/// ai_latency 同理，跨轮沿用会让延迟样本被旧值污染。
const AI_ROUND_META_KEYS: [&str; 5] = [
    "ai_inv",
    "ai_risk",
    "ai_scale",
    "ai_latency",
    "ai_prompt_profile",
];

/// 写入 signals.features 的 AI 自评字段键（数值 + 按轮次）。
const AI_META_KEYS: [&str; AI_NUMERIC_META_KEYS.len() + AI_ROUND_META_KEYS.len()] = {
    let mut keys = [""; AI_NUMERIC_META_KEYS.len() + AI_ROUND_META_KEYS.len()];
    let mut index = 0;
    while index < AI_NUMERIC_META_KEYS.len() {
        keys[index] = AI_NUMERIC_META_KEYS[index];
        index += 1;
    }
    let mut offset = 0;
    while offset < AI_ROUND_META_KEYS.len() {
        keys[index + offset] = AI_ROUND_META_KEYS[offset];
        offset += 1;
    }
    keys
};

/// 合并时用旧值补齐数值字段；按轮次字段以最新一轮为准，缺失时删除旧键。
fn fill_missing_ai_meta(features: &mut Value, previous_text: &str) {
    let previous: Value = match serde_json::from_str(previous_text) {
        Ok(value) => value,
        Err(_) => return,
    };
    let Some(previous_object) = previous.as_object() else {
        return;
    };
    let Some(object) = features.as_object_mut() else {
        return;
    };
    // 数值字段：新行缺失时沿用旧值。
    for key in AI_NUMERIC_META_KEYS {
        if !object.contains_key(key) {
            if let Some(value) = previous_object.get(key) {
                object.insert(key.to_string(), value.clone());
            }
        }
    }
    // 按轮次字段：新一轮没给就删掉旧键，不得跨轮残留。
    for key in AI_ROUND_META_KEYS {
        if !object.contains_key(key) {
            object.remove(key);
        }
    }
}

/// 落库一批审计记录。返回受影响的行数（新增 + 合并）。
pub fn record(
    tx: &Transaction<'_>,
    mode: &str,
    ts: i64,
    rows: &[AuditSignal],
) -> Result<usize, String> {
    let mut affected = 0usize;
    for row in rows {
        if row.symbol.trim().is_empty() {
            continue;
        }
        let key = reason_key(&row.reject_reason);
        let mergeable = find_mergeable(tx, mode, row, ts, &key)?;
        let mut features = row.features_with_meta();
        // 合并时先用旧行的 AI 自评字段补齐新行缺失的键，再写入本轮的新值。
        if let Some((_, previous_features)) = mergeable.as_ref() {
            fill_missing_ai_meta(&mut features, previous_features);
        }
        if let Some(object) = features.as_object_mut() {
            object.insert("reason_key".to_string(), Value::from(key.clone()));
            if !row.stage.is_empty() {
                object.insert("stage".to_string(), Value::from(row.stage.clone()));
            }
        }
        // 序列化失败时退回旧行文本，至少保住上一次的 features 与 reason_key。
        let features_text = serde_json::to_string(&features).unwrap_or_else(|_| {
            mergeable
                .as_ref()
                .map(|(_, previous)| previous.clone())
                .unwrap_or_else(|| "{}".to_string())
        });
        if let Some((id, _)) = mergeable {
            tx.execute(
                "UPDATE signals SET ts=?1, score=?2, features=?3, ai_confidence=?4, ai_reason=?5, \
                 repeats=repeats+1 WHERE id=?6",
                rusqlite::params![
                    ts,
                    row.score,
                    features_text,
                    row.ai_confidence,
                    row.ai_reason,
                    id,
                ],
            )
            .map_err(|error| format!("合并信号审计失败: {error}"))?;
        } else {
            tx.execute(
                "INSERT INTO signals(ts,symbol,direction,score,features,ai_action,ai_confidence, \
                 ai_reason,decision,reject_reason,mode,repeats) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,1)",
                rusqlite::params![
                    ts,
                    row.symbol,
                    row.direction,
                    row.score,
                    features_text,
                    row.ai_action,
                    row.ai_confidence,
                    row.ai_reason,
                    row.decision,
                    row.reject_reason,
                    mode,
                ],
            )
            .map_err(|error| format!("写入信号审计失败: {error}"))?;
        }
        affected += 1;
    }
    Ok(affected)
}

/// 找出可以合并的旧行：同环境、同标的、同方向、同结论、同原因类别。
fn find_mergeable(
    tx: &Transaction<'_>,
    mode: &str,
    row: &AuditSignal,
    ts: i64,
    key: &str,
) -> Result<Option<(i64, String)>, String> {
    let mut stmt = tx
        .prepare(
            "SELECT id, features, reject_reason FROM signals WHERE mode=?1 AND symbol=?2 \
             AND direction=?3 AND decision=?4 AND ai_action=?5 AND ts>=?6 \
             ORDER BY ts DESC LIMIT 32",
        )
        .map_err(|error| format!("查询信号审计失败: {error}"))?;
    let candidates = stmt
        .query_map(
            rusqlite::params![
                mode,
                row.symbol,
                row.direction,
                row.decision,
                row.ai_action,
                ts - MERGE_WINDOW_SEC,
            ],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|error| format!("查询信号审计失败: {error}"))?;
    for candidate in candidates.flatten() {
        let (id, features, reason) = candidate;
        let stored = stored_key(&features).unwrap_or_else(|| reason_key(&reason));
        if stored == key {
            return Ok(Some((id, features)));
        }
    }
    Ok(None)
}

/// 半 AI 运行指标的闸门白名单：顺序与前端展示一致。
const GATE_KEYS: [&str; 12] = [
    "funding_veto",
    "news_veto",
    "direction_limit",
    "correlation_limit",
    "score_split_low",
    "ai_fail_open",
    "ai_dq",
    "ai_rr",
    "ai_confidence",
    "ai_direction",
    "other",
    "none",
];

/// AI 主动收紧/否决的闸门：命中即计入 AI 否决率。
const AI_VETO_GATES: [&str; 4] = ["ai_dq", "ai_rr", "ai_confidence", "ai_direction"];

/// 最近窗口内一条信号在聚合时需要的字段。
struct SignalMetricRow {
    decision: String,
    ai_action: String,
    gate: Option<String>,
    skip: bool,
    scale: Option<f64>,
    /// 整次复核总耗时（毫秒），来自 features.ai_latency.wall_ms。
    wall_ms: Option<f64>,
    /// 复核排队耗时（毫秒），来自 features.ai_latency.queue_ms。
    queue_ms: Option<f64>,
    /// 提示词档位，来自 features.ai_prompt_profile。
    prompt_profile: Option<String>,
    /// 是否已带结构化 insight。
    has_insight: bool,
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// 最近 `window_sec` 的半 AI 运行指标：信号/决议、scale 分布、fail-open、
/// 闸门分层、AI 延迟与盘口健康度。只读聚合，不改交易行为。
///
/// 信号按 `mode` 隔离，与 `/engine/stats` 其他字段一致；`ai_call_logs` 没有
/// mode 列，AI 延迟只能取全环境样本，返回值用 `ai_latency_scope` 注明。
///
/// `features` 优先用 SQLite `json_extract` 展平；老库缺少 JSON1 或存了坏 JSON 时，
/// 回退到 Rust 侧逐行解析，保证接口不因数据脏而失败。
pub fn half_ai_stats(conn: &Connection, mode: &str, now: i64, window_sec: i64) -> Value {
    let since = now.saturating_sub(window_sec.max(0));
    let mut signals_total = 0_i64;
    let mut opened = 0_i64;
    let mut rejected = 0_i64;
    let mut ai_reviewed = 0_i64;
    let mut ai_veto = 0_i64;
    let mut ai_fail_open = 0_i64;
    let mut high_score_skip = 0_i64;
    let mut ai_prompt_full = 0_i64;
    let mut ai_prompt_slim = 0_i64;
    // [0.5, 0.75, 0.9~1.0, none]
    let mut scales = [0_i64; 4];
    let mut gates: BTreeMap<&'static str, i64> =
        GATE_KEYS.iter().map(|key| (*key, 0_i64)).collect();
    let mut walls: Vec<f64> = Vec::new();
    let mut queues: Vec<f64> = Vec::new();
    let mut insight_signals = 0_i64;

    for row in signal_metric_rows(conn, mode, since) {
        signals_total += 1;
        if row.has_insight {
            insight_signals += 1;
        }
        if let Some(ms) = row.wall_ms {
            walls.push(ms);
        }
        if let Some(ms) = row.queue_ms {
            queues.push(ms);
        }
        match row.decision.as_str() {
            "opened" => opened += 1,
            "rejected" => rejected += 1,
            _ => {}
        }
        let gate = row.gate.as_deref().unwrap_or("none");
        let gate_key = if gates.contains_key(gate) {
            gate
        } else {
            "other"
        };
        if let Some(slot) = gates.get_mut(gate_key) {
            *slot += 1;
        }
        if gate_key == "ai_fail_open" {
            ai_fail_open += 1;
        }
        if row.skip {
            high_score_skip += 1;
        }
        match row.prompt_profile.as_deref() {
            Some("full") => ai_prompt_full += 1,
            Some("slim") => ai_prompt_slim += 1,
            _ => {}
        }
        let action = row.ai_action.trim();
        // AI 实际复核过的信号：有动作且不是 none/error，作为否决率分母。
        if !action.is_empty() && action != "none" && action != "error" {
            ai_reviewed += 1;
        }
        if AI_VETO_GATES.contains(&gate_key) || action == "reject" || action == "hold" {
            ai_veto += 1;
        }
        scales[scale_bucket(row.scale)] += 1;
    }

    let ai_veto_rate = if ai_reviewed == 0 {
        0.0
    } else {
        ai_veto as f64 / ai_reviewed as f64 * 100.0
    };

    let mut latencies: Vec<f64> = Vec::new();
    let mut by_provider: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let (latency_rows, latency_scoped) = latency_samples(conn, since, mode);
    for (latency, provider) in latency_rows {
        let ms = latency as f64;
        // 空 provider 归入 unknown，避免分供应商视图出现空 key。
        let key = provider
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "unknown".to_string());
        latencies.push(ms);
        by_provider.entry(key).or_default().push(ms);
    }
    latencies.sort_by(f64::total_cmp);
    walls.sort_by(f64::total_cmp);
    queues.sort_by(f64::total_cmp);
    let max_latency = latencies.last().map(|ms| round2(*ms)).unwrap_or(0.0);
    let mut provider_stats = serde_json::Map::new();
    for (provider, samples) in by_provider.iter_mut() {
        samples.sort_by(f64::total_cmp);
        provider_stats.insert(provider.clone(), latency_summary(samples));
    }

    let book = crate::book_stream::stats();
    let book_queries = book["queries"].as_u64().unwrap_or(0);
    let book_stale = book["stale"].as_u64().unwrap_or(0);
    let book_stale_pct = if book_queries == 0 {
        0.0
    } else {
        round2(book_stale as f64 / book_queries as f64 * 100.0)
    };

    json!({
        "window_sec": window_sec.max(0),
        "signals_total": signals_total,
        "opened": opened,
        "rejected": rejected,
        "ai_reviewed": ai_reviewed,
        "ai_veto_rate_pct": round2(ai_veto_rate),
        "scale_0_5": scales[0],
        "scale_0_75": scales[1],
        "scale_0_9_1": scales[2],
        "scale_none": scales[3],
        "ai_fail_open": ai_fail_open,
        "high_score_skip": high_score_skip,
        "ai_prompt_full": ai_prompt_full,
        "ai_prompt_slim": ai_prompt_slim,
        "insight_coverage": if signals_total == 0 {
            0.0
        } else {
            round2(insight_signals as f64 / signals_total as f64)
        },
        "gates": gates,
        "ai_latency_samples": latencies.len(),
        "ai_latency_p50_ms": percentile(&latencies, 0.50),
        "ai_latency_p95_ms": percentile(&latencies, 0.95),
        "ai_latency_p99_ms": percentile(&latencies, 0.99),
        "ai_latency_max_ms": max_latency,
        "ai_latency_percentile_method": "nearest_rank",
        "ai_latency_by_provider": provider_stats,
        "ai_latency_scope": if latency_scoped { "mode" } else { "all_modes" },
        "ai_wall_samples": walls.len(),
        "ai_wall_p50_ms": percentile(&walls, 0.50),
        "ai_wall_p95_ms": percentile(&walls, 0.95),
        "ai_wall_p99_ms": percentile(&walls, 0.99),
        "ai_queue_samples": queues.len(),
        "ai_queue_p50_ms": percentile(&queues, 0.50),
        "ai_queue_p95_ms": percentile(&queues, 0.95),
        "ai_queue_p99_ms": percentile(&queues, 0.99),
        "book_stale_pct": book_stale_pct,
        "book": book,
    })
}

/// 读取 AI 调用延迟样本：优先按 mode 过滤；老库/测试夹具缺 mode 列时回退全环境。
///
/// 返回 `(样本, 是否按 mode 过滤)`；调用方用后者标注 `ai_latency_scope`。
fn latency_samples(
    conn: &Connection,
    since: i64,
    mode: &str,
) -> (Vec<(i64, Option<String>)>, bool) {
    let row =
        |row: &rusqlite::Row<'_>| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?));
    if let Ok(mut stmt) = conn.prepare(
        "SELECT latency_ms, provider FROM ai_call_logs \
         WHERE ts >= ?1 AND latency_ms > 0 AND mode = ?2",
    ) {
        if let Ok(rows) = stmt.query_map(rusqlite::params![since, mode], row) {
            return (rows.flatten().collect(), true);
        }
    }
    if let Ok(mut stmt) = conn
        .prepare("SELECT latency_ms, provider FROM ai_call_logs WHERE ts >= ?1 AND latency_ms > 0")
    {
        if let Ok(rows) = stmt.query_map([since], row) {
            return (rows.flatten().collect(), false);
        }
    }
    (Vec::new(), false)
}

/// 一组已排序延迟样本的分位数摘要；`samples` 用来看清尾分位在小样本下的可信度。
fn latency_summary(sorted: &[f64]) -> Value {
    json!({
        "samples": sorted.len(),
        "p50_ms": percentile(sorted, 0.50),
        "p95_ms": percentile(sorted, 0.95),
        "p99_ms": percentile(sorted, 0.99),
    })
}

/// 已排序样本的分位值；空样本返回 0，避免接口出现 null。
fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 * q).floor() as usize).min(sorted.len() - 1);
    round2(sorted[index])
}

/// scale 分桶：0.5 / 0.75 / 0.9~1.0，其余（含缺失）归 none。
fn scale_bucket(scale: Option<f64>) -> usize {
    let Some(scale) = scale.filter(|value| value.is_finite()) else {
        return 3;
    };
    if (scale - 0.5).abs() < 0.01 {
        0
    } else if (scale - 0.75).abs() < 0.01 {
        1
    } else if (0.9..=1.0).contains(&scale) {
        2
    } else {
        3
    }
}

fn signal_metric_rows(conn: &Connection, mode: &str, since: i64) -> Vec<SignalMetricRow> {
    json_extract_rows(conn, mode, since).unwrap_or_else(|| features_rows(conn, mode, since))
}

/// JSON1 可用的主路径：把 features 拆成列，聚合仍在 Rust 侧做。
fn json_extract_rows(conn: &Connection, mode: &str, since: i64) -> Option<Vec<SignalMetricRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT decision, ai_action, \
             json_extract(features, '$.filter_trace.gate'), \
             json_extract(features, '$.filter_trace.skip'), \
             json_extract(features, '$.ai_scale'), \
             json_extract(features, '$.ai_latency.wall_ms'), \
             json_extract(features, '$.ai_latency.queue_ms'), \
             json_extract(features, '$.ai_prompt_profile'), \
             json_extract(features, '$.insight') IS NOT NULL \
             FROM signals WHERE ts >= ?1 AND mode = ?2",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![since, mode], |r| {
            Ok(SignalMetricRow {
                decision: r.get(0)?,
                ai_action: r.get(1)?,
                gate: r.get(2)?,
                skip: r
                    .get::<_, Option<i64>>(3)?
                    .map(|flag| flag != 0)
                    .unwrap_or(false),
                scale: r.get(4)?,
                wall_ms: r.get(5)?,
                queue_ms: r.get(6)?,
                prompt_profile: r.get(7)?,
                has_insight: r
                    .get::<_, Option<i64>>(8)?
                    .map(|flag| flag != 0)
                    .unwrap_or(false),
            })
        })
        .ok()?;
    rows.collect::<Result<Vec<_>, _>>().ok()
}

/// 回退路径：整行读出 features 后按 JSON 结构解析。
fn features_rows(conn: &Connection, mode: &str, since: i64) -> Vec<SignalMetricRow> {
    let Ok(mut stmt) = conn
        .prepare("SELECT decision, ai_action, features FROM signals WHERE ts >= ?1 AND mode = ?2")
    else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(rusqlite::params![since, mode], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .map(|(decision, ai_action, features)| {
            let value: Value = features
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok())
                .unwrap_or(Value::Null);
            let trace = &value["filter_trace"];
            SignalMetricRow {
                decision,
                ai_action,
                gate: trace["gate"].as_str().map(str::to_string),
                skip: trace["skip"].as_bool().unwrap_or(false),
                scale: value["ai_scale"].as_f64(),
                wall_ms: value["ai_latency"]["wall_ms"].as_f64(),
                queue_ms: value["ai_latency"]["queue_ms"].as_f64(),
                prompt_profile: value["ai_prompt_profile"].as_str().map(str::to_string),
                has_insight: value.get("insight").is_some(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn table() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE signals(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, direction TEXT, \
             score REAL, features TEXT, ai_action TEXT, ai_confidence REAL, ai_reason TEXT, \
             decision TEXT, reject_reason TEXT, mode TEXT, repeats INTEGER); \
             CREATE TABLE ai_call_logs(id INTEGER PRIMARY KEY, ts INTEGER, provider TEXT, model TEXT, \
             purpose TEXT, ok INTEGER, latency_ms INTEGER, prompt_tokens INTEGER, completion_tokens INTEGER, \
             cost_usd REAL, error TEXT, raw_text TEXT);",
        )
        .unwrap();
        conn
    }

    fn insert_ai_call(conn: &Connection, ts: i64, provider: &str, latency_ms: i64) {
        conn.execute(
            "INSERT INTO ai_call_logs(ts, provider, latency_ms) VALUES(?1, ?2, ?3)",
            rusqlite::params![ts, provider, latency_ms],
        )
        .unwrap();
    }

    fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM signals", [], |r| r.get(0))
            .unwrap()
    }

    fn repeats(conn: &Connection) -> i64 {
        conn.query_row("SELECT repeats FROM signals LIMIT 1", [], |r| r.get(0))
            .unwrap()
    }

    fn insert_signal(conn: &Connection, ts: i64, decision: &str, ai_action: &str, features: &str) {
        insert_signal_in(conn, "demo", ts, decision, ai_action, features);
    }

    fn insert_signal_in(
        conn: &Connection,
        mode: &str,
        ts: i64,
        decision: &str,
        ai_action: &str,
        features: &str,
    ) {
        conn.execute(
            "INSERT INTO signals(ts,symbol,direction,score,features,ai_action,ai_confidence,\
             ai_reason,decision,reject_reason,mode,repeats) \
             VALUES(?1,'BTCUSDT','long',0.8,?2,?3,0.7,'',?4,'',?5,1)",
            rusqlite::params![ts, features, ai_action, decision, mode],
        )
        .unwrap();
    }

    #[test]
    fn insight_is_merged_into_features_with_veto_source() {
        let row = AuditSignal::new("BTCUSDT")
            .with_direction("long")
            .with_score(-0.4)
            .with_ai("reject", 0.3, "弱势")
            .with_decision("rejected")
            .with_stage("gate")
            .with_reject_reason("负面事件否决：交易所被调查");
        let features = row.features_with_meta();
        assert_eq!(features["insight"]["direction"], "long");
        assert_eq!(features["insight"]["magnitude"], 0.4);
        assert_eq!(features["insight"]["confidence"], 0.3);
        assert_eq!(features["insight"]["action"], "reject");
        assert_eq!(features["insight"]["vetoed_by"], "news_veto");
        assert_eq!(features["insight"]["reason"], "负面事件否决：交易所被调查");
    }

    #[test]
    fn half_ai_stats_reports_insight_coverage() {
        let conn = table();
        let now = 400_000;
        insert_signal(
            &conn,
            now - 10,
            "opened",
            "open_long",
            r#"{"insight":{"direction":"long","magnitude":0.8},"filter_trace":{"gate":"none"}}"#,
        );
        insert_signal(
            &conn,
            now - 20,
            "rejected",
            "reject",
            r#"{"filter_trace":{"gate":"ai_dq"}}"#,
        );
        let value = half_ai_stats(&conn, "demo", now, 86_400);
        assert_eq!(value["signals_total"], 2);
        assert_eq!(value["insight_coverage"], 0.5);
    }

    #[test]
    fn half_ai_stats_counts_gates_and_scales() {
        let conn = table();
        let now = 100_000;
        insert_signal(
            &conn,
            now - 10,
            "opened",
            "open_long",
            r#"{"ai_scale":1.0,"filter_trace":{"gate":"none","decision":"opened"}}"#,
        );
        insert_signal(
            &conn,
            now - 20,
            "rejected",
            "open_long",
            r#"{"filter_trace":{"gate":"ai_fail_open"}}"#,
        );
        insert_signal(
            &conn,
            now - 30,
            "rejected",
            "reject",
            r#"{"ai_scale":0.5,"filter_trace":{"gate":"ai_dq"}}"#,
        );
        insert_signal(
            &conn,
            now - 40,
            "rejected",
            "open_long",
            r#"{"ai_scale":0.75,"filter_trace":{"gate":"funding_veto"}}"#,
        );
        insert_signal(
            &conn,
            now - 50,
            "flat",
            "open_long",
            r#"{"filter_trace":{"gate":"none","skip":true}}"#,
        );
        insert_signal(
            &conn,
            now - 90_000,
            "rejected",
            "reject",
            r#"{"filter_trace":{"gate":"news_veto"}}"#,
        );

        let value = half_ai_stats(&conn, "demo", now, 86_400);
        assert_eq!(value["signals_total"], 5, "窗口外的信号不计入");
        assert_eq!(value["opened"], 1);
        assert_eq!(value["rejected"], 3);
        assert_eq!(value["ai_reviewed"], 5, "AI 有动作的信号都算复核过");
        assert_eq!(value["ai_fail_open"], 1);
        assert_eq!(value["high_score_skip"], 1);
        assert_eq!(value["scale_0_5"], 1);
        assert_eq!(value["scale_0_75"], 1);
        assert_eq!(value["scale_0_9_1"], 1);
        assert_eq!(value["scale_none"], 2);
        assert_eq!(value["gates"]["funding_veto"], 1);
        assert_eq!(value["gates"]["ai_dq"], 1);
        assert_eq!(value["gates"]["ai_fail_open"], 1);
        assert_eq!(value["gates"]["none"], 2);
        assert_eq!(value["gates"]["news_veto"], 0, "窗口外的闸门不应计数");
        assert_eq!(
            value["ai_veto_rate_pct"], 20.0,
            "仅 AI 主动否决计入: 1 / ai_reviewed 5"
        );
        assert_eq!(value["ai_latency_samples"], 0, "无 AI 调用样本");
        assert_eq!(value["ai_latency_p99_ms"], 0.0);
        assert_eq!(value["ai_latency_max_ms"], 0.0);
        assert_eq!(value["ai_latency_by_provider"], json!({}));
    }

    #[test]
    fn half_ai_stats_counts_prompt_profiles() {
        let conn = table();
        let now = 300_000;
        insert_signal(
            &conn,
            now - 10,
            "opened",
            "open_long",
            r#"{"ai_prompt_profile":"full","filter_trace":{"gate":"none"}}"#,
        );
        insert_signal(
            &conn,
            now - 20,
            "opened",
            "open_long",
            r#"{"ai_prompt_profile":"slim","filter_trace":{"gate":"none"}}"#,
        );
        insert_signal(
            &conn,
            now - 30,
            "rejected",
            "reject",
            r#"{"filter_trace":{"gate":"ai_dq"}}"#,
        );

        let value = half_ai_stats(&conn, "demo", now, 86_400);
        assert_eq!(value["ai_prompt_full"], 1, "完整档位计数");
        assert_eq!(value["ai_prompt_slim"], 1, "瘦身档位计数");
    }

    #[test]
    fn half_ai_stats_filters_by_mode() {
        let conn = table();
        let now = 200_000;
        insert_signal_in(
            &conn,
            "demo",
            now - 10,
            "opened",
            "open_long",
            r#"{"ai_scale":1.0,"filter_trace":{"gate":"none"}}"#,
        );
        insert_signal_in(
            &conn,
            "demo",
            now - 20,
            "rejected",
            "reject",
            r#"{"filter_trace":{"gate":"ai_dq"}}"#,
        );
        insert_signal_in(
            &conn,
            "live",
            now - 15,
            "rejected",
            "reject",
            r#"{"filter_trace":{"gate":"news_veto"}}"#,
        );

        let demo = half_ai_stats(&conn, "demo", now, 86_400);
        assert_eq!(demo["signals_total"], 2, "只统计当前模式");
        assert_eq!(demo["opened"], 1);
        assert_eq!(demo["rejected"], 1);
        assert_eq!(demo["gates"]["ai_dq"], 1);
        assert_eq!(demo["gates"]["news_veto"], 0, "live 信号不能混进 demo");

        let live = half_ai_stats(&conn, "live", now, 86_400);
        assert_eq!(live["signals_total"], 1);
        assert_eq!(live["gates"]["news_veto"], 1);
        assert_eq!(live["gates"]["none"], 0);
    }

    #[test]
    fn ai_veto_rate_uses_ai_reviewed_denominator() {
        let conn = table();
        let now = 300_000;
        insert_signal_in(
            &conn,
            "demo",
            now - 10,
            "opened",
            "open_long",
            r#"{"filter_trace":{"gate":"none"}}"#,
        );
        insert_signal_in(
            &conn,
            "demo",
            now - 20,
            "rejected",
            "reject",
            r#"{"filter_trace":{"gate":"other"}}"#,
        );
        insert_signal_in(
            &conn,
            "demo",
            now - 30,
            "flat",
            "none",
            r#"{"filter_trace":{"gate":"none"}}"#,
        );
        insert_signal_in(
            &conn,
            "demo",
            now - 40,
            "flat",
            "",
            r#"{"filter_trace":{"gate":"none"}}"#,
        );

        let value = half_ai_stats(&conn, "demo", now, 86_400);
        assert_eq!(value["signals_total"], 4);
        assert_eq!(value["ai_reviewed"], 2, "none/空动作不算 AI 复核过");
        assert_eq!(
            value["ai_veto_rate_pct"], 50.0,
            "分母取 ai_reviewed，而不是全部信号"
        );
    }

    #[test]
    fn half_ai_stats_scopes_ai_latency_by_mode_when_column_exists() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE ai_call_logs(id INTEGER PRIMARY KEY, ts INTEGER, provider TEXT, \
               latency_ms INTEGER, mode TEXT); \
             INSERT INTO ai_call_logs VALUES(1, 100, 'p', 50, 'demo'); \
             INSERT INTO ai_call_logs VALUES(2, 100, 'p', 400, 'live');",
        )
        .unwrap();

        let demo = half_ai_stats(&conn, "demo", 200, 1_000);
        assert_eq!(demo["ai_latency_scope"], "mode");
        assert_eq!(demo["ai_latency_samples"], 1);
        assert_eq!(demo["ai_latency_p95_ms"], 50.0);

        let live = half_ai_stats(&conn, "live", 200, 1_000);
        assert_eq!(live["ai_latency_samples"], 1);
        assert_eq!(live["ai_latency_p95_ms"], 400.0);
    }

    #[test]
    fn half_ai_stats_handles_empty_window() {
        let conn = table();
        let value = half_ai_stats(&conn, "demo", 424_242, 86_400);
        assert_eq!(value["signals_total"], 0);
        assert_eq!(value["opened"], 0);
        assert_eq!(value["rejected"], 0);
        assert_eq!(value["ai_reviewed"], 0);
        assert_eq!(value["ai_veto_rate_pct"], 0.0);
        assert_eq!(value["scale_none"], 0);
        assert_eq!(value["ai_fail_open"], 0);
        assert_eq!(value["high_score_skip"], 0);
        assert_eq!(value["ai_latency_p50_ms"], 0.0);
        assert_eq!(value["ai_latency_p95_ms"], 0.0);
        assert_eq!(value["ai_latency_p99_ms"], 0.0);
        assert_eq!(value["ai_latency_max_ms"], 0.0);
        assert_eq!(value["ai_latency_samples"], 0);
        assert_eq!(value["ai_latency_by_provider"], json!({}));
        assert_eq!(value["gates"]["none"], 0);
        assert_eq!(value["gates"]["ai_dq"], 0);
    }

    #[test]
    fn half_ai_stats_includes_p99_and_max() {
        let conn = table();
        let now = 500_000;
        for latency in 1..=100 {
            insert_ai_call(&conn, now - 10, "alpha", latency);
        }
        insert_ai_call(&conn, now - 90_000, "alpha", 9_999);

        let value = half_ai_stats(&conn, "demo", now, 86_400);
        assert_eq!(value["ai_latency_samples"], 100, "只统计窗口内正延迟样本");
        assert_eq!(
            value["ai_latency_p50_ms"], 51.0,
            "nearest-rank: floor(100*0.5)"
        );
        assert_eq!(value["ai_latency_p95_ms"], 96.0);
        assert_eq!(value["ai_latency_p99_ms"], 100.0);
        assert_eq!(value["ai_latency_max_ms"], 100.0);
        assert_eq!(value["ai_latency_scope"], "all_modes");
        assert_eq!(value["ai_latency_by_provider"]["alpha"]["samples"], 100);
        assert_eq!(value["ai_latency_by_provider"]["alpha"]["p99_ms"], 100.0);
    }

    #[test]
    fn half_ai_stats_groups_latency_by_provider() {
        let conn = table();
        let now = 600_000;
        insert_ai_call(&conn, now - 10, "alpha", 100);
        insert_ai_call(&conn, now - 20, "alpha", 200);
        insert_ai_call(&conn, now - 30, "alpha", 300);
        insert_ai_call(&conn, now - 40, "beta", 50);
        insert_ai_call(&conn, now - 50, "beta", 60);
        insert_ai_call(&conn, now - 60, "", 10);
        insert_ai_call(&conn, now - 70, "beta", 0);

        let value = half_ai_stats(&conn, "demo", now, 86_400);
        assert_eq!(value["ai_latency_samples"], 6, "零延迟不计入样本");
        let by_provider = &value["ai_latency_by_provider"];
        assert_eq!(by_provider["alpha"]["samples"], 3);
        assert_eq!(by_provider["alpha"]["p50_ms"], 200.0);
        assert_eq!(by_provider["alpha"]["p95_ms"], 300.0);
        assert_eq!(by_provider["alpha"]["p99_ms"], 300.0);
        assert_eq!(by_provider["beta"]["samples"], 2);
        assert_eq!(by_provider["beta"]["p95_ms"], 60.0);
        assert_eq!(
            by_provider["unknown"]["samples"], 1,
            "空 provider 归入 unknown"
        );
        assert_eq!(by_provider["unknown"]["p50_ms"], 10.0);
    }

    #[test]
    fn half_ai_stats_reports_wall_and_queue_percentiles() {
        let conn = table();
        let now = 700_000;
        for (index, (wall, queue)) in [(100, 10), (200, 20), (300, 30), (400, 40), (500, 50)]
            .into_iter()
            .enumerate()
        {
            let features = json!({
                "ai_latency": {"wall_ms": wall, "provider_ms": 80, "queue_ms": queue},
                "filter_trace": {"gate": "none"},
            })
            .to_string();
            insert_signal_in(
                &conn,
                "demo",
                now - 10 - index as i64,
                "opened",
                "open_long",
                &features,
            );
        }

        let value = half_ai_stats(&conn, "demo", now, 86_400);
        assert_eq!(value["ai_wall_samples"], 5);
        assert_eq!(value["ai_wall_p50_ms"], 300.0, "nearest-rank: floor(5*0.5)");
        assert_eq!(value["ai_wall_p95_ms"], 500.0);
        assert_eq!(value["ai_wall_p99_ms"], 500.0);
        assert_eq!(value["ai_queue_samples"], 5);
        assert_eq!(value["ai_queue_p50_ms"], 30.0);
        assert_eq!(value["ai_queue_p95_ms"], 50.0);
        assert_eq!(value["ai_queue_p99_ms"], 50.0);
    }

    #[test]
    fn half_ai_stats_reports_zero_when_latency_missing() {
        let conn = table();
        insert_signal(
            &conn,
            800_000,
            "opened",
            "open_long",
            r#"{"filter_trace":{"gate":"none"}}"#,
        );
        let value = half_ai_stats(&conn, "demo", 800_000, 86_400);
        assert_eq!(value["ai_wall_samples"], 0);
        assert_eq!(value["ai_wall_p95_ms"], 0.0);
        assert_eq!(value["ai_queue_samples"], 0);
        assert_eq!(value["ai_queue_p95_ms"], 0.0);
    }

    #[test]
    fn percentile_handles_small_sample_without_panic() {
        assert_eq!(percentile(&[], 0.99), 0.0);
        assert_eq!(percentile(&[42.0], 0.99), 42.0);
        assert_eq!(percentile(&[42.0], 0.0), 42.0);
        assert_eq!(percentile(&[10.0, 20.0], 0.99), 20.0);
        assert_eq!(percentile(&[10.0, 20.0], 0.0), 10.0);
        assert_eq!(percentile(&[10.0, 20.0], 1.0), 20.0, "q=1 不越界");
    }

    #[test]
    fn identical_signals_merge_and_keep_the_scan_count() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        let row = AuditSignal::new("BTCUSDT")
            .with_decision("flat")
            .with_reject_reason("波动率过低");
        record(&tx, "demo", 1000, &[row.clone()]).unwrap();
        record(&tx, "demo", 1015, &[row.clone()]).unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&conn), 1, "相同信号只能留一行");
        assert_eq!(repeats(&conn), 2, "repeats 要记真实扫描次数");
    }

    #[test]
    fn reasons_that_differ_only_by_numbers_still_merge() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        record(
            &tx,
            "demo",
            1000,
            &[AuditSignal::new("BTCUSDT")
                .with_decision("rejected")
                .with_reject_reason("波动率过低 0.030% < 0.050%")],
        )
        .unwrap();
        record(
            &tx,
            "demo",
            1015,
            &[AuditSignal::new("BTCUSDT")
                .with_decision("rejected")
                .with_reject_reason("波动率过低 0.040% < 0.050%")],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&conn), 1, "同一类原因不该每轮都插一行");
        assert_eq!(repeats(&conn), 2);
    }

    #[test]
    fn different_reasons_are_kept_apart() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        record(
            &tx,
            "demo",
            1000,
            &[AuditSignal::new("BTCUSDT")
                .with_decision("rejected")
                .with_reject_reason("波动率过低")],
        )
        .unwrap();
        record(
            &tx,
            "demo",
            1001,
            &[AuditSignal::new("BTCUSDT")
                .with_decision("rejected")
                .with_reject_reason("量能不足")],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&conn), 2, "原因不同就是不同的拒绝理由");
    }

    #[test]
    fn rows_outside_the_merge_window_start_a_new_line() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        let row = AuditSignal::new("ETHUSDT").with_decision("flat");
        record(&tx, "demo", 1000, &[row.clone()]).unwrap();
        record(&tx, "demo", 1000 + MERGE_WINDOW_SEC + 1, &[row]).unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&conn), 2);
    }

    #[test]
    fn gate_stage_is_recorded_in_features() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        record(
            &tx,
            "demo",
            1000,
            &[AuditSignal::new("BTCUSDT")
                .with_stage("gate")
                .with_decision("rejected")
                .with_reject_reason("量能不足")],
        )
        .unwrap();
        tx.commit().unwrap();
        let features: String = conn
            .query_row("SELECT features FROM signals", [], |r| r.get(0))
            .unwrap();
        let value: Value = serde_json::from_str(&features).unwrap();
        assert_eq!(value["stage"], "gate");
    }

    #[test]
    fn audit_signal_persists_ai_meta_in_features() {
        let mut row = AuditSignal::new("BTCUSDT").with_ai("open_long", 0.8, "趋势向上");
        row.data_quality = Some(0.77);
        row.rr_target = Some(1.6);
        row.valid_bars = Some(6);
        row.invalidation_price = Some(97.5);
        row.entry_price = Some(99.8);
        let features = row.features_with_meta();
        assert_eq!(features["ai_dq"], 0.77);
        assert_eq!(features["ai_rr"], 1.6);
        assert_eq!(features["ai_vb"], 6);
        assert_eq!(features["ai_ip"], 97.5);
        assert_eq!(features["ai_ep"], 99.8);
    }

    #[test]
    fn audit_signal_persists_ai_latency_in_features() {
        let mut row = AuditSignal::new("BTCUSDT");
        row.provider_ms = Some(120);
        row.queue_ms = Some(30);
        row.wall_ms = Some(180);
        let features = row.features_with_meta();
        assert_eq!(features["ai_latency"]["wall_ms"], 180);
        assert_eq!(features["ai_latency"]["provider_ms"], 120);
        assert_eq!(features["ai_latency"]["queue_ms"], 30);
    }

    #[test]
    fn review_latency_is_persisted_in_features() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        let mut row = AuditSignal::new("ETHUSDT").with_decision("opened");
        row.provider_ms = Some(120);
        row.queue_ms = Some(30);
        row.wall_ms = Some(180);
        record(&tx, "demo", 1000, &[row]).unwrap();
        tx.commit().unwrap();
        let features: String = conn
            .query_row("SELECT features FROM signals", [], |r| r.get(0))
            .unwrap();
        let value: Value = serde_json::from_str(&features).unwrap();
        assert_eq!(value["ai_latency"]["wall_ms"], 180);
        assert_eq!(value["ai_latency"]["provider_ms"], 120);
        assert_eq!(value["ai_latency"]["queue_ms"], 30);
    }

    #[test]
    fn audit_signal_persists_ai_text_fields() {
        let mut row = AuditSignal::new("BTCUSDT").with_ai("open_long", 0.8, "趋势向上");
        row.invalidation_text = Some("跌破 97.5 失效".to_string());
        row.risk_text = Some("高位放量；资金费率抬升".to_string());
        let features = row.features_with_meta();
        assert_eq!(features["ai_inv"], "跌破 97.5 失效");
        assert_eq!(features["ai_risk"], "高位放量；资金费率抬升");
    }

    #[test]
    fn audit_signal_omits_empty_ai_text_fields() {
        let row = AuditSignal::new("BTCUSDT");
        let features = row.features_with_meta();
        assert!(
            features.get("ai_inv").is_none(),
            "无失效文本时不应写入 ai_inv: {features}"
        );
        assert!(
            features.get("ai_risk").is_none(),
            "无风险文本时不应写入 ai_risk: {features}"
        );
    }

    #[test]
    fn record_persists_ai_meta_in_signals() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        let mut row = AuditSignal::new("ETHUSDT").with_ai("open_long", 0.8, "趋势向上");
        row.data_quality = Some(0.77);
        row.rr_target = Some(1.6);
        row.valid_bars = Some(6);
        row.invalidation_price = Some(97.5);
        row.entry_price = Some(99.8);
        record(&tx, "demo", 1000, &[row]).unwrap();
        tx.commit().unwrap();
        let features: String = conn
            .query_row("SELECT features FROM signals", [], |r| r.get(0))
            .unwrap();
        let value: Value = serde_json::from_str(&features).unwrap();
        assert_eq!(value["ai_dq"], 0.77);
        assert_eq!(value["ai_rr"], 1.6);
        assert_eq!(value["ai_vb"], 6);
        assert_eq!(value["ai_ip"], 97.5);
        assert_eq!(value["ai_ep"], 99.8);
    }

    #[test]
    fn merged_signal_keeps_previous_ai_meta_when_new_row_missing() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        let mut first = AuditSignal::new("BTCUSDT")
            .with_decision("flat")
            .with_reject_reason("波动率过低");
        first.data_quality = Some(0.8);
        record(&tx, "demo", 1000, &[first]).unwrap();
        // 第二轮模型没给 dq，合并后不能把旧值清空。
        let second = AuditSignal::new("BTCUSDT")
            .with_decision("flat")
            .with_reject_reason("波动率过低");
        record(&tx, "demo", 1015, &[second]).unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&conn), 1, "同类信号应合并成一行");
        assert_eq!(repeats(&conn), 2, "合并要累加扫描次数");
        let features: String = conn
            .query_row("SELECT features FROM signals", [], |r| r.get(0))
            .unwrap();
        let value: Value = serde_json::from_str(&features).unwrap();
        assert_eq!(value["ai_dq"], 0.8, "新行缺 ai_dq 时要沿用旧值");
    }

    #[test]
    fn merged_signal_drops_stale_ai_text_when_new_round_missing() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        let mut first = AuditSignal::new("BTCUSDT")
            .with_decision("flat")
            .with_reject_reason("波动率过低");
        first.data_quality = Some(0.8);
        first.invalidation_text = Some("跌破 EMA".to_string());
        first.risk_text = Some("上方套牢".to_string());
        record(&tx, "demo", 1000, &[first]).unwrap();
        // 第二轮同键但没有文本：数值沿用旧值，文本必须以最新一轮为准消失。
        let second = AuditSignal::new("BTCUSDT")
            .with_decision("flat")
            .with_reject_reason("波动率过低");
        record(&tx, "demo", 1015, &[second]).unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&conn), 1, "同类信号应合并成一行");
        let features: String = conn
            .query_row("SELECT features FROM signals", [], |r| r.get(0))
            .unwrap();
        let value: Value = serde_json::from_str(&features).unwrap();
        assert_eq!(value["ai_dq"], 0.8, "数值字段新行缺失时仍沿用旧值");
        assert!(
            value.get("ai_inv").is_none(),
            "新行没给失效文本时不得残留旧值: {value}"
        );
        assert!(
            value.get("ai_risk").is_none(),
            "新行没给风险文本时不得残留旧值: {value}"
        );
    }

    #[test]
    fn filter_trace_classifies_funding_veto() {
        let row = AuditSignal::new("BTCUSDT")
            .with_stage("gate")
            .with_decision("rejected")
            .with_reject_reason("资金费率不利 0.12%");
        let features = row.features_with_meta();
        assert_eq!(features["filter_trace"]["stage"], "gate");
        assert_eq!(features["filter_trace"]["decision"], "rejected");
        assert_eq!(features["filter_trace"]["gate"], "funding_veto");
        assert_eq!(
            features["filter_trace"]["reject_reason"],
            "资金费率不利 0.12%"
        );
    }

    #[test]
    fn filter_trace_classifies_news_veto() {
        let row = AuditSignal::new("BTCUSDT")
            .with_stage("gate")
            .with_decision("rejected")
            .with_reject_reason("负面事件否决：交易所被调查");
        let features = row.features_with_meta();
        assert_eq!(features["filter_trace"]["gate"], "news_veto");
    }

    #[test]
    fn filter_trace_classifies_score_split_low() {
        let row = AuditSignal::new("BTCUSDT")
            .with_decision("rejected")
            .with_reject_reason("评分低于 AI 分流下限 0.200 < 0.450");
        let features = row.features_with_meta();
        assert_eq!(features["filter_trace"]["gate"], "score_split_low");
    }

    #[test]
    fn filter_trace_classifies_ai_fail_open() {
        let row = AuditSignal::new("BTCUSDT")
            .with_decision("opened")
            .with_reject_reason("AI 复核失败，按 fail-open 降仓: timeout");
        let features = row.features_with_meta();
        assert_eq!(features["filter_trace"]["gate"], "ai_fail_open");
    }

    #[test]
    fn filter_trace_persists_scale() {
        let mut row = AuditSignal::new("BTCUSDT").with_ai("open_long", 0.8, "趋势向上");
        row.scale = Some(0.5);
        let features = row.features_with_meta();
        assert_eq!(features["ai_scale"], 0.5);
        assert_eq!(features["filter_trace"]["scale"], 0.5);
        assert_eq!(features["filter_trace"]["ai_action"], "open_long");
    }

    #[test]
    fn filter_trace_marks_high_score_skip() {
        let mut row = AuditSignal::new("BTCUSDT").with_ai("open_long", 0.0, "ai_skip=high_score");
        row.scale = Some(1.0);
        let features = row.features_with_meta();
        assert_eq!(features["filter_trace"]["gate"], "none");
        assert_eq!(features["filter_trace"]["skip"], true);
        assert_eq!(features["filter_trace"]["ai_action"], "open_long");
    }

    #[test]
    fn classify_gate_covers_stable_prefixes() {
        assert_eq!(classify_gate("", "gate"), "none");
        assert_eq!(classify_gate("资金费率不利 0.1%", ""), "funding_veto");
        assert_eq!(classify_gate("负面事件否决：x", ""), "news_veto");
        assert_eq!(classify_gate("同方向持仓已达上限 2", ""), "direction_limit");
        assert_eq!(
            classify_gate("高相关同方向持仓过多 3", ""),
            "correlation_limit"
        );
        assert_eq!(
            classify_gate("评分低于 AI 分流下限 0.2", ""),
            "score_split_low"
        );
        assert_eq!(
            classify_gate("AI 复核失败，按 fail-open 降仓: x", ""),
            "ai_fail_open"
        );
        assert_eq!(classify_gate("AI 数据质量分 0.2 过低", ""), "ai_dq");
        assert_eq!(classify_gate("AI 盈亏比 1.1 不足", ""), "ai_rr");
        assert_eq!(classify_gate("AI 置信度 0.3 过低", ""), "ai_confidence");
        assert_eq!(classify_gate("方向不一致", ""), "ai_direction");
        assert_eq!(classify_gate("AI 方向为 short", ""), "ai_direction");
        assert_eq!(classify_gate("量能不足", ""), "other");
    }

    #[test]
    fn merged_signal_keeps_latest_filter_trace() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        record(
            &tx,
            "demo",
            1000,
            &[AuditSignal::new("BTCUSDT")
                .with_stage("gate")
                .with_decision("rejected")
                .with_reject_reason("量能不足")],
        )
        .unwrap();
        record(
            &tx,
            "demo",
            1015,
            &[AuditSignal::new("BTCUSDT")
                .with_decision("rejected")
                .with_reject_reason("量能不足")],
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&conn), 1, "同类信号应合并成一行");
        assert_eq!(repeats(&conn), 2);
        let features: String = conn
            .query_row("SELECT features FROM signals", [], |r| r.get(0))
            .unwrap();
        let value: Value = serde_json::from_str(&features).unwrap();
        assert_eq!(
            value["filter_trace"]["stage"], "",
            "合并后 filter_trace 必须是最新一轮"
        );
    }

    #[test]
    fn merged_signal_drops_stale_ai_scale() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        let mut first = AuditSignal::new("BTCUSDT")
            .with_decision("flat")
            .with_reject_reason("波动率过低");
        first.scale = Some(0.8);
        record(&tx, "demo", 1000, &[first]).unwrap();
        // 第二轮同键但没有 scale：ai_scale 必须按轮次消失，不能沿用旧值。
        let second = AuditSignal::new("BTCUSDT")
            .with_decision("flat")
            .with_reject_reason("波动率过低");
        record(&tx, "demo", 1015, &[second]).unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&conn), 1, "同类信号应合并成一行");
        let features: String = conn
            .query_row("SELECT features FROM signals", [], |r| r.get(0))
            .unwrap();
        let value: Value = serde_json::from_str(&features).unwrap();
        assert!(
            value.get("ai_scale").is_none(),
            "新行没给 scale 时不得残留旧 ai_scale: {value}"
        );
        assert!(
            value["filter_trace"]["scale"].is_null(),
            "filter_trace.scale 应反映本轮无 scale: {value}"
        );
    }

    #[test]
    fn modes_do_not_share_rows() {
        let mut conn = table();
        let tx = conn.transaction().unwrap();
        let row = AuditSignal::new("BTCUSDT").with_decision("flat");
        record(&tx, "demo", 1000, &[row.clone()]).unwrap();
        record(&tx, "live", 1001, &[row]).unwrap();
        tx.commit().unwrap();
        assert_eq!(count(&conn), 2);
    }
}
