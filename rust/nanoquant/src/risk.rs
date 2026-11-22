//! 开仓闸门。所有拒绝都带可读原因，回答「这一轮为什么没开仓」。
//!
//! 顺序与 legacy 内核（已退役） `engine/risk.py::global_gates` 一致：权益下限 → 日内亏损 → 最大回撤 →
//! 连亏冷静期 → 已熔断。先命中者生效，后面的不再看。

/// 闸门结论。`halt` 非空才写进持久化的熔断状态：冷静期到点自愈，不能持久化，
/// 否则到期后仍被判熔断，引擎被永久锁死。
#[derive(Debug, PartialEq)]
pub struct Gate {
    pub ok: bool,
    pub reason: String,
    pub halt: String,
}

impl Gate {
    fn open() -> Self {
        Self {
            ok: true,
            reason: String::new(),
            halt: String::new(),
        }
    }
    fn closed(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: reason.into(),
            halt: String::new(),
        }
    }
    fn halt(reason: impl Into<String>, halt: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: reason.into(),
            halt: halt.into(),
        }
    }
}

/// 账户与风控阈值。百分比都是正数，如日内亏损上限 5 表示跌 5% 触发。
pub struct RiskInput {
    pub equity: f64,
    pub day_start_equity: f64,
    pub peak_equity: f64,
    pub min_equity: f64,
    pub daily_loss_limit_pct: f64,
    pub max_drawdown_pct: f64,
    pub consecutive_losses: i64,
    pub consecutive_loss_limit: i64,
    pub cooldown_until: i64,
    pub now: i64,
    pub halted: String,
}

pub fn global_gates(input: &RiskInput) -> Gate {
    if input.equity <= 0.0 {
        return Gate::closed("账户权益为 0");
    }
    if input.equity < input.min_equity {
        return Gate::closed(format!(
            "权益 {:.4} USDT 低于下限 {} USDT",
            input.equity, input.min_equity
        ));
    }
    if input.day_start_equity > 0.0 {
        let day_pnl_pct = (input.equity - input.day_start_equity) / input.day_start_equity * 100.0;
        if day_pnl_pct <= -input.daily_loss_limit_pct {
            let halt = format!(
                "日内亏损 {day_pnl_pct:.2}% 超过上限 {}%",
                input.daily_loss_limit_pct
            );
            return Gate::halt(format!("触发{halt}"), halt);
        }
    }
    if input.peak_equity > 0.0 {
        let dd = (input.peak_equity - input.equity) / input.peak_equity * 100.0;
        if dd >= input.max_drawdown_pct {
            let halt = format!("最大回撤 {dd:.2}% 超过上限 {}%", input.max_drawdown_pct);
            return Gate::halt(format!("触发{halt}"), halt);
        }
    }
    if input.consecutive_losses >= input.consecutive_loss_limit && input.now < input.cooldown_until
    {
        let remain = input.cooldown_until - input.now;
        return Gate::closed(format!(
            "连亏 {} 笔, 冷静期剩余 {remain}s",
            input.consecutive_losses
        ));
    }
    if !input.halted.is_empty() {
        return Gate::halt(
            format!("引擎已熔断: {}", input.halted),
            input.halted.clone(),
        );
    }
    Gate::open()
}

/// 一笔在仓，只保留闸门要看的字段。
#[cfg(test)]
pub struct OpenPosition {
    pub symbol: String,
    pub side: String,
}

/// 单标的闸门：已持有该标的、持仓数上限、同向上限、冷却。
///
/// `same_direction_limit <= 0` 表示不限制同向。冷却只看该标的自己的到期时间。
#[cfg(test)]
pub fn symbol_gates(
    symbol: &str,
    side: &str,
    positions: &[OpenPosition],
    max_positions: usize,
    same_direction_limit: i64,
    cooldown_until: i64,
    now: i64,
) -> Gate {
    if positions.iter().any(|p| p.symbol == symbol) {
        return Gate::closed("该标的已有持仓");
    }
    if positions.len() >= max_positions {
        return Gate::closed(format!("持仓数已达上限 {max_positions}"));
    }
    if same_direction_limit > 0 && !side.is_empty() {
        let same = positions.iter().filter(|p| p.side == side).count() as i64;
        if same >= same_direction_limit {
            return Gate::closed(format!(
                "同方向持仓已达上限 {same_direction_limit} 个 ({side}), 避免相关性叠加"
            ));
        }
    }
    if now < cooldown_until {
        return Gate::closed(format!("标的冷却中, 剩余 {}s", cooldown_until - now));
    }
    Gate::open()
}

