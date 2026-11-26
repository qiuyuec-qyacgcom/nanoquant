use std::sync::OnceLock;

use serde_json::{json, Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ParamMeta {
    pub group: String,
    pub key: String,
    pub kind: &'static str,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub visible: bool,
    pub ai_writable: bool,
}

/// 这些组的全部非敏感字段可写（含页面可见字段）。
pub(crate) const AI_FULL_WRITABLE_GROUPS: &[&str] = &["strategy"];

/// 组级黑名单：即使整组对 AI 不可见也不得写入，避免新组默认变成可写。
const AI_BLOCKED_GROUPS: &[&str] = &["ai_tuning", "risk", "symbol_scan"];

/// 运行时开关：即使不可见也不交给 AI，避免它把自己关掉或切环境。
/// v2 业务门槛同理：盈亏比门槛与信号有效期由人工配置，AI 不得自行放宽。
const AI_BLOCKED_KEYS: &[(&str, &str)] = &[
    ("engine", "mode"),
    ("engine", "enabled"),
    ("engine", "close_on_halt"),
    ("signal_follow", "enabled"),
    ("backup", "encrypt"),
    ("strategy", "min_entry_rr"),
    ("strategy", "valid_bars"),
    // 均值回归开关是人工断路器，AI 调参不得自行关闭。
    ("strategy", "mr_enabled"),
];

pub(crate) const RANGES: &[(&str, &str, f64, f64, f64)] = &[
    ("engine", "cycle_interval_sec", 15.0, 3600.0, 1.0),
    ("engine", "max_positions", 1.0, 50.0, 1.0),
    ("engine", "max_symbols_per_cycle", 1.0, 100.0, 1.0),
    ("engine", "scan_concurrency", 1.0, 20.0, 1.0),
    ("engine", "timezone_offset", -12.0, 14.0, 1.0),
    ("engine", "recv_window_ms", 1000.0, 60000.0, 1000.0),

    ("risk", "max_leverage", 1.0, 100.0, 1.0),
    ("risk", "min_leverage", 1.0, 100.0, 1.0),
    ("risk", "local_leverage_base", 1.0, 25.0, 1.0),
    ("risk", "local_leverage_vol_target", 0.5, 10.0, 0.1),
    ("risk", "local_leverage_score_weight", 0.0, 20.0, 1.0),
    ("risk", "margin_pct", 1.0, 100.0, 1.0),
    ("risk", "max_exposure_pct", 1.0, 10000.0, 10.0),
    ("risk", "max_same_direction", 0.0, 100.0, 1.0),
    ("risk", "correlation_limit", 1.0, 5.0, 1.0),
    ("risk", "direction_exposure_limit_pct", 100.0, 1000.0, 10.0),
    ("risk", "category_exposure_limit_pct", 0.0, 10000.0, 10.0),
    ("risk", "vol_target_atr_pct", 0.0, 10.0, 0.1),
    ("risk", "daily_loss_limit_pct", 0.01, 100.0, 1.0),
    ("risk", "max_drawdown_pct", 0.0, 100.0, 1.0),
    ("risk", "consecutive_loss_limit", 1.0, 100.0, 1.0),
    ("risk", "cooldown_minutes", 0.0, 1440.0, 1.0),
    ("risk", "symbol_cooldown_minutes", 0.0, 1440.0, 1.0),
    ("risk", "min_notional_buffer", 1.0, 10.0, 0.01),
    ("risk", "atr_stop_mult", 1.0, 4.0, 0.1),
    ("risk", "min_equity_usdt", 0.0, 100000.0, 0.1),
    ("risk", "fee_rate_taker", 0.0, 0.01, 0.0001),
    ("risk", "fee_rate_maker", 0.0, 0.01, 0.0001),
    ("risk", "global_stop_loss_pct", 0.0, 100.0, 0.1),
    ("risk", "global_take_profit_pct", 0.0, 100.0, 0.1),
    ("strategy", "ema_fast", 1.0, 200.0, 1.0),
    ("strategy", "ema_slow", 2.0, 400.0, 1.0),
    ("strategy", "rsi_period", 2.0, 100.0, 1.0),
    ("strategy", "atr_period", 2.0, 100.0, 1.0),
    ("strategy", "entry_threshold", 0.01, 1.0, 0.01),
    ("strategy", "exit_threshold", 0.0, 1.0, 0.01),
    ("strategy", "volume_ratio_min", 0.0, 10.0, 0.1),
    ("strategy", "atr_pct_min", 0.0, 10.0, 0.01),
    ("strategy", "atr_pct_max", 0.1, 20.0, 0.1),
    ("strategy", "min_24h_quote_volume", 0.0, 1.0e12, 100000.0),

    ("strategy", "roi_min_r", 0.0, 20.0, 0.1),
    ("strategy", "trailing_pct", 0.0, 200.0, 0.1),
    ("strategy", "trailing_arm_r", 0.0, 20.0, 0.1),
    ("strategy", "breakeven_at_r", 0.0, 20.0, 0.1),
    ("strategy", "max_data_gaps", 0.0, 10.0, 1.0),
    ("strategy", "data_stale_mult", 1.0, 10.0, 0.1),
    ("strategy", "partial_tp_at_r", 0.0, 20.0, 0.1),
    ("strategy", "partial_tp_ratio", 0.0, 1.0, 0.01),
    ("strategy", "max_spread_bps", 0.0, 1000.0, 0.5),
    ("strategy", "funding_veto_bps", 0.0, 50.0, 0.5),
    ("strategy", "mr_rsi_high", 50.0, 90.0, 1.0),
    ("strategy", "mr_rsi_low", 10.0, 50.0, 1.0),
    ("strategy", "mr_band_k", 1.0, 4.0, 0.1),
    ("strategy", "mr_sl_pct", 5.0, 50.0, 1.0),
    ("strategy", "mr_tp_pct", 5.0, 100.0, 1.0),
    // v2 开仓盈亏比门槛：默认 0.8，低于此值的信号不开仓；1 步 = 0.1 R。
    ("strategy", "min_entry_rr", 0.0, 10.0, 0.1),
    // v2 信号有效期：AI 未给出有效期时使用的 K 线根数，默认 6；整数语义。
    ("strategy", "valid_bars", 1.0, 24.0, 1.0),
    ("ai_tuning", "max_auto_apply_per_24h", 1.0, 100.0, 1.0),
    (
        "ai_tuning",
        "no_candidate_rollback_sec",
        60.0,
        86400.0,
        60.0,
    ),
    ("ai_tuning", "regime_confirm_sec", 60.0, 86400.0, 60.0),
    ("ai_tuning", "regime_cooldown_sec", 0.0, 86400.0, 60.0),
    ("ai_tuning", "min_oos_trades", 1.0, 1000.0, 1.0),
    ("symbol_scan", "interval_hours", 1.0, 24.0, 1.0),
    ("symbol_scan", "market_limit", 50.0, 500.0, 10.0),
    ("ai", "min_confidence", 0.0, 1.0, 0.01),
    ("ai", "param_tuning_min_interval_sec", 60.0, 604800.0, 60.0),
    ("ai", "max_calls_per_hour", 1.0, 10000.0, 1.0),
    ("ai", "monthly_budget_usd", 0.0, 1.0e6, 0.1),
    ("ai", "timeout_sec", 1.0, 300.0, 1.0),
    ("ai", "temperature", 0.0, 2.0, 0.1),
    ("ai", "decision_schema_version", 1.0, 10.0, 1.0),
    ("ai", "fail_open_scale", 0.0, 1.0, 0.05),
    ("ai", "high_score_threshold", 0.5, 1.0, 0.01),
    ("ai", "low_score_threshold", 0.0, 0.7, 0.01),
    ("ai", "review_cache_ttl_sec", 0.0, 600.0, 1.0),
    ("ai", "review_concurrency", 1.0, 16.0, 1.0),
    ("ai", "review_timeout_sec", 5.0, 120.0, 1.0),
    ("data", "kline_limit", 10.0, 1000.0, 10.0),
    ("data", "ticker_ttl_sec", 1.0, 3600.0, 1.0),
    ("data", "kline_ttl_sec", 1.0, 3600.0, 1.0),
    ("data", "request_timeout_sec", 1.0, 300.0, 1.0),
    ("data", "max_pool_size", 1.0, 500.0, 1.0),
    ("data", "book_stale_ms", 500.0, 30000.0, 500.0),
    ("news", "refresh_minutes", 1.0, 1440.0, 1.0),
    ("news", "keep_days", 1.0, 365.0, 1.0),
    ("news", "sentiment_threshold", -1.0, 1.0, 0.01),
    ("news", "max_items_per_source", 1.0, 200.0, 1.0),
    ("news", "request_timeout_sec", 1.0, 300.0, 1.0),
    ("news", "veto_window_min", 5.0, 240.0, 1.0),
    ("news", "veto_max_items", 1.0, 10.0, 1.0),
    ("evolution", "max_injected", 0.0, 100.0, 1.0),
    ("evolution", "ttl_days", 1.0, 365.0, 1.0),
    ("evolution", "min_confidence", 0.0, 1.0, 0.01),
    ("evolution", "global_review_every_trades", 1.0, 1000.0, 1.0),
    ("evolution", "max_rule_chars", 10.0, 10000.0, 10.0),
    ("evolution", "min_lesson_sample", 1.0, 1000.0, 1.0),
    ("evolution", "batch_review_size", 1.0, 100.0, 1.0),
    ("evolution", "min_health_sample", 1.0, 1000.0, 1.0),
    ("evolution", "archive_health_threshold", 0.0, 1.0, 0.01),
    ("evolution", "max_active_mantras", 1.0, 1000.0, 1.0),
    ("backup", "retention", 1.0, 200.0, 1.0),
    ("system", "event_retention_days", 1.0, 3650.0, 1.0),
    ("system", "signal_retention_days", 1.0, 3650.0, 1.0),
    ("system", "equity_retention_days", 1.0, 3650.0, 1.0),
    ("system", "alert_cooldown_sec", 0.0, 86400.0, 1.0),
    ("system", "alert_digest_minutes", 1.0, 1440.0, 1.0),
    ("system", "api_rate_limit_per_min", 0.0, 100000.0, 1.0),
    ("signal_follow", "poll_sec", 1.0, 3600.0, 1.0),
    ("signal_follow", "leverage_bonus", 0.0, 100.0, 1.0),
    ("signal_follow", "max_age_sec", 1.0, 86400.0, 1.0),
    ("signal_follow", "max_per_hour", 1.0, 1000.0, 1.0),
    ("signal_follow", "default_sl_pct", 0.0, 100.0, 0.1),
    ("signal_follow", "margin_pct_override", 0.0, 100.0, 0.1),
    ("journal", "min_record_confidence", 0.0, 1.0, 0.01),
    ("journal", "label_after_sec", 1.0, 2592000.0, 3600.0),
    ("journal", "correct_threshold_pct", 0.0, 100.0, 0.01),
    ("journal", "retrieve_top_k", 1.0, 100.0, 1.0),
    ("journal", "max_experiences", 1.0, 10000.0, 1.0),
    ("journal", "min_similarity", 0.0, 1.0, 0.01),
];

const VISIBLE: &[(&str, &[&str])] = &[
    (
        "engine",
        &[
            "cycle_interval_sec",
            "max_positions",
            "trading_windows",
            "timezone_offset",
            "order_type",
            "close_on_halt",
        ],
    ),
    (
        "strategy",
        &[
            "timeframe",
            "valid_bars",
            "entry_threshold",
            "min_entry_rr",
            "exit_threshold",
            "volume_ratio_min",
            "atr_pct_min",
            "atr_pct_max",
            "min_24h_quote_volume",

            "trailing_pct",
            "breakeven_at_r",
            "partial_tp_at_r",
            "partial_tp_ratio",
            "max_spread_bps",
            "funding_veto_bps",
            "mr_enabled",
            "mr_rsi_high",
            "mr_rsi_low",
            "mr_band_k",
            "mr_sl_pct",
            "mr_tp_pct",
            "roi_table",
        ],
    ),
    (
        "ai_tuning",
        &[
            "enabled",
            "auto_apply",
            "max_auto_apply_per_24h",
            "no_candidate_rollback_sec",
            "regime_confirm_sec",
            "regime_cooldown_sec",
            "min_oos_trades",
            "auto_apply_timeframes",
        ],
    ),
    (
        "symbol_scan",
        &["enabled", "interval_hours", "market_limit"],
    ),
    (
        "ai",
        &[
            "use_invalidation_price",
            "use_entry_price",
            "veto_only",
            "scale_on_confidence",
            "fail_open_scale",
            "high_score_threshold",
            "low_score_threshold",
            "review_cache_ttl_sec",
            "review_concurrency",
            "review_timeout_sec",
        ],
    ),
    (
        "risk",
        &[

            "margin_pct",
            "min_leverage",
            "max_leverage",
            "local_leverage_base",
            "local_leverage_vol_target",
            "local_leverage_score_weight",
            "max_exposure_pct",
            "max_same_direction",
            "correlation_limit",
            "direction_exposure_limit_pct",
            "atr_stop_mult",
            "global_stop_loss_pct",
            "global_take_profit_pct",
            "daily_loss_limit_pct",
            "max_drawdown_pct",
            "consecutive_loss_limit",
            "cooldown_minutes",
            "symbol_cooldown_minutes",
            "min_equity_usdt",
            "category_exposure_limit_pct",
            "vol_target_atr_pct",
            "auto_protect_external",
            "min_notional_buffer",
            "fee_rate_taker",
            "fee_rate_maker",
        ],
    ),
    (
        "data",
        &[
            "demo_base_url",
            "live_base_url",
            "backup_base_urls",
            "proxy",
            "book_ws_enabled",
            "book_stale_ms",
        ],
    ),
    (
        "news",
        &[
            "enabled",
            "refresh_minutes",
            "keep_days",
            "sources",
            "ai_summary",
            "sentiment_threshold",
            "veto_enabled",
            "veto_window_min",
            "veto_max_items",
        ],
    ),
    (
        "system",
        &[
            "log_level",
            "event_retention_days",
            "signal_retention_days",
            "equity_retention_days",
            "alert_enabled",
            "alert_min_level",
            "alert_webhook_url",
            "alert_error_webhook_url",
            "alert_telegram_token",
            "alert_telegram_chat_id",
            "alert_cooldown_sec",
            "api_rate_limit_per_min",
            "metrics_enabled",
        ],
    ),
    (
        "backup",
        &["retention", "encrypt", "auto_daily", "include_logs"],
    ),
];

static DEFAULTS: OnceLock<Value> = OnceLock::new();

fn defaults() -> &'static Value {
    DEFAULTS.get_or_init(|| {
        serde_json::from_str(include_str!("default_settings.json")).expect("default_settings.json")
    })
}

