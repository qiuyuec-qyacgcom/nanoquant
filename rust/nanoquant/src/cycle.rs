//! 一轮调度。与 legacy 内核（已退役） `cycle` 的阶段顺序一致。
//!
//! 顺序固定：对账 → 管仓 → 交易时段 → 全局闸门 → 扫描。
//! 先管仓再看闸门：闸门不过时仍要把该平的仓平掉，熔断不能把亏损仓位留在场内。
//! 引擎未启用时只记心跳，对账、管仓、扫描全部跳过。

use crate::manage::{manage, ManageAction, ManageConfig, ManagedPosition};
use crate::reconcile::{reconcile, LocalPosition, ReconcileAction, VenuePosition};
use crate::risk::{global_gates, trading_window_open, Gate, RiskInput};
use crate::scan::{select, Candidate, ScanGates};

/// 一轮的产出。
#[derive(Debug, PartialEq)]
pub struct CycleReport {
    /// 引擎未启用或闸门拦截时的原因，空串表示正常跑完。
    pub skipped: String,
    pub reconcile: Vec<ReconcileAction>,
    pub manage: Vec<ManageAction>,
    /// 本轮送给 AI 的标的。
    pub scanned: Vec<String>,
}

/// 一轮所需的输入，全部由调用方准备好，调度本身不碰网络和数据库。
pub struct CycleInput<'a> {
    pub enabled: bool,
    pub mode: &'a str,
    pub now_minute: i64,
    pub windows: &'a [String],
    pub risk: &'a RiskInput,
    pub venue: &'a [VenuePosition],
    pub local: &'a [LocalPosition],
    pub fee_rate: f64,
    pub exit_price: f64,
    pub closing: &'a [String],
    pub positions: &'a [ManagedPosition],
    pub manage_cfg: &'a ManageConfig,
    pub candidates: &'a [Candidate],
    pub scan_gates: &'a ScanGates,
    pub budget: usize,
}

pub fn run_cycle(input: &CycleInput) -> CycleReport {
    if !input.enabled {
        return CycleReport {
            skipped: "引擎未启用".to_string(),
            reconcile: vec![],
            manage: vec![],
            scanned: vec![],
        };
    }

    let reconcile_actions = reconcile(
        input.mode,
        input.venue,
        input.local,
        input.fee_rate,
        input.exit_price,
        input.closing,
    );
    let manage_actions: Vec<ManageAction> = input
        .positions
        .iter()
        .map(|p| manage(p, input.manage_cfg))
        .collect();

    let (skipped, scanned) = scan_cycle(
        input.windows,
        input.now_minute,
        input.risk,
        input.candidates,
        input.scan_gates,
        input.budget,
    );
    CycleReport {
        skipped,
        reconcile: reconcile_actions,
        manage: manage_actions,
        scanned,
    }
}

