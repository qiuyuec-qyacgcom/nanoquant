//! 持仓快速风控：有仓位时每 0.5 秒用公开标记价检查全局/单仓止盈止损。
//!
//! 与 480 秒策略周期解耦：命中即走平仓链路，同时把标记价与浮盈写回持仓，
//! 供权益面板与行情同步。

use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::time::Duration;

use crate::runtime::EngineControl;

/// 把最新标记价和浮盈写回持仓。快速风控每 0.5 秒调用一次，
/// 即使未命中风控分支也刷新，权益面板因此与行情同步。
pub(crate) fn apply_position_mark(
    conn: &rusqlite::Connection,
    id: i64,
    mark: f64,
    entry: f64,
    qty: f64,
    direction: f64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE positions SET mark_price=?1, venue_unrealized=?2, venue_ts=?3 \
         WHERE id=?4 AND status='open'",
        (mark, (mark - entry) * qty * direction, crate::now(), id),
    )
}

/// 持仓风控专用：有仓位时每 0.5 秒用公开标记价检查全局/单仓止盈止损，
/// 命中后立即走平仓链路，不再等待 480 秒策略周期。
pub(crate) async fn run_fast_risk_task(
    db: crate::db::Db,
    master_key: Vec<u8>,
    mut stop: watch::Receiver<bool>,
) {
    let control = EngineControl::new(db.clone(), master_key);
    let mut closing = std::collections::HashSet::new();
    loop {
        if *stop.borrow() {
            break;
        }
        if let Err(error) = fast_risk_cycle(&db, &control, &mut closing).await {
            tracing::debug!(%error, "快速风控检查失败");
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            _ = stop.changed() => break,
        }
    }
}

/// 开关只在消费点生效：关闭时一律回退旧行为，不采用 AI 值。
pub(crate) fn gate_ai_value<T>(value: Option<T>, enabled: bool) -> Option<T> {
    if enabled {
        value
    } else {
        None
    }
}

/// 快速风控判定输入：9 个量具名化，避免调用点位置参数错位。
pub(crate) struct FastRiskInput<'a> {
    side: &'a str,
    entry: f64,
    mark: f64,
    sl: f64,
    tp: f64,
    leverage: f64,
    invalidation_price: Option<f64>,
    global_stop_pct: f64,
    global_tp_pct: f64,
}

/// 快速风控裁决：AI 失效价 → ATR 保护价 → ROI 兜底 → 单仓止盈价。
/// 返回 None 表示继续持仓。
fn fast_risk_reason(input: &FastRiskInput) -> Option<&'static str> {
    // 价格非法时直接放行，避免除零或 NaN 比较导致误平仓。
    if !input.entry.is_finite()
        || input.entry <= 0.0
        || !input.mark.is_finite()
        || input.mark <= 0.0
    {
        return None;
    }
    // 非 "long" 一律按空单处理（沿用旧行为）。
    let direction = if input.side == "long" { 1.0 } else { -1.0 };

    // ① AI 失效价：AI 的否决意见，保持最高优先级。
    if let Some(ip) = input
        .invalidation_price
        .filter(|v| *v > 0.0 && v.is_finite())
    {
        if (direction > 0.0 && input.mark <= ip) || (direction < 0.0 && input.mark >= ip) {
            return Some("invalidation");
        }
    }

    // ② ATR 保护价：开仓时按 ATR% 写入，不随杠杆收缩，必须先于 ROI 兜底。
    if input.sl > 0.0
        && ((direction > 0.0 && input.mark <= input.sl)
            || (direction < 0.0 && input.mark >= input.sl))
    {
        return Some("stop_loss");
    }

    // ③ 仓位止盈止损率：ROI 口径的账户风险预算，整仓强制平仓。
    let margin_pct =
        (input.mark - input.entry) * direction / input.entry * 100.0 * input.leverage.max(1.0);
    if input.global_stop_pct > 0.0 && margin_pct <= -input.global_stop_pct {
        return Some("global_stop_loss");
    }
    if input.global_tp_pct > 0.0 && margin_pct >= input.global_tp_pct {
        return Some("global_take_profit");
    }

    // ④ 单仓止盈价：趋势仓开仓时为 0，仅均值回归仓仍有值。
    if input.tp > 0.0
        && ((direction > 0.0 && input.mark >= input.tp)
            || (direction < 0.0 && input.mark <= input.tp))
    {
        return Some("take_profit");
    }
    None
}