pub(crate) fn is_sensitive(group: &str, key: &str) -> bool {
    if group == "security" {
        return true;
    }
    let lower = key.to_ascii_lowercase();
    [
        "secret",
        "password",
        "token",
        "api_key",
        "_enc",
        "webhook",
        "credential",
        "authorization",
        "chat_id",
        "private_key",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_visible(group: &str, key: &str) -> bool {
    VISIBLE
        .iter()
        .find(|(name, _)| *name == group)
        .map(|(_, keys)| keys.contains(&key))
        .unwrap_or(false)
}

fn is_ai_writable(group: &str, key: &str) -> bool {
    if group == "ai"
        || AI_BLOCKED_GROUPS.contains(&group)
        || AI_BLOCKED_KEYS.contains(&(group, key))
    {
        return false;
    }
    if is_sensitive(group, key) {
        return false;
    }
    if AI_FULL_WRITABLE_GROUPS.contains(&group) {
        return true;
    }
    !is_visible(group, key)
}

/// AI 调参专用边界：prompt 展示与候选校验共用同一区间。
pub(crate) fn ai_bounds(group: &str, key: &str) -> Option<(f64, f64)> {
    match (group, key) {
        ("strategy", "atr_pct_min") => Some((0.0, 1.0)),
        ("strategy", "atr_pct_max") => Some((6.0, 20.0)),
        ("strategy", "volume_ratio_min") => Some((0.0, 1.0)),
        ("strategy", "entry_threshold") => Some((0.01, 0.5)),
        _ => None,
    }
}

pub(crate) fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Null => "null",
    }
}