pub(crate) fn scan_cycle(
    windows: &[String],
    now_minute: i64,
    risk: &RiskInput,
    candidates: &[Candidate],
    gates: &ScanGates,
    budget: usize,
) -> (String, Vec<String>) {
    if !trading_window_open(windows, now_minute) {
        return ("不在交易时段".into(), Vec::new());
    }
    let gate: Gate = global_gates(risk);
    if !gate.ok {
        return (gate.reason, Vec::new());
    }
    let scanned = select(candidates, gates, budget)
        .into_iter()
        .map(|candidate| candidate.symbol.clone())
        .collect();
    (String::new(), scanned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk(equity: f64, halted: &str) -> RiskInput {
        RiskInput {
            equity,
            day_start_equity: equity,
            peak_equity: equity,
            min_equity: 50.0,
            daily_loss_limit_pct: 5.0,
            max_drawdown_pct: 15.0,
            consecutive_losses: 0,
            consecutive_loss_limit: 4,
            cooldown_until: 0,
            now: 0,
            halted: halted.to_string(),
        }
    }

    fn manage_cfg() -> ManageConfig {
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

    fn scan_gates() -> ScanGates {
        ScanGates {
            atr_pct_min: 0.3,
            atr_pct_max: 5.0,
            volume_ratio_min: 0.8,
            min_24h_quote_volume: 3e6,
            spread_bps_max: 8.0,
            entry_threshold: 0.5,
        }
    }

    fn base<'a>(
        enabled: bool,
        risk: &'a RiskInput,
        manage_cfg: &'a ManageConfig,
        scan_gates: &'a ScanGates,
        windows: &'a [String],
    ) -> CycleInput<'a> {
        CycleInput {
            enabled,
            mode: "live",
            now_minute: 12 * 60,
            windows,
            risk,
            venue: &[],
            local: &[],
            fee_rate: 0.0004,
            exit_price: 0.0,
            closing: &[],
            positions: &[],
            manage_cfg,
            candidates: &[],
            scan_gates,
            budget: 3,
        }
    }

    #[test]
    fn disabled_engine_does_nothing() {
        let (risk, manage_cfg, scan_gates, windows) =
            (risk(1000.0, ""), manage_cfg(), scan_gates(), Vec::new());
        let report = run_cycle(&base(false, &risk, &manage_cfg, &scan_gates, &windows));
        assert_eq!(report.skipped, "引擎未启用");
        assert!(
            report.reconcile.is_empty() && report.manage.is_empty() && report.scanned.is_empty()
        );
    }

    #[test]
    fn halted_gate_still_manages_open_positions_before_skipping_scan() {
        let (broke, manage_cfg, scan_gates, windows) = (
            risk(1000.0, "手动熔断"),
            manage_cfg(),
            scan_gates(),
            Vec::new(),
        );
        let mut input = base(true, &broke, &manage_cfg, &scan_gates, &windows);
        let pos = ManagedPosition {
            side: "long".to_string(),
            entry_price: 100.0,
            mark_price: 97.0,
            peak_price: 100.0,
            sl_price: 98.0,
            initial_sl: 98.0,
            tp_price: 106.0,
            qty: 1.0,
            partial_taken: false,
            score: 0.0,
            held_minutes: 0.0,
        };
        let candidates = vec![Candidate {
            symbol: "BTCUSDT".to_string(),
            data_ok: true,
            atr_pct: 1.0,
            vol_ratio: 1.0,
            quote_volume: 1e8,
            spread_bps: 2.0,
            strength: 0.9,
        }];
        input.positions = std::slice::from_ref(&pos);
        input.candidates = &candidates;

        let report = run_cycle(&input);
        assert!(report.skipped.contains("熔断"), "熔断必须拦住开新仓");
        assert!(report.scanned.is_empty(), "闸门不过不能扫描");
        assert_eq!(
            report.manage,
            vec![ManageAction::Close {
                reason: "stop_loss".to_string()
            }],
            "熔断也不能把该止损的仓留在场内"
        );
    }

    #[test]
    fn outside_trading_window_reconciles_but_does_not_scan() {
        let (risk, manage_cfg, scan_gates) = (risk(1000.0, ""), manage_cfg(), scan_gates());
        let windows = vec!["09:00-10:00".to_string()];
        let mut input = base(true, &risk, &manage_cfg, &scan_gates, &windows);
        input.now_minute = 15 * 60;
        let local = vec![LocalPosition {
            symbol: "ETHUSDT".to_string(),
            mode: "live".to_string(),
            side: "long".to_string(),
            qty: 1.0,
            entry_price: 100.0,
        }];
        let candidates = vec![Candidate {
            symbol: "BTCUSDT".to_string(),
            data_ok: true,
            atr_pct: 1.0,
            vol_ratio: 1.0,
            quote_volume: 1e8,
            spread_bps: 2.0,
            strength: 0.9,
        }];
        input.local = &local;
        input.exit_price = 110.0;
        input.candidates = &candidates;

        let report = run_cycle(&input);
        assert_eq!(report.skipped, "不在交易时段");
        assert!(report.scanned.is_empty());
        assert!(
            matches!(&report.reconcile[0], ReconcileAction::CloseLocal { pnl, .. } if *pnl > 0.0),
            "时段外也要把交易所已平的仓补记盈亏"
        );
    }
}
