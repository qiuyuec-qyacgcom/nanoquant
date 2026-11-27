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
    /// 是否由风控熔断（日亏/回撤/连亏）触发。手动熔断为 false，
    /// `engine.close_on_halt` 只在风控熔断时平仓，手动暂停不能动仓位。
    pub risk_halt: bool,
}

impl Gate {
    fn open() -> Self {
        Self {
            ok: true,
            reason: String::new(),
            halt: String::new(),
            risk_halt: false,
        }
    }
    fn closed(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: reason.into(),
            halt: String::new(),
            risk_halt: false,
        }
    }
    fn halt(reason: impl Into<String>, halt: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: reason.into(),
            halt: halt.into(),
            risk_halt: true,
        }
    }
    /// 连亏冷静期：只拦本轮，不持久化，但属于风控熔断。
    fn risk_blocked(reason: impl Into<String>) -> Self {
        Self {
            ok: false,
            reason: reason.into(),
            halt: String::new(),
            risk_halt: true,
        }
    }
    /// 持久化的 halted：可能是手动暂停，不能当作风控熔断去平仓。
    /// 只有文本明确来自日亏/回撤时才视作风控熔断。
    fn persisted_halt(reason: impl Into<String>, halt: impl Into<String>) -> Self {
        let halt = halt.into();
        let risk_halt = halt.contains("日内亏损") || halt.contains("回撤");
        Self {
            ok: false,
            reason: reason.into(),
            halt,
            risk_halt,
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
        return Gate::risk_blocked(format!(
            "连亏 {} 笔, 冷静期剩余 {remain}s",
            input.consecutive_losses
        ));
    }
    if !input.halted.is_empty() {
        return Gate::persisted_halt(
            format!("引擎已熔断: {}", input.halted),
            input.halted.clone(),
        );
    }
    Gate::open()
}

/// 仓位估算用到的风控参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizeParams {
    pub margin_pct: f64,
    pub min_notional_buffer: f64,
}

/// 交易所按 `availableBalance` 校验初始保证金，留 10% 安全垫。
pub const MARGIN_SAFETY: f64 = 0.90;
/// 为跨过最小名义额缓冲，允许对目标名义额的最大放大倍数。
const MAX_BUFFER_OVERSHOOT: f64 = 1.10;