/// 杠杆夹取。上限取全局上限、交易所上限、100 的最小者；没有建议值时用下限。
///
/// AI 或信号给的杠杆只是建议：低于下限抬上去，高于上限压下来。
/// 交易所允许的上限更低时以交易所为准，安全优先。
#[cfg(test)]
pub fn choose_leverage(global_min: i64, global_max: i64, exchange_max: i64, suggested: f64) -> i64 {
    let upper = 1.max(global_max.min(exchange_max).min(100));
    let lower = 1.max(global_min.min(upper));
    let desired = if suggested > 0.0 {
        suggested.round() as i64
    } else {
        lower
    };
    lower.max(desired.min(upper))
}

/// 仓位估算用到的风控参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeParams {
    pub risk_per_trade_pct: f64,
    pub margin_pct: f64,
    pub min_notional_buffer: f64,
}

/// 交易所按 `availableBalance` 校验初始保证金，留 10% 安全垫。
pub const MARGIN_SAFETY: f64 = 0.90;
/// 为跨过最小名义额缓冲，允许对风险预算的最大放大倍数。
const MAX_BUFFER_OVERSHOOT: f64 = 1.10;

/// 风险预算 → 名义额 → 数量，返回 `(qty, 说明)`。
///
/// 与 legacy 内核（已退役） `broker.estimate_qty` 同口径：名义额取「风险预算」与「保证金上限」
/// 的较小值，再受可用余额约束；最后按步进对齐到**缓冲后**的最小量，
/// 宁可直接拒绝，也不贴着交易所下限下单。
pub fn estimate_qty(
    equity: f64,
    price: f64,
    leverage: f64,
    params: &SizeParams,
    sl_pct: f64,
    filter: &crate::live::SymbolFilter,
    risk_scale: f64,
    available: Option<f64>,
) -> (f64, String) {
    if !equity.is_finite() || equity <= 0.0 || !price.is_finite() || price <= 0.0 {
        return (0.0, "权益或价格无效".into());
    }
    if !leverage.is_finite() || leverage <= 0.0 {
        return (0.0, "杠杆无效".into());
    }
    let risk_budget = equity * params.risk_per_trade_pct / 100.0 * risk_scale.max(0.0);
    let risk_notional = risk_budget / sl_pct.max(1e-6);
    let margin_cap = equity * params.margin_pct / 100.0 * leverage;
    let mut notional = risk_notional.min(margin_cap);

    let mut affordable = 0.0;
    if let Some(available) = available {
        affordable = available.max(0.0) * MARGIN_SAFETY * leverage;
        notional = notional.min(affordable);
    }

    let buffer = params.min_notional_buffer.clamp(1.0, 3.0);
    let need = filter.effective_min_notional(price) * buffer;
    let min_qty = filter.effective_min_qty(price);
    let min_qty_buffered = min_qty.max(filter.ceil_qty(need / price));
    let mut qty = filter.floor_qty(notional / price);
    // 下取整把量压到缓冲门槛以下时，抬回缓冲后的最小量：
    // 只在保证金够、且缓冲要求没有远超风险预算（10% 以内）时抬。
    if min_qty_buffered > 0.0 && qty < min_qty_buffered - 1e-12 {
        let target = min_qty_buffered * price;
        if target <= notional * MAX_BUFFER_OVERSHOOT
            && (available.is_none() || affordable + 1e-9 >= target)
        {
            qty = min_qty_buffered;
        }
    }
    if qty <= 0.0 || qty < min_qty_buffered - 1e-12 {
        if available.is_some() && affordable < need {
            return (
                0.0,
                format!(
                    "可用保证金不足: 可用 {:.2} USDT, 按 {leverage}x 最多可开 {affordable:.2} USDT 名义额, 但该标的最小名义额约 {need:.2} USDT（含 {buffer}x 缓冲）",
                    available.unwrap_or(0.0)
                ),
            );
        }
        return (
            0.0,
            format!(
                "可分配名义额 {notional:.4} USDT 不足该标的最小可下单量（约需 {need:.2} USDT, 最小数量 {min_qty_buffered} @ {price:.6}）"
            ),
        );
    }
    if let Err(reason) = filter.validate(qty, price) {
        return (
            0.0,
            format!("{reason}（该标的有效最小名义额约 {need:.2} USDT）"),
        );
    }
    (qty, "ok".into())
}

