//! 自动换池的纯函数替换计划。
//!
//! 输入当前池、持仓集合、本轮扫描特征与闸门，输出「保留 / 删除 / 新增 / 持仓 / 候选」。
//! 本模块不访问网络与数据库，只做纯计算；执行层由调用方负责。

#![cfg_attr(not(test), allow(dead_code))]
// TODO(任务5): 接线后移除上面的 dead_code 豁免。

use std::collections::{HashMap, HashSet};

use crate::scan::{risk_gate, Candidate, ScanGates};

/// 一个标的的本轮扫描特征，只保留换池要看的字段。
#[derive(Debug, Clone)]
pub struct ScannedSymbol {
    pub symbol: String,
    pub data_ok: bool,
    pub vol_ratio: f64,
    pub atr_pct: f64,
    pub quote_volume: f64,
    pub spread_bps: f64,
}

/// 一轮换池的计划。
#[derive(Debug, Clone, Default)]
pub struct ScanPlan {
    pub keep: Vec<String>,
    pub delete: Vec<String>,
    pub add: Vec<String>,
    /// 本轮有持仓的标的。恒为 `keep` 的子集（`held ⊆ keep`），下游不要重复计数。
    pub held: Vec<String>,
    pub candidates: Vec<String>,
    /// 与 `delete` 一一对应的删除原因（symbol, risk_gate 原因）。
    pub delete_reasons: Vec<(String, String)>,
}

/// 把扫描特征转成闸门输入。强度用 `vol_ratio`，方向留空（换池只看能不能做）。
fn as_candidate(row: &ScannedSymbol) -> Candidate {
    Candidate {
        symbol: row.symbol.clone(),
        data_ok: row.data_ok,
        atr_pct: row.atr_pct,
        vol_ratio: row.vol_ratio,
        quote_volume: row.quote_volume,
        spread_bps: row.spread_bps,
        strength: row.vol_ratio,
        preview_side: None,
    }
}

