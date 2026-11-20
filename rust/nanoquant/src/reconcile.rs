//! 持仓对账。本地账本与交易所持仓不一致时的处置，与 legacy 内核（已退役） `_reconcile_live` 一致。
//!
//! 三条规则：交易所有、本地没有 → 接管入账；两边都有 → 以交易所数量为准；
//! 本地有、交易所没有 → 判定为已在交易所平掉，补记一笔成交。
//! 只对账当前交易环境，另一个环境的仓位不能被判成「交易所已无持仓」。

/// 对账动作。
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReconcileAction {
    /// 交易所出现了本地没有的仓位，接管入账。`naked` 表示没有止损也没有止盈。
    Adopt {
        symbol: String,
        side: String,
        qty: f64,
        naked: bool,
    },
    /// 两边都有，用交易所的数量与标记价覆盖本地。
    Update {
        symbol: String,
        qty: f64,
        mark_price: f64,
    },
    /// 本地有仓但交易所已无持仓，按同步关闭并补记盈亏。
    CloseLocal { symbol: String, pnl: f64 },
}

pub struct VenuePosition {
    pub symbol: String,
    pub side: String,
    pub qty: f64,
    pub mark_price: f64,
    pub sl_price: f64,
    pub tp_price: f64,
}

pub struct LocalPosition {
    pub symbol: String,
    pub mode: String,
    pub side: String,
    pub qty: f64,
    pub entry_price: f64,
}

/// 对账。`mode` 决定看哪一套账本：模拟盘的仓位绝不能拿实盘持仓去关。
pub fn reconcile(
    mode: &str,
    venue: &[VenuePosition],
    local: &[LocalPosition],
    fee_rate: f64,
    exit_prices: &[(String, f64)],
    closing: &[(String, String)],
) -> Vec<ReconcileAction> {
    let mut actions = Vec::new();
    let mine: Vec<&LocalPosition> = local.iter().filter(|p| p.mode == mode).collect();

    for vp in venue {
        match mine
            .iter()
            .find(|p| p.symbol == vp.symbol && p.side == vp.side)
        {
            None => actions.push(ReconcileAction::Adopt {
                symbol: vp.symbol.clone(),
                side: vp.side.clone(),
                qty: vp.qty,
                naked: vp.sl_price <= 0.0 && vp.tp_price <= 0.0,
            }),
            Some(_) => actions.push(ReconcileAction::Update {
                symbol: vp.symbol.clone(),
                qty: vp.qty,
                mark_price: vp.mark_price,
            }),
        }
    }
    for lp in &mine {
        if venue
            .iter()
            .any(|v| v.symbol == lp.symbol && v.side == lp.side)
        {
            continue;
        }
        // 系统正在平这个仓：成交由平仓路径落库，对账再补一笔就是重复记账
        if closing
            .iter()
            .any(|(symbol, side)| symbol == &lp.symbol && side == &lp.side)
        {
            continue;
        }
        let exit_price = exit_prices
            .iter()
            .find(|(symbol, _)| symbol == &lp.symbol)
            .map_or(0.0, |(_, price)| *price);
        actions.push(ReconcileAction::CloseLocal {
            symbol: lp.symbol.clone(),
            pnl: realized_pnl(
                lp.side.as_str(),
                lp.entry_price,
                exit_price,
                lp.qty,
                fee_rate,
            ),
        });
    }
    actions
}

/// 平仓盈亏：价差减去开平两次手续费。方向为 long 时价格上涨盈利。
fn realized_pnl(side: &str, entry: f64, exit: f64, qty: f64, fee_rate: f64) -> f64 {
    let direction = if side == "long" { 1.0 } else { -1.0 };
    let gross = (exit - entry) * qty * direction;
    let fees = qty * (entry + exit) * fee_rate;
    round8(gross - fees)
}