fn range_of(group: &str, key: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
    RANGES
        .iter()
        .find(|(name, field, ..)| *name == group && *field == key)
        .map(|(_, _, min, max, step)| (Some(*min), Some(*max), Some(*step)))
        .unwrap_or((None, None, None))
}

pub(crate) fn catalog() -> Value {
    let mut groups = Map::new();
    for (group, object) in defaults().as_object().unwrap() {
        if group == "security" {
            continue;
        }
        let Some(object) = object.as_object() else {
            continue;
        };
        let mut hidden = Vec::new();
        let mut writable = Vec::new();
        let mut meta = Map::new();
        for (key, default) in object {
            if is_sensitive(group, key) {
                continue;
            }
            let visible = is_visible(group, key);
            let ai_writable = is_ai_writable(group, key);
            let (min, max, step) = range_of(group, key);
            if !visible {
                hidden.push(json!(key));
            }
            if ai_writable {
                writable.push(json!(key));
            }
            meta.insert(
                key.clone(),
                json!({
                    "kind": kind_of(default),
                    "min": min,
                    "max": max,
                    "step": step,
                    "visible": visible,
                    "ai_writable": ai_writable,
                }),
            );
        }
        groups.insert(
            group.clone(),
            json!({ "hidden": hidden, "writable": writable, "meta": meta }),
        );
    }
    json!({ "groups": groups })
}

