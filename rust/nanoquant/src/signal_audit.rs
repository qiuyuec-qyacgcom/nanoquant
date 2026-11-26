//! 信号审计落库。
//!
//! 每轮扫描都要留下可追溯的一行：量化评分、AI 复核结论、没开仓的原因。
//! 引擎 15 秒一轮、标的池十几个，直接 insert 会把表写爆，
//! 所以相同信号合并成一行，用 repeats 记真实扫描次数——
//! 「扫描密度」看的是 repeats 累加，不是行数。

use rusqlite::Transaction;
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
        }
        features
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
/// ai_scale 只代表当前轮的下单缩放，跨轮沿用会与 filter_trace.scale 口径不一致。
const AI_ROUND_META_KEYS: [&str; 3] = ["ai_inv", "ai_risk", "ai_scale"];

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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn table() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE signals(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, direction TEXT, \
             score REAL, features TEXT, ai_action TEXT, ai_confidence REAL, ai_reason TEXT, \
             decision TEXT, reject_reason TEXT, mode TEXT, repeats INTEGER);",
        )
        .unwrap();
        conn
    }

    fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM signals", [], |r| r.get(0))
            .unwrap()
    }

    fn repeats(conn: &Connection) -> i64 {
        conn.query_row("SELECT repeats FROM signals LIMIT 1", [], |r| r.get(0))
            .unwrap()
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