fn round8(value: f64) -> f64 {
    (value * 1e8).round() / 1e8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn venue(symbol: &str, qty: f64, sl: f64) -> VenuePosition {
        VenuePosition {
            symbol: symbol.to_string(),
            side: "long".to_string(),
            qty,
            mark_price: 100.0,
            sl_price: sl,
            tp_price: 0.0,
        }
    }

    fn local(symbol: &str, mode: &str) -> LocalPosition {
        LocalPosition {
            symbol: symbol.to_string(),
            mode: mode.to_string(),
            side: "long".to_string(),
            qty: 1.0,
            entry_price: 100.0,
        }
    }

    #[test]
    fn venue_position_missing_locally_is_adopted_and_flagged_when_naked() {
        let actions = reconcile("live", &[venue("BTCUSDT", 0.5, 0.0)], &[], 0.0004, &[], &[]);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            ReconcileAction::Adopt { symbol, naked, .. } => {
                assert_eq!(symbol, "BTCUSDT");
                assert!(*naked, "没有止损止盈的外部仓位必须标成裸仓");
            }
            other => panic!("应为接管入账，得到 {other:?}"),
        }
    }

    #[test]
    fn local_position_absent_on_venue_is_closed_but_other_mode_is_untouched() {
        let local_rows = vec![local("ETHUSDT", "live"), local("SOLUSDT", "demo")];
        let actions = reconcile(
            "live",
            &[],
            &local_rows,
            0.0004,
            &[("ETHUSDT".to_string(), 100.0)],
            &[],
        );
        assert_eq!(actions.len(), 1, "模拟盘仓位不能被实盘对账关掉");
        match &actions[0] {
            ReconcileAction::CloseLocal { symbol, pnl } => {
                assert_eq!(symbol, "ETHUSDT");
                assert!(*pnl < 0.0, "平价离场仍要扣掉手续费");
            }
            other => panic!("应为同步关闭，得到 {other:?}"),
        }
    }

    #[test]
    fn matching_position_takes_venue_quantity_and_pnl_respects_direction() {
        let actions = reconcile(
            "live",
            &[venue("BTCUSDT", 0.3, 90.0)],
            &[local("BTCUSDT", "live")],
            0.0004,
            &[],
            &[],
        );
        assert!(matches!(&actions[0], ReconcileAction::Update { qty, .. } if *qty == 0.3));

        let mut short = local("BTCUSDT", "live");
        short.side = "short".to_string();
        // 空头在 90 平仓：价差盈利 10，扣掉手续费后仍应盈利
        let actions = reconcile(
            "live",
            &[],
            &[short],
            0.0004,
            &[("BTCUSDT".to_string(), 90.0)],
            &[],
        );
        match &actions[0] {
            ReconcileAction::CloseLocal { pnl, .. } => assert!(*pnl > 0.0, "空头价格下跌应当盈利"),
            other => panic!("应为同步关闭，得到 {other:?}"),
        }
    }

    #[test]
    fn close_pnl_uses_each_symbols_own_exit_price() {
        let local_rows = vec![local("BTCUSDT", "live"), local("ETHUSDT", "live")];
        let actions = reconcile(
            "live",
            &[],
            &local_rows,
            0.0004,
            &[
                ("BTCUSDT".to_string(), 110.0),
                ("ETHUSDT".to_string(), 90.0),
            ],
            &[],
        );
        assert_eq!(actions.len(), 2);
        match (&actions[0], &actions[1]) {
            (
                ReconcileAction::CloseLocal { pnl: btc, .. },
                ReconcileAction::CloseLocal { pnl: eth, .. },
            ) => {
                assert!(*btc > 0.0, "BTC 用 110 平仓应盈利");
                assert!(*eth < 0.0, "ETH 用 90 平仓应亏损");
            }
            other => panic!("应为两笔同步关闭，得到 {other:?}"),
        }
    }

    #[test]
    fn position_being_closed_is_not_booked_twice() {
        let actions = reconcile(
            "live",
            &[],
            &[local("BTCUSDT", "live")],
            0.0004,
            &[("BTCUSDT".to_string(), 110.0)],
            &[("BTCUSDT".to_string(), "long".to_string())],
        );
        assert!(actions.is_empty(), "平仓在途时对账再补记一笔就是重复成交");
    }

    #[test]
    fn hedge_sides_are_matched_independently() {
        let mut venue_short = venue("BTCUSDT", 2.0, 105.0);
        venue_short.side = "short".to_string();
        let actions = reconcile(
            "live",
            &[venue_short],
            &[local("BTCUSDT", "live")],
            0.0004,
            &[("BTCUSDT".to_string(), 100.0)],
            &[],
        );
        assert_eq!(actions.len(), 2, "同标的多空必须分别对账");
        assert!(matches!(
            &actions[0],
            ReconcileAction::Adopt { side, .. } if side == "short"
        ));
        assert!(matches!(
            &actions[1],
            ReconcileAction::CloseLocal { symbol, .. } if symbol == "BTCUSDT"
        ));
    }
}