async fn fast_risk_cycle(
    db: &crate::db::Db,
    control: &EngineControl,
    closing: &mut std::collections::HashSet<i64>,
) -> Result<(), String> {
    let (rows, global_stop, global_tp, ai_cfg) = {
        let conn = db.conn.lock().map_err(|_| "数据库锁不可用".to_string())?;
        let ai_cfg = crate::ai_client::read_ai_config_conn(&conn);
        let mode: String = conn
            .query_row(
                "SELECT value FROM engine_state WHERE key='mode'",
                [],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| "demo".to_string());
        if mode != "live" {
            return Ok(());
        }
        let risk = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='risk'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or_else(|| json!({}));
        let global_stop = risk["global_stop_loss_pct"].as_f64().unwrap_or(10.0);
        let global_tp = risk["global_take_profit_pct"].as_f64().unwrap_or(15.0);
        let mut stmt = conn
            .prepare(
"SELECT id,symbol,side,entry_price,sl_price,tp_price,leverage,qty,invalidation_price,features \
                 FROM positions WHERE mode='live' AND status='open' AND qty > 0",
            )
            .map_err(|error| error.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, Option<f64>>(4)?.unwrap_or(0.0),
                    row.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                    row.get::<_, Option<f64>>(6)?.unwrap_or(1.0),
                    row.get::<_, f64>(7)?,
                    row.get::<_, Option<f64>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                ))
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        (rows, global_stop, global_tp, ai_cfg)
    };
    if rows.is_empty() {
        return Ok(());
    }
    let provider = crate::providers::BinanceProvider::public("https://fapi.binance.com")
        .map_err(|error| format!("{error:?}"))?;
    for (id, symbol, side, entry, sl, tp, leverage, qty, invalidation_price, features) in rows {
        if closing.contains(&id) {
            continue;
        }
        let Ok(mark) = provider.public_mark_price(&symbol).await else {
            continue;
        };
        if !mark.is_finite() || mark <= 0.0 || entry <= 0.0 {
            continue;
        }
        let direction = if side == "long" { 1.0 } else { -1.0 };

        if let Ok(conn) = db.conn.lock() {
            let _ = apply_position_mark(&conn, id, mark, entry, qty, direction);
        }
        let limits = crate::mean_reversion::position_margin_limits(features.as_deref());
        let reason = fast_risk_reason(&FastRiskInput {
            side: &side,
            entry,
            mark,
            sl,
            tp,
            leverage,
            invalidation_price: gate_ai_value(invalidation_price, ai_cfg.use_invalidation_price),
            global_stop_pct: limits.map_or(global_stop, |limits| limits.0),
            global_tp_pct: limits.map_or(global_tp, |limits| limits.1),
        });
        let Some(reason) = reason else {
            continue;
        };
        closing.insert(id);
        if let Err(error) = control.close_position(id, reason).await {
            tracing::debug!(error = ?error, symbol = %symbol, reason, "快速风控平仓失败");
        }
        closing.remove(&id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn risk_input<'a>(
        side: &'a str,
        entry: f64,
        mark: f64,
        sl: f64,
        tp: f64,
        leverage: f64,
        invalidation_price: Option<f64>,
        global_stop_pct: f64,
        global_tp_pct: f64,
    ) -> FastRiskInput<'a> {
        FastRiskInput {
            side,
            entry,
            mark,
            sl,
            tp,
            leverage,
            invalidation_price,
            global_stop_pct,
            global_tp_pct,
        }
    }

    #[test]
    fn invalidation_price_triggers_fast_close() {
        // 快速风控需在 AI 失效价被触及时立即返回 invalidation：
        // 多单标记价跌到失效价、空单标记价涨到失效价即视为失效。
        // 全局止损阈值需高于杠杆放大后的浮亏（-4% × 5 = -20%），
        // 否则会先命中 global_stop_loss，无法验证失效价分支。
        assert_eq!(
            fast_risk_reason(&risk_input(
                "long",
                100.0,
                96.0,
                95.0,
                110.0,
                5.0,
                Some(97.0),
                30.0,
                15.0
            )),
            Some("invalidation")
        );
        assert_eq!(
            fast_risk_reason(&risk_input(
                "short",
                100.0,
                104.0,
                105.0,
                90.0,
                5.0,
                Some(103.0),
                30.0,
                15.0
            )),
            Some("invalidation")
        );
        assert_eq!(
            fast_risk_reason(&risk_input(
                "long",
                100.0,
                99.0,
                95.0,
                110.0,
                5.0,
                Some(97.0),
                30.0,
                15.0
            )),
            None
        );
    }

    #[test]
    fn fast_risk_ignores_invalidation_when_switch_off() {
        // 开关关闭时消费点把 invalidation_price 置空，失效价不再触发平仓。
        assert_eq!(
            fast_risk_reason(&risk_input(
                "long",
                100.0,
                96.0,
                95.0,
                110.0,
                5.0,
                gate_ai_value(Some(97.0), false),
                30.0,
                15.0,
            )),
            None
        );
        // 开关打开时与旧行为一致。
        assert_eq!(
            fast_risk_reason(&risk_input(
                "long",
                100.0,
                96.0,
                95.0,
                110.0,
                5.0,
                gate_ai_value(Some(97.0), true),
                30.0,
                15.0,
            )),
            Some("invalidation")
        );
    }

    #[test]
    fn invalidation_has_highest_priority() {
        // 失效价是 AI 的否决意见，优先级最高：
        // 即便同时满足 ROI 兜底，也应返回 invalidation。
        assert_eq!(
            fast_risk_reason(&risk_input(
                "long",
                100.0,
                90.0,
                0.0,
                0.0,
                5.0,
                Some(95.0),
                10.0,
                0.0
            )),
            Some("invalidation")
        );
    }

    #[test]
    fn atr_stop_precedes_roi_budget() {
        // sl=95、entry=100、lev=9：mark=94 同时跌破 ATR 保护价并越过 ROI 止损线，
        // 必须先返回 stop_loss，否则 ATR 保护价永远轮不到触发。
        assert_eq!(
            fast_risk_reason(&risk_input(
                "long", 100.0, 94.0, 95.0, 0.0, 9.0, None, 10.0, 0.0
            )),
            Some("stop_loss")
        );
    }

    #[test]
    fn roi_budget_still_triggers() {
        // 无 ATR 保护价（sl=0）时，ROI 兜底仍按保证金收益率平仓。
        assert_eq!(
            fast_risk_reason(&risk_input(
                "long", 100.0, 95.0, 0.0, 0.0, 2.0, None, 10.0, 0.0
            )),
            Some("global_stop_loss")
        );
    }

    #[test]
    fn fast_risk_reason_ignores_invalid_prices() {
        // entry/mark 非法时不得触发任何平仓，避免除零或脏数据误杀持仓。
        assert_eq!(
            fast_risk_reason(&risk_input(
                "long",
                0.0,
                96.0,
                95.0,
                110.0,
                5.0,
                Some(97.0),
                10.0,
                15.0
            )),
            None
        );
        assert_eq!(
            fast_risk_reason(&risk_input(
                "long",
                100.0,
                0.0,
                95.0,
                110.0,
                5.0,
                Some(97.0),
                10.0,
                15.0
            )),
            None
        );
    }

    #[test]
    fn fast_risk_reason_ignores_negative_and_nonfinite_invalidation() {
        // 失效价仅在有限正数时才参与判定。
        for ip in [-1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                fast_risk_reason(&risk_input(
                    "long",
                    100.0,
                    99.0,
                    0.0,
                    0.0,
                    5.0,
                    Some(ip),
                    0.0,
                    0.0
                )),
                None
            );
        }
    }
}
