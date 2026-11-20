//! 持仓管理。出场判定与 legacy 内核（已退役） `detect_exit` / `_manage_positions` 一致。
//!
//! 判定顺序固定：部分止盈 → 硬止损 → 止盈 → 移动止损 → 信号反转 → 保本上移。
//! 移动止损必须先浮盈到激活门槛才接管，否则开仓即追踪会把单子在硬止损之前
//! 被噪声扫掉。部分止盈的 R 用**初始**止损计算，当前止损上移后会失真。

/// 管仓动作。
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ManageAction {
    /// 浮盈达到 N 倍初始风险，先平掉一部分。
    PartialTakeProfit {
        qty: f64,
    },
    /// 全部平仓，附带原因。
    Close {
        reason: String,
    },
    /// 止损上移到保本价。必须连同止盈一起重挂，否则止盈单被撤掉不再补。
    MoveToBreakeven {
        new_sl: f64,
    },
    Hold,
}

/// 一笔在管的仓位。
#[derive(Clone)]
pub struct ManagedPosition {
    pub side: String,
    pub entry_price: f64,
    pub mark_price: f64,
    /// 持仓期间的最有利价格：多头取最高，空头取最低。
    pub peak_price: f64,
    pub sl_price: f64,
    /// 开仓时的止损，部分止盈用它算 R，不用会随移动止损变化的当前止损。
    pub initial_sl: f64,
    pub tp_price: f64,
    pub qty: f64,
    /// 已部分止盈过就不再触发第二次。
    pub partial_taken: bool,
    /// 当轮重算的方向分。开仓时写入的分数不会随行情反转。
    pub score: f64,
    /// 持仓时长（分钟），用于时间阶梯止盈。
    pub held_minutes: f64,
}

/// 管仓参数。
#[derive(Debug, Clone, PartialEq)]
pub struct ManageConfig {
    /// 浮盈达到多少个 R 时部分止盈，0 表示关闭。
    pub partial_tp_at_r: f64,
    /// 部分止盈平掉的比例。
    pub partial_tp_ratio: f64,
    /// 移动止损回撤百分比，0 表示关闭。
    pub trailing_pct: f64,
    /// 移动止损激活门槛：浮盈先达到多少个 R 才开始追踪。
    pub trailing_arm_r: f64,
    /// 信号反转阈值。
    pub exit_threshold: f64,
    /// 浮盈达到多少个 R 时把止损上移到保本，0 表示关闭。
    pub breakeven_at_r: f64,
    /// 全局止损：价格相对开仓价不利变动达到该百分比就整仓平仓，0 表示关闭。
    pub global_stop_loss_pct: f64,
    /// 全局止盈：价格相对开仓价有利变动达到该百分比就整仓平仓，0 表示关闭。
    pub global_take_profit_pct: f64,
    pub qty_step: f64,
    pub min_qty: f64,
    /// 时间阶梯止盈 [[分钟, 收益率%]]。
    pub roi_table: Vec<(f64, f64)>,
}

pub fn manage(pos: &ManagedPosition, cfg: &ManageConfig) -> ManageAction {
    let direction = if pos.side == "long" { 1.0 } else { -1.0 };
    let pnl_pct = if pos.entry_price > 0.0 {
        (pos.mark_price - pos.entry_price) * direction / pos.entry_price * 100.0
    } else {
        0.0
    };
    if cfg.global_stop_loss_pct > 0.0 && pnl_pct <= -cfg.global_stop_loss_pct {
        return ManageAction::Close {
            reason: "global_stop_loss".to_string(),
        };
    }
    if cfg.global_take_profit_pct > 0.0 && pnl_pct >= cfg.global_take_profit_pct {
        return ManageAction::Close {
            reason: "global_take_profit".to_string(),
        };
    }

    if cfg.partial_tp_at_r > 0.0 && !pos.partial_taken {
        let r = partial_tp_r(&pos.side, pos.entry_price, pos.initial_sl, pos.mark_price);
        if r >= cfg.partial_tp_at_r {
            let qty = partial_close_qty(pos.qty, cfg.partial_tp_ratio, cfg.qty_step, cfg.min_qty);
            if qty > 0.0 {
                return ManageAction::PartialTakeProfit { qty };
            }
        }
    }

    if pos.sl_price > 0.0 && hit(direction, pos.mark_price, pos.sl_price, true) {
        return ManageAction::Close {
            reason: "stop_loss".to_string(),
        };
    }
    if pos.tp_price > 0.0 && hit(direction, pos.mark_price, pos.tp_price, false) {
        return ManageAction::Close {
            reason: "take_profit".to_string(),
        };
    }

    if let Some(target) = roi_target(&cfg.roi_table, pos.held_minutes) {
        let roi_pct = (pos.mark_price - pos.entry_price) * direction / pos.entry_price * 100.0;
        if roi_pct >= target {
            return ManageAction::Close {
                reason: "roi_target".to_string(),
            };
        }
    }

    if cfg.trailing_pct > 0.0 && pos.peak_price > 0.0 && pos.entry_price > 0.0 {
        let risk = if pos.sl_price > 0.0 {
            (pos.entry_price - pos.sl_price).abs()
        } else {
            pos.entry_price * 0.005
        };
        let favor = (pos.peak_price - pos.entry_price) * direction;
        if favor >= risk * cfg.trailing_arm_r {
            let pulled_back = if direction > 0.0 {
                pos.mark_price <= pos.peak_price * (1.0 - cfg.trailing_pct / 100.0)
            } else {
                pos.mark_price >= pos.peak_price * (1.0 + cfg.trailing_pct / 100.0)
            };
            if pulled_back {
                return ManageAction::Close {
                    reason: "trailing_stop".to_string(),
                };
            }
        }
    }

    if direction > 0.0 && pos.score <= -cfg.exit_threshold {
        return ManageAction::Close {
            reason: "signal_flip".to_string(),
        };
    }
    if direction < 0.0 && pos.score >= cfg.exit_threshold {
        return ManageAction::Close {
            reason: "signal_flip".to_string(),
        };
    }

    if pos.sl_price > 0.0 && cfg.breakeven_at_r > 0.0 {
        let r = (pos.mark_price - pos.entry_price) * direction
            / (pos.entry_price - pos.sl_price).abs().max(1e-9);
        if r >= cfg.breakeven_at_r {
            let new_sl = pos.entry_price * (1.0 + 0.0004 * direction);
            let tighter = (direction > 0.0 && new_sl > pos.sl_price)
                || (direction < 0.0 && new_sl < pos.sl_price);
            if tighter {
                return ManageAction::MoveToBreakeven { new_sl };
            }
        }
    }
    ManageAction::Hold
}