/// 保证金占比 → 名义额 → 数量，返回 `(qty, 说明)`。
///
/// 仓位只由 `margin_pct`（单仓保证金占权益比例）决定：
/// `名义额 = 权益 × margin_pct% × 杠杆`，再受可用余额约束；
/// 最后按步进对齐到**缓冲后**的最小量，宁可直接拒绝，也不贴着交易所下限下单。
pub fn estimate_qty(
    equity: f64,
    price: f64,
    leverage: f64,
    params: &SizeParams,
    filter: &crate::live::SymbolFilter,
    available: Option<f64>,
) -> (f64, String) {
    if !equity.is_finite() || equity <= 0.0 || !price.is_finite() || price <= 0.0 {
        return (0.0, "权益或价格无效".into());
    }
    if !leverage.is_finite() || leverage <= 0.0 {
        return (0.0, "杠杆无效".into());
    }
    let margin_cap = equity * params.margin_pct / 100.0 * leverage;
    let mut notional = margin_cap;

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

/// 趋势仓的波动保护止损价：价格距离 = `stop_distance_atr_mult × ATR%`。
///
/// 该口径与杠杆无关，避免「止损距离 = 保证金% ÷ 杠杆」随杠杆收缩到噪音区。
/// ATR% 或倍数非法时返回 0，让上游按「缺少有效止损价」拒单。
pub(crate) fn atr_stop_price(side: &str, price: f64, atr_pct: f64, mult: f64) -> f64 {
    if !price.is_finite()
        || price <= 0.0
        || !atr_pct.is_finite()
        || atr_pct <= 0.0
        || !mult.is_finite()
        || mult <= 0.0
    {
        return 0.0;
    }
    let distance = mult * atr_pct / 100.0;
    if distance >= 1.0 {
        return 0.0;
    }
    let direction = if side == "long" { 1.0 } else { -1.0 };
    price * (1.0 - direction * distance)
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
    fn sizing_uses_margin_percentage_of_equity() {
        // 保证金上限 100×50%×10 = 500 名义额 → 5 手。
        let (qty, note) = estimate_qty(100.0, 100.0, 10.0, &params(), &filter(), None);
        assert_eq!(note, "ok");
        assert!((qty - 5.0).abs() < 1e-9);
    }

    #[test]
    fn sizing_lifts_a_floored_qty_back_to_the_buffered_minimum() {
        // 名义额 5 → 0.05 手；缓冲后门槛 5.25 → 0.053 手，放大 6% 在允许范围内。
        let params = SizeParams {
            margin_pct: 50.0,
            min_notional_buffer: 1.05,
        };
        let (qty, _) = estimate_qty(10.0, 100.0, 1.0, &params, &filter(), None);
        assert!((qty - 0.053).abs() < 1e-9, "实际 {qty}");
    }

    #[test]
    fn sizing_respects_available_balance_with_safety_margin() {
        // 可用 1 USDT、10x、安全垫 0.9 → 最多 9 USDT 名义额。
        let (qty, _) = estimate_qty(100.0, 100.0, 10.0, &params(), &filter(), Some(1.0));
        assert!((qty - 0.09).abs() < 1e-9, "实际 {qty}");
    }

    #[test]
    fn sizing_rejects_when_the_minimum_does_not_fit() {
        let params = SizeParams {
            margin_pct: 5.0,
            min_notional_buffer: 1.05,
        };
        let (qty, note) = estimate_qty(10.0, 100.0, 1.0, &params, &filter(), None);
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
        assert!(gate.risk_halt, "连亏冷静期属于风控熔断");
    }

    #[test]
    fn only_risk_halts_are_marked_for_close_on_halt() {
        let mut manual = healthy();
        manual.halted = "手动暂停".into();
        let gate = global_gates(&manual);
        assert!(!gate.ok);
        assert!(!gate.risk_halt, "手动暂停不能触发 close_on_halt");

        let mut drawdown = healthy();
        drawdown.halted = "最大回撤触发熔断".into();
        let gate = global_gates(&drawdown);
        assert!(!gate.ok);
        assert!(gate.risk_halt, "持久化的回撤熔断仍属风控熔断");

        let mut daily = healthy();
        daily.equity = 940.0;
        let gate = global_gates(&daily);
        assert!(!gate.ok && gate.risk_halt, "日内亏损必须标记为风控熔断");
    }

    #[test]
    fn atr_stop_distance_is_multiple_of_atr_without_leverage() {
        // 3.55% ATR × 1.3 = 4.615% 价格距离，多头止损价 95.385、空头 104.615。
        let long = atr_stop_price("long", 100.0, 3.55, 1.3);
        let short = atr_stop_price("short", 100.0, 3.55, 1.3);
        assert!((long - 95.385).abs() < 1e-9, "实际 {long}");
        assert!((short - 104.615).abs() < 1e-9, "实际 {short}");
        // 同样的 ATR 与倍数不因任何杠杆输入而改变。
        assert_eq!(long, atr_stop_price("long", 100.0, 3.55, 1.3));
        assert_eq!(atr_stop_price("long", 100.0, 0.0, 1.3), 0.0);
        assert_eq!(atr_stop_price("long", 100.0, 3.55, 0.0), 0.0);
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

    #[test]
    fn sizing_is_capped_by_available_margin_not_equity() {
        // 权益 1000 但可用只剩 10：按权益能开很大，按可用余额只能开 10×0.9×10 = 90 名义额
        let (qty, note) = estimate_qty(1000.0, 100.0, 10.0, &params(), &filter(), Some(10.0));
        assert_eq!(note, "ok");
        assert!(
            qty <= 0.9 + 1e-9,
            "数量必须受可用余额限制，否则会被 -2019 拒绝"
        );

        let (zero, why) = estimate_qty(1000.0, 100.0, 10.0, &params(), &filter(), Some(0.2));
        assert_eq!(zero, 0.0);
        assert!(why.contains("可用保证金不足"), "{why}");
    }
}