/// 交易时段。窗口形如 `09:00-12:00`，结束不大于开始视为跨日。空列表表示全天可交易。
pub fn trading_window_open(windows: &[String], minutes_now: i64) -> bool {
    if windows.is_empty() {
        return true;
    }
    for raw in windows {
        let Some((start_s, end_s)) = raw.split_once('-') else {
            continue;
        };
        let (Some(start), Some(end)) = (clock_minutes(start_s), clock_minutes(end_s)) else {
            continue;
        };
        if start <= end {
            if (start..end).contains(&minutes_now) {
                return true;
            }
        } else if minutes_now >= start || minutes_now < end {
            return true;
        }
    }
    false
}

fn clock_minutes(text: &str) -> Option<i64> {
    let (h, m) = text.trim().split_once(':')?;
    Some(h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> SizeParams {
        SizeParams {
            risk_per_trade_pct: 10.0,
            margin_pct: 50.0,
            min_notional_buffer: 1.05,
        }
    }

    fn filter() -> crate::live::SymbolFilter {
        crate::live::SymbolFilter {
            qty_step: 0.001,
            min_qty: 0.001,
            price_tick: 0.01,
            min_notional: 5.0,
        }
    }

    #[test]
    fn sizing_takes_the_smaller_of_risk_budget_and_margin_cap() {
        // 风险预算 100×10%÷2% = 500；保证金上限 100×50%×10 = 500。
        let (qty, note) = estimate_qty(100.0, 100.0, 10.0, &params(), 0.02, &filter(), 1.0, None);
        assert_eq!(note, "ok");
        assert!((qty - 5.0).abs() < 1e-9);
    }

    #[test]
    fn sizing_lifts_a_floored_qty_back_to_the_buffered_minimum() {
        // 名义额 5 → 0.05 手；缓冲后门槛 5.25 → 0.053 手，放大 6% 在允许范围内。
        let params = SizeParams {
            risk_per_trade_pct: 10.0,
            margin_pct: 50.0,
            min_notional_buffer: 1.05,
        };
        let (qty, _) = estimate_qty(10.0, 100.0, 1.0, &params, 0.005, &filter(), 1.0, None);
        assert!((qty - 0.053).abs() < 1e-9, "实际 {qty}");
    }

    #[test]
    fn sizing_respects_available_balance_with_safety_margin() {
        // 可用 1 USDT、10x、安全垫 0.9 → 最多 9 USDT 名义额。
        let (qty, _) = estimate_qty(
            100.0,
            100.0,
            10.0,
            &params(),
            0.02,
            &filter(),
            1.0,
            Some(1.0),
        );
        assert!((qty - 0.09).abs() < 1e-9, "实际 {qty}");
    }

    #[test]
    fn sizing_rejects_when_the_minimum_does_not_fit() {
        let params = SizeParams {
            risk_per_trade_pct: 10.0,
            margin_pct: 5.0,
            min_notional_buffer: 1.05,
        };
        let (qty, note) = estimate_qty(10.0, 100.0, 1.0, &params, 0.005, &filter(), 1.0, None);
        assert_eq!(qty, 0.0);
        assert!(note.contains("不足"), "{note}");
    }

    fn healthy() -> RiskInput {
        RiskInput {
            equity: 1000.0,
            day_start_equity: 1000.0,
            peak_equity: 1000.0,
            min_equity: 50.0,
            daily_loss_limit_pct: 5.0,
            max_drawdown_pct: 20.0,
            consecutive_losses: 0,
            consecutive_loss_limit: 3,
            cooldown_until: 0,
            now: 1_000,
            halted: String::new(),
        }
    }

    #[test]
    fn daily_loss_halts_before_drawdown() {
        let mut input = healthy();
        input.equity = 940.0;
        input.peak_equity = 2000.0;
        let gate = global_gates(&input);
        assert!(!gate.ok);
        assert!(
            gate.halt.contains("日内亏损"),
            "日内亏损先于回撤命中，且必须持久化"
        );
        assert!(!gate.halt.contains("回撤"));
    }

    #[test]
    fn cooldown_blocks_this_round_but_is_not_persisted() {
        let mut input = healthy();
        input.consecutive_losses = 3;
        input.cooldown_until = 1_500;
        let gate = global_gates(&input);
        assert!(!gate.ok);
        assert!(gate.reason.contains("冷静期"));
        assert!(
            gate.halt.is_empty(),
            "冷静期到点自愈，持久化会把引擎永久锁死"
        );
    }

    #[test]
    fn overnight_window_is_open_after_midnight() {
        let windows = vec!["22:00-02:00".to_string()];
        assert!(trading_window_open(&windows, 23 * 60));
        assert!(
            trading_window_open(&windows, 60),
            "跨日窗口在午夜后仍应开放"
        );
        assert!(!trading_window_open(&windows, 12 * 60));
        assert!(trading_window_open(&[], 12 * 60), "未配置窗口即全天可交易");
    }

    fn pos(symbol: &str, side: &str) -> OpenPosition {
        OpenPosition {
            symbol: symbol.to_string(),
            side: side.to_string(),
        }
    }

    #[test]
    fn symbol_gate_blocks_duplicate_and_same_direction_stacking() {
        let held = vec![pos("BTCUSDT", "long"), pos("ETHUSDT", "long")];
        assert_eq!(
            symbol_gates("BTCUSDT", "long", &held, 5, 3, 0, 0).reason,
            "该标的已有持仓"
        );
        let cap = symbol_gates("SOLUSDT", "long", &held, 5, 2, 0, 0);
        assert!(cap.reason.contains("同方向"), "两个多头已达同向上限");
        assert!(
            symbol_gates("SOLUSDT", "short", &held, 5, 2, 0, 0).ok,
            "反向不受同向上限影响"
        );
        assert!(
            symbol_gates("SOLUSDT", "long", &held, 5, 0, 0, 0).ok,
            "上限为 0 表示不限制"
        );
    }

    #[test]
    fn sizing_is_capped_by_available_margin_not_equity() {
        // 权益 1000 但可用只剩 10：按权益能开很大，按可用余额只能开 10×0.9×10 = 90 名义额
        let (qty, note) = estimate_qty(
            1000.0,
            100.0,
            10.0,
            &params(),
            0.01,
            &filter(),
            1.0,
            Some(10.0),
        );
        assert_eq!(note, "ok");
        assert!(
            qty <= 0.9 + 1e-9,
            "数量必须受可用余额限制，否则会被 -2019 拒绝"
        );

        let (zero, why) = estimate_qty(
            1000.0,
            100.0,
            10.0,
            &params(),
            0.01,
            &filter(),
            1.0,
            Some(0.2),
        );
        assert_eq!(zero, 0.0);
        assert!(why.contains("可用保证金不足"), "{why}");
    }

    #[test]
    fn leverage_is_clamped_to_exchange_ceiling() {
        assert_eq!(choose_leverage(20, 50, 125, 0.0), 20, "无建议值时用下限");
        assert_eq!(
            choose_leverage(20, 50, 125, 80.0),
            50,
            "建议值不能超过全局上限"
        );
        assert_eq!(
            choose_leverage(20, 50, 10, 30.0),
            10,
            "交易所上限更低时以交易所为准"
        );
        assert_eq!(choose_leverage(20, 50, 125, 8.0), 20, "低于下限抬到下限");
    }
}