/// 计算一轮换池计划。纯函数，不产生副作用。
pub fn plan(
    current: &[String],
    held: &HashSet<String>,
    scanned: &[ScannedSymbol],
    gates: &ScanGates,
    target: usize,
) -> ScanPlan {
    let by_symbol: HashMap<&str, &ScannedSymbol> =
        scanned.iter().map(|s| (s.symbol.as_str(), s)).collect();
    let current_set: HashSet<&str> = current.iter().map(|s| s.as_str()).collect();

    let mut plan = ScanPlan::default();
    let mut to_replace: Vec<(String, String)> = Vec::new();

    for symbol in current {
        if held.contains(symbol) {
            plan.keep.push(symbol.clone());
            plan.held.push(symbol.clone());
            continue;
        }
        match by_symbol.get(symbol.as_str()) {
            Some(row) => {
                let reason = risk_gate(&as_candidate(row), gates);
                if reason.is_empty() {
                    plan.keep.push(symbol.clone());
                } else {
                    to_replace.push((symbol.clone(), reason));
                }
            }
            None => to_replace.push((symbol.clone(), "缺少行情数据".to_string())),
        }
    }

    // 从 `by_symbol` 生成候选：同一 symbol 天然只出现一次，避免 `scanned` 重复导致重复入池。
    let mut candidates: Vec<&ScannedSymbol> = by_symbol
        .values()
        .copied()
        .filter(|s| !current_set.contains(s.symbol.as_str()))
        .filter(|s| risk_gate(&as_candidate(s), gates).is_empty())
        .collect();
    candidates.sort_by(|a, b| b.vol_ratio.total_cmp(&a.vol_ratio));
    plan.candidates = candidates.iter().map(|s| s.symbol.clone()).collect();

    let replace_count = to_replace.len().min(candidates.len());
    for (symbol, reason) in to_replace.iter().take(replace_count) {
        plan.delete.push(symbol.clone());
        plan.delete_reasons.push((symbol.clone(), reason.clone()));
    }
    plan.add
        .extend(candidates.iter().take(replace_count).map(|s| s.symbol.clone()));

    // 先按「合格 keep + 已替换数量」算空位，再把未被替换的标的并入 keep，
    // 否则剩余标的会先撑大 keep，导致 target 补足算少。
    let slots = target.saturating_sub(plan.keep.len() + replace_count);
    let fill = candidates.len().saturating_sub(replace_count).min(slots);
    plan.add.extend(
        candidates
            .iter()
            .skip(replace_count)
            .take(fill)
            .map(|s| s.symbol.clone()),
    );

    // 候选不足时未替换成功的不达标标的实际留在池里，必须计入 keep，避免下游误删。
    plan.keep.extend(
        to_replace
            .iter()
            .skip(replace_count)
            .map(|(symbol, _)| symbol.clone()),
    );

    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gates() -> ScanGates {
        ScanGates {
            atr_pct_min: 0.05,
            atr_pct_max: 4.0,
            volume_ratio_min: 0.8,
            min_24h_quote_volume: 3_000_000.0,
            spread_bps_max: 8.0,
            entry_threshold: 0.3,
        }
    }

    fn scanned(symbol: &str, vol: f64) -> ScannedSymbol {
        ScannedSymbol {
            symbol: symbol.to_string(),
            data_ok: true,
            vol_ratio: vol,
            atr_pct: 1.0,
            quote_volume: 1e8,
            spread_bps: 2.0,
        }
    }

    fn owned(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn all_current_pass_keeps_everything_without_replacements() {
        let current = owned(&["AAAUSDT", "BBBUSDT"]);
        let rows = vec![scanned("AAAUSDT", 1.5), scanned("BBBUSDT", 2.0)];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 2);

        assert_eq!(plan.keep, owned(&["AAAUSDT", "BBBUSDT"]));
        assert!(plan.delete.is_empty(), "全部通过不应删除任何标的");
        assert!(plan.add.is_empty(), "全部通过不应新增任何标的");
        assert!(plan.held.is_empty());
        assert!(plan.candidates.is_empty(), "原池标的不能算候选");
    }

    #[test]
    fn failing_current_symbols_are_replaced_by_top_candidates() {
        let current = owned(&["AAAUSDT", "BBBUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 1.5),
            scanned("BBBUSDT", 0.5),
            scanned("CCCUSDT", 1.0),
            scanned("DDDUSDT", 2.0),
        ];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 2);

        assert_eq!(plan.keep, owned(&["AAAUSDT"]));
        assert_eq!(plan.delete, owned(&["BBBUSDT"]));
        assert_eq!(plan.add, owned(&["DDDUSDT"]), "应补入量能最强的候选");
        assert_eq!(plan.candidates, owned(&["DDDUSDT", "CCCUSDT"]));
        assert!(plan.held.is_empty());
    }

    #[test]
    fn held_symbols_are_kept_even_when_they_fail_the_gate() {
        let current = owned(&["AAAUSDT", "BBBUSDT"]);
        let rows = vec![scanned("AAAUSDT", 1.5), scanned("BBBUSDT", 0.5)];
        let held: HashSet<String> = ["BBBUSDT".to_string()].into_iter().collect();
        let plan = plan(&current, &held, &rows, &gates(), 2);

        assert_eq!(plan.keep, owned(&["AAAUSDT", "BBBUSDT"]));
        assert_eq!(plan.held, owned(&["BBBUSDT"]));
        assert!(plan.delete.is_empty(), "持仓标的永不删除");
        assert!(plan.add.is_empty());
    }

    #[test]
    fn short_pool_is_filled_to_target_from_candidates() {
        let current = owned(&["AAAUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 1.5),
            scanned("BBBUSDT", 2.0),
            scanned("CCCUSDT", 1.0),
        ];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 3);

        assert_eq!(plan.keep, owned(&["AAAUSDT"]));
        assert!(plan.delete.is_empty());
        assert_eq!(plan.add, owned(&["BBBUSDT", "CCCUSDT"]));
        assert_eq!(plan.candidates, owned(&["BBBUSDT", "CCCUSDT"]));
    }

    #[test]
    fn insufficient_candidates_only_replace_what_is_available() {
        let current = owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 0.5),
            scanned("BBBUSDT", 0.5),
            scanned("CCCUSDT", 0.5),
            scanned("DDDUSDT", 1.0),
        ];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 3);

        assert_eq!(plan.keep, owned(&["BBBUSDT", "CCCUSDT"]));
        assert_eq!(plan.delete, owned(&["AAAUSDT"]));
        assert_eq!(plan.add, owned(&["DDDUSDT"]));
        assert_eq!(plan.candidates, owned(&["DDDUSDT"]));
        assert!(
            !plan.delete.contains(&"BBBUSDT".to_string())
                && !plan.delete.contains(&"CCCUSDT".to_string()),
            "候选不足时剩余不达标标的不应进入删除"
        );
    }

    #[test]
    fn oversized_passing_pool_is_not_trimmed() {
        let current = owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 1.0),
            scanned("BBBUSDT", 1.0),
            scanned("CCCUSDT", 1.0),
        ];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 2);

        assert_eq!(plan.keep, owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]));
        assert!(plan.delete.is_empty());
        assert!(plan.add.is_empty());
    }

    #[test]
    fn full_gate_applies_to_pool_and_candidates() {
        let current = owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]);
        let mut high_atr = scanned("AAAUSDT", 1.0);
        high_atr.atr_pct = 5.0;
        let mut thin = scanned("BBBUSDT", 1.0);
        thin.quote_volume = 2_000_000.0;
        let mut wide = scanned("CCCUSDT", 1.0);
        wide.spread_bps = 9.0;
        let mut bad_candidate = scanned("EEEUSDT", 5.0);
        bad_candidate.spread_bps = 9.0;
        let rows = vec![high_atr, thin, wide, scanned("DDDUSDT", 1.0), bad_candidate];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 3);

        assert_eq!(plan.keep, owned(&["BBBUSDT", "CCCUSDT"]));
        assert_eq!(plan.delete, owned(&["AAAUSDT"]));
        assert_eq!(plan.add, owned(&["DDDUSDT"]));
        assert_eq!(plan.candidates, owned(&["DDDUSDT"]), "点差过大的候选必须被剔除");
    }

    #[test]
    fn missing_scanned_symbol_is_deleted_with_reason() {
        let current = owned(&["AAAUSDT"]);
        let rows = vec![scanned("DDDUSDT", 1.0)];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 1);

        assert!(plan.keep.is_empty());
        assert_eq!(plan.delete, owned(&["AAAUSDT"]));
        assert_eq!(
            plan.delete_reasons,
            vec![("AAAUSDT".to_string(), "缺少行情数据".to_string())]
        );
        assert_eq!(plan.add, owned(&["DDDUSDT"]));
    }

    #[test]
    fn delete_reasons_capture_all_gate_failures() {
        let current = owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]);
        let mut high_atr = scanned("AAAUSDT", 1.0);
        high_atr.atr_pct = 5.0;
        let mut thin = scanned("BBBUSDT", 1.0);
        thin.quote_volume = 2_000_000.0;
        let mut wide = scanned("CCCUSDT", 1.0);
        wide.spread_bps = 9.0;
        let rows = vec![
            high_atr,
            thin,
            wide,
            scanned("DDDUSDT", 3.0),
            scanned("EEEUSDT", 2.0),
            scanned("FFFUSDT", 1.5),
        ];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 3);

        assert_eq!(plan.delete, owned(&["AAAUSDT", "BBBUSDT", "CCCUSDT"]));
        assert_eq!(plan.delete_reasons.len(), 3);
        assert!(
            plan.delete_reasons[0].1.contains("波动率过高"),
            "实际原因: {:?}",
            plan.delete_reasons[0].1
        );
        assert_eq!(plan.delete_reasons[1].1, "24h 成交额偏低");
        assert!(
            plan.delete_reasons[2].1.contains("盘口点差过大"),
            "实际原因: {:?}",
            plan.delete_reasons[2].1
        );
        assert_eq!(plan.add, owned(&["DDDUSDT", "EEEUSDT", "FFFUSDT"]));
    }

    #[test]
    fn duplicate_scanned_symbols_are_deduplicated() {
        let current = owned(&["AAAUSDT"]);
        let rows = vec![
            scanned("BBBUSDT", 1.0),
            scanned("BBBUSDT", 2.0),
            scanned("CCCUSDT", 3.0),
        ];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 3);

        assert_eq!(plan.candidates, owned(&["CCCUSDT", "BBBUSDT"]));
        assert_eq!(plan.add, owned(&["CCCUSDT", "BBBUSDT"]));
        let unique: HashSet<&String> = plan.candidates.iter().collect();
        assert_eq!(unique.len(), plan.candidates.len(), "候选不得重复");
    }

    #[test]
    fn candidates_are_sorted_by_volume_ratio_desc() {
        let current = owned(&["AAAUSDT"]);
        let rows = vec![
            scanned("AAAUSDT", 1.0),
            scanned("BBBUSDT", 1.2),
            scanned("CCCUSDT", 3.0),
            scanned("DDDUSDT", 2.0),
        ];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 5);

        assert_eq!(plan.keep, owned(&["AAAUSDT"]));
        assert_eq!(plan.candidates, owned(&["CCCUSDT", "DDDUSDT", "BBBUSDT"]));
        assert_eq!(plan.add, owned(&["CCCUSDT", "DDDUSDT", "BBBUSDT"]));
    }

    #[test]
    fn empty_input_and_zero_target_are_handled() {
        let first = plan(&[], &HashSet::new(), &[], &gates(), 0);
        assert!(first.keep.is_empty());
        assert!(first.delete.is_empty());
        assert!(first.add.is_empty());
        assert!(first.held.is_empty());
        assert!(first.candidates.is_empty());
        assert!(first.delete_reasons.is_empty());

        // target = 0 且全部通过时不裁减合格标的。
        let current = owned(&["AAAUSDT"]);
        let rows = vec![scanned("AAAUSDT", 1.0)];
        let plan = plan(&current, &HashSet::new(), &rows, &gates(), 0);
        assert_eq!(plan.keep, owned(&["AAAUSDT"]));
        assert!(plan.delete.is_empty());
        assert!(plan.add.is_empty());
    }
}