pub(crate) fn find(group: &str, key: &str) -> Option<ParamMeta> {
    if is_sensitive(group, key) {
        return None;
    }
    let value = defaults().get(group)?.get(key)?;
    let (min, max, step) = range_of(group, key);
    Some(ParamMeta {
        group: group.to_string(),
        key: key.to_string(),
        kind: kind_of(value),
        min,
        max,
        step,
        visible: is_visible(group, key),
        ai_writable: is_ai_writable(group, key),
    })
}

pub(crate) fn coerce(meta: &ParamMeta, value: &Value) -> Result<Value, String> {
    match meta.kind {
        "number" => {
            let number = value
                .as_f64()
                .ok_or_else(|| format!("{}.{} 必须是数字", meta.group, meta.key))?;
            if !number.is_finite() {
                return Err(format!("{}.{} 必须是有限数字", meta.group, meta.key));
            }
            Ok(json!(number.clamp(
                meta.min.unwrap_or(f64::MIN),
                meta.max.unwrap_or(f64::MAX)
            )))
        }
        "bool" => value
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| format!("{}.{} 必须是布尔值", meta.group, meta.key)),
        "string" => value
            .as_str()
            .map(|text| json!(text))
            .ok_or_else(|| format!("{}.{} 必须是字符串", meta.group, meta.key)),
        "array" => value
            .as_array()
            .map(|_| value.clone())
            .ok_or_else(|| format!("{}.{} 必须是数组", meta.group, meta.key)),
        "object" => value
            .as_object()
            .map(|_| value.clone())
            .ok_or_else(|| format!("{}.{} 必须是对象", meta.group, meta.key)),
        _ => Err(format!("{}.{} 类型不支持", meta.group, meta.key)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catalog_hides_sensitive_fields_and_covers_strategy() {
        let catalog = catalog();
        let strategy = &catalog["groups"]["strategy"];
        assert!(
            strategy["hidden"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "ema_fast"),
            "{catalog}"
        );
        assert!(catalog["groups"].get("security").is_none());
        assert!(!serde_json::to_string(&catalog).unwrap().contains("api_key"));
    }

    #[test]
    fn coerce_clamps_numbers_and_rejects_wrong_types() {
        let meta = find("strategy", "entry_threshold").expect("entry_threshold 必须注册");
        assert_eq!(coerce(&meta, &json!(9.0)).unwrap(), json!(1.0));
        assert!(coerce(&meta, &json!("x")).is_err());
        assert!(coerce(&meta, &json!(f64::NAN)).is_err());
    }

    #[test]
    fn registry_ranges_match_settings_validation() {
        assert_eq!(
            find("engine", "cycle_interval_sec").unwrap().min,
            Some(15.0)
        );
        assert_eq!(find("risk", "max_leverage").unwrap().max, Some(100.0));
        assert_eq!(find("risk", "atr_stop_mult").unwrap().min, Some(1.0));
        assert_eq!(find("risk", "atr_stop_mult").unwrap().max, Some(4.0));
        assert_eq!(
            find("risk", "daily_loss_limit_pct").unwrap().min,
            Some(0.01)
        );
        assert_eq!(find("backup", "retention").unwrap().max, Some(200.0));
    }

    #[test]
    fn sensitive_and_runtime_switches_are_not_ai_writable() {
        assert!(is_sensitive("system", "alert_webhook_url"));
        assert!(is_sensitive("system", "alert_telegram_chat_id"));
        assert!(find("system", "alert_webhook_url").is_none());
        assert!(!find("engine", "enabled").unwrap().ai_writable);
        assert!(!find("signal_follow", "enabled").unwrap().ai_writable);
        assert!(!find("backup", "encrypt").unwrap().ai_writable);
        assert!(!find("risk", "max_leverage").unwrap().ai_writable);
    }

    #[test]
    fn every_non_sensitive_default_field_is_registered() {
        let defaults: Value = serde_json::from_str(include_str!("default_settings.json")).unwrap();
        for (group, object) in defaults.as_object().unwrap() {
            if group == "security" {
                continue;
            }
            for key in object.as_object().unwrap().keys() {
                if is_sensitive(group, key) {
                    continue;
                }
                assert!(find(group, key).is_some(), "{group}.{key} 未注册");
            }
        }
    }

    #[test]
    fn ai_cannot_write_its_own_config_or_engine_mode() {
        assert!(!is_ai_writable("ai", "enabled"));
        assert!(!is_ai_writable("engine", "mode"));
        assert!(is_ai_writable("strategy", "entry_threshold"));
    }

    #[test]
    fn ai_writable_covers_hidden_and_core_groups_only() {
        assert!(
            find("strategy", "ema_fast").unwrap().ai_writable,
            "隐藏参数可写"
        );
        assert!(
            find("strategy", "entry_threshold").unwrap().ai_writable,
            "入场核心可写"
        );
        assert!(
            !find("risk", "max_leverage").unwrap().ai_writable,
            "风控核心不可写"
        );
        assert!(
            !find("engine", "cycle_interval_sec").unwrap().ai_writable,
            "可见运行参数不可写"
        );
        assert!(
            !find("system", "log_level").unwrap().ai_writable,
            "可见日志参数不可写"
        );
        assert!(
            !find("ai", "allow_param_tuning").unwrap().ai_writable,
            "ai 组始终不可写"
        );
    }

    #[test]
    fn ai_tuning_group_is_visible_but_ai_blocked() {
        let value = crate::param_registry::catalog();
        let group = &value["groups"]["ai_tuning"];
        assert_eq!(group["writable"], serde_json::json!([]));
        for key in [
            "enabled",
            "auto_apply",
            "max_auto_apply_per_24h",
            "no_candidate_rollback_sec",
            "regime_confirm_sec",
            "regime_cooldown_sec",
            "min_oos_trades",
            "auto_apply_timeframes",
        ] {
            assert_eq!(
                group["meta"][key]["visible"],
                serde_json::json!(true),
                "{key} 应可见"
            );
            assert_eq!(
                group["meta"][key]["ai_writable"],
                serde_json::json!(false),
                "{key} 不可被 AI 写"
            );
        }
    }

    #[test]
    fn risk_is_not_ai_writable_after_a_phase() {
        assert!(
            !crate::param_registry::find("risk", "max_leverage")
                .unwrap()
                .ai_writable
        );
        assert!(
            !crate::param_registry::find("risk", "margin_pct")
                .unwrap()
                .ai_writable
        );
        assert!(!is_ai_writable("risk", "future_hidden_field"));
        assert!(
            crate::param_registry::find("strategy", "entry_threshold")
                .unwrap()
                .ai_writable
        );
    }

    #[test]
    fn symbol_scan_group_is_visible_and_ai_blocked() {
        let value = crate::param_registry::catalog();
        let group = &value["groups"]["symbol_scan"];
        assert_eq!(
            group["meta"]["enabled"]["visible"],
            serde_json::json!(true),
            "enabled 应可见"
        );
        for key in ["interval_hours", "market_limit"] {
            assert_eq!(
                group["meta"][key]["visible"],
                serde_json::json!(true),
                "{key} 应可见"
            );
        }
        assert_eq!(group["writable"], serde_json::json!([]));
        assert_eq!(
            find("symbol_scan", "interval_hours").unwrap().min,
            Some(1.0)
        );
        assert_eq!(
            find("symbol_scan", "interval_hours").unwrap().max,
            Some(24.0)
        );
        assert_eq!(
            find("symbol_scan", "market_limit").unwrap().min,
            Some(50.0)
        );
        assert_eq!(
            find("symbol_scan", "market_limit").unwrap().max,
            Some(500.0)
        );
        assert_eq!(
            find("symbol_scan", "interval_hours").unwrap().step,
            Some(1.0)
        );
        assert_eq!(
            find("symbol_scan", "market_limit").unwrap().step,
            Some(10.0)
        );
        let defaults = defaults();
        assert_eq!(defaults["symbol_scan"]["enabled"], json!(true));
        assert_eq!(defaults["symbol_scan"]["interval_hours"], json!(3));
        assert_eq!(defaults["symbol_scan"]["market_limit"], json!(300));
        assert!(!is_ai_writable("symbol_scan", "enabled"));
        assert!(!is_ai_writable("symbol_scan", "future_hidden_field"));
    }

    #[test]
    fn v2_threshold_params_are_registered() {
        let d = defaults();
        assert_eq!(d["strategy"]["min_entry_rr"], json!(0.8));
        assert_eq!(d["strategy"]["valid_bars"], json!(6));
        assert_eq!(d["ai"]["use_invalidation_price"], json!(false));
        assert_eq!(d["ai"]["use_entry_price"], json!(false));
        assert!(is_visible("strategy", "valid_bars"));
        assert!(is_visible("strategy", "min_entry_rr"));
        assert!(is_visible("ai", "use_invalidation_price"));
        assert!(is_visible("ai", "use_entry_price"));
        assert!(!find("strategy", "min_entry_rr").unwrap().ai_writable);
        assert!(!find("strategy", "valid_bars").unwrap().ai_writable);
    }

    #[test]
    fn ai_bounds_are_stricter_than_manual_ranges() {
        for (group, key) in [
            ("strategy", "atr_pct_min"),
            ("strategy", "atr_pct_max"),
            ("strategy", "volume_ratio_min"),
            ("strategy", "entry_threshold"),
        ] {
            let (low, high) = ai_bounds(group, key).unwrap();
            let (min, max, _) = range_of(group, key);
            assert!(low >= min.unwrap(), "{group}.{key} 下界应落在 RANGES 内");
            assert!(high <= max.unwrap(), "{group}.{key} 上界应落在 RANGES 内");
        }
        assert_eq!(
            crate::param_registry::ai_bounds("strategy", "atr_pct_min"),
            Some((0.0, 1.0))
        );
        assert_eq!(
            crate::param_registry::ai_bounds("strategy", "atr_pct_max"),
            Some((6.0, 20.0))
        );
        assert_eq!(
            crate::param_registry::ai_bounds("strategy", "volume_ratio_min"),
            Some((0.0, 1.0))
        );
        assert_eq!(
            crate::param_registry::ai_bounds("strategy", "entry_threshold"),
            Some((0.01, 0.5))
        );
        assert_eq!(ai_bounds("risk", "max_leverage"), None);
        assert_eq!(ai_bounds("strategy", "ema_fast"), None);
        assert_eq!(ai_bounds("ai_tuning", "min_oos_trades"), None);
    }
}