pub(crate) fn roi_target(table: &[(f64, f64)], minutes: f64) -> Option<f64> {
    table
        .iter()
        .filter(|(at, _)| minutes >= *at)
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, pct)| *pct)
}

fn hit(direction: f64, mark: f64, level: f64, is_stop: bool) -> bool {
    if direction > 0.0 {
        if is_stop {
            mark <= level
        } else {
            mark >= level
        }
    } else if is_stop {
        mark >= level
    } else {
        mark <= level
    }
}

/// 浮盈相当于多少个初始风险。初始止损缺失时返回 0，不能拿 0 当止损算出假 R。
fn partial_tp_r(side: &str, entry: f64, initial_sl: f64, mark: f64) -> f64 {
    if initial_sl <= 0.0 || entry <= 0.0 {
        return 0.0;
    }
    let risk = (entry - initial_sl).abs();
    if risk <= 0.0 {
        return 0.0;
    }
    let direction = if side == "long" { 1.0 } else { -1.0 };
    (mark - entry) * direction / risk
}

/// 部分止盈的下单量：按步进取整，取整后低于最小量就返回 0，避免发出必被拒的单。
fn partial_close_qty(qty: f64, ratio: f64, step: f64, min_qty: f64) -> f64 {
    if qty <= 0.0 || ratio <= 0.0 {
        return 0.0;
    }
    let mut raw = qty * ratio;
    if step > 0.0 {
        raw = (raw / step + 1e-12).floor() * step;
    }
    if raw <= 0.0 || raw < min_qty - 1e-12 {
        return 0.0;
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_pos() -> ManagedPosition {
        ManagedPosition {
            side: "long".to_string(),
            entry_price: 100.0,
            mark_price: 100.0,
            peak_price: 100.0,
            sl_price: 98.0,
            initial_sl: 98.0,
            tp_price: 106.0,
            qty: 1.0,
            partial_taken: false,
            score: 0.0,
            held_minutes: 0.0,
        }
    }

    fn cfg() -> ManageConfig {
        ManageConfig {
            partial_tp_at_r: 2.0,
            partial_tp_ratio: 0.5,
            trailing_pct: 1.0,
            trailing_arm_r: 1.5,
            exit_threshold: 0.6,
            breakeven_at_r: 1.0,
            global_stop_loss_pct: 0.0,
            global_take_profit_pct: 0.0,
            qty_step: 0.001,
            min_qty: 0.001,
            roi_table: Vec::new(),
        }
    }

    #[test]
    fn trailing_stop_does_not_arm_before_profit_reaches_threshold() {
        let mut pos = long_pos();
        // 最高只到过 100.5（0.25R），现价回撤 1%：未达激活门槛，不能被移动止损扫掉
        pos.peak_price = 100.5;
        pos.mark_price = 99.4;
        assert_eq!(
            manage(&pos, &cfg()),
            ManageAction::Hold,
            "未激活的移动止损会在硬止损前被噪声扫掉"
        );
    }

    #[test]
    fn partial_take_profit_uses_initial_risk_and_floors_to_step() {
        let mut pos = long_pos();
        pos.mark_price = 104.0; // 初始风险 2，浮盈 4，正好 2R
        pos.sl_price = 103.0; // 止损已上移，但 R 仍按初始止损算
        match manage(&pos, &cfg()) {
            ManageAction::PartialTakeProfit { qty } => assert!((qty - 0.5).abs() < 1e-9),
            other => panic!("应部分止盈，得到 {other:?}"),
        }

        let mut tiny = cfg();
        tiny.min_qty = 0.6;
        assert_eq!(
            manage(&pos, &tiny),
            ManageAction::Hold,
            "取整后低于最小量必须跳过，不能发必被拒的单"
        );
    }

    #[test]
    fn breakeven_move_only_tightens_the_stop() {
        let mut pos = long_pos();
        pos.mark_price = 102.5; // 浮盈 1.25R，超过保本门槛 1R
        pos.partial_taken = true; // 部分止盈已做过，本轮只看保本
        match manage(&pos, &cfg()) {
            ManageAction::MoveToBreakeven { new_sl } => assert!(
                new_sl > 100.0 && new_sl > pos.sl_price,
                "保本价应高于成本且收紧止损"
            ),
            other => panic!("应上移到保本，得到 {other:?}"),
        }

        pos.sl_price = 100.5; // 止损已经比保本价更紧
        assert_eq!(
            manage(&pos, &cfg()),
            ManageAction::Hold,
            "保本上移只能收紧止损，不能放宽"
        );
    }

    #[test]
    fn short_stop_loss_triggers_when_price_rises_through_it() {
        let mut pos = long_pos();
        pos.side = "short".to_string();
        pos.sl_price = 103.0;
        pos.mark_price = 103.5;
        assert_eq!(
            manage(&pos, &cfg()),
            ManageAction::Close {
                reason: "stop_loss".to_string()
            }
        );
    }

    #[test]
    fn signal_flip_uses_the_fresh_score_not_the_entry_score() {
        let mut pos = long_pos();
        pos.score = -0.8; // 当轮重算后方向分翻到空头
        assert_eq!(
            manage(&pos, &cfg()),
            ManageAction::Close {
                reason: "signal_flip".to_string()
            }
        );

        pos.score = 0.9; // 开仓时的分数一直是正的，不会触发反转
        assert_eq!(manage(&pos, &cfg()), ManageAction::Hold);
    }

    #[test]
    fn partial_take_profit_at_one_r_uses_half_ratio() {
        let mut pos = long_pos();
        pos.mark_price = 102.0;
        let mut config = cfg();
        config.partial_tp_at_r = 1.0;
        match manage(&pos, &config) {
            ManageAction::PartialTakeProfit { qty } => assert!((qty - 0.5).abs() < 1e-9),
            other => panic!("expected partial tp, got {other:?}"),
        }
    }

    #[test]
    fn global_stop_loss_closes_at_price_percentage() {
        let mut pos = long_pos();
        pos.sl_price = 0.0;
        pos.tp_price = 0.0;
        pos.mark_price = 98.0;
        let mut config = cfg();
        config.global_stop_loss_pct = 2.0;
        assert_eq!(
            manage(&pos, &config),
            ManageAction::Close {
                reason: "global_stop_loss".to_string()
            }
        );
    }

    #[test]
    fn global_take_profit_closes_short_at_price_percentage() {
        let mut pos = long_pos();
        pos.side = "short".to_string();
        pos.sl_price = 0.0;
        pos.tp_price = 0.0;
        pos.mark_price = 97.5;
        let mut config = cfg();
        config.global_take_profit_pct = 2.0;
        assert_eq!(
            manage(&pos, &config),
            ManageAction::Close {
                reason: "global_take_profit".to_string()
            }
        );
    }

    #[test]
    fn global_take_profit_takes_priority_over_per_position_tp() {
        let mut pos = long_pos();
        pos.tp_price = 101.0;
        pos.mark_price = 105.0;
        let mut config = cfg();
        config.global_take_profit_pct = 3.0;
        assert_eq!(
            manage(&pos, &config),
            ManageAction::Close {
                reason: "global_take_profit".to_string()
            },
            "全局止盈先于持仓自身的止盈价"
        );
    }

    #[test]
    fn zero_global_rate_disables_the_rule() {
        let mut pos = long_pos();
        pos.sl_price = 0.0;
        pos.tp_price = 0.0;
        pos.mark_price = 98.0;
        assert_eq!(manage(&pos, &cfg()), ManageAction::Hold);
    }

    #[test]
    fn time_ladder_closes_when_roi_reaches_threshold() {
        let mut pos = long_pos();
        pos.mark_price = 100.5;
        pos.held_minutes = 31.0;
        let mut config = cfg();
        config.roi_table = vec![
            (0.0, 0.8),
            (15.0, 0.6),
            (30.0, 0.45),
            (60.0, 0.3),
            (120.0, 0.2),
        ];
        assert_eq!(
            manage(&pos, &config),
            ManageAction::Close {
                reason: "roi_target".to_string()
            }
        );
    }

    #[test]
    fn time_ladder_holds_below_threshold() {
        let mut pos = long_pos();
        pos.mark_price = 100.5;
        pos.held_minutes = 10.0;
        let mut config = cfg();
        config.roi_table = vec![(0.0, 0.8), (15.0, 0.6), (30.0, 0.45)];
        assert_eq!(manage(&pos, &config), ManageAction::Hold);
    }
}
