//! 扫描选标的。与 legacy 内核（已退役） `risk_gate_reason` / `candidate_strength` 一致。
//!
//! 风险闸门与方向无关：它只拦坏数据、波动率越界、没量和没深度的标的。
//! 方向判定是策略观点，不能拿它当「要不要送给 AI 看」的门槛，
//! 否则量化一给出 flat，AI 就永远不被调用。
//! 每轮送给 AI 的额度有限，候选按「离触发有多近」排序取前几个，
//! 而不是按扫描到的顺序，否则额度常年被最没机会的标的占掉。

/// 一个标的本轮的特征，只保留筛选要看的字段。
pub struct Candidate {
    pub symbol: String,
    pub data_ok: bool,
    pub atr_pct: f64,
    pub vol_ratio: f64,
    pub quote_volume: f64,
    /// 盘口点差（bp）。
    pub spread_bps: f64,
    /// 方向分的绝对值。均值回归模式下改传 |z|。
    pub strength: f64,
}

/// 筛选门槛。
pub struct ScanGates {
    pub atr_pct_min: f64,
    pub atr_pct_max: f64,
    pub volume_ratio_min: f64,
    pub min_24h_quote_volume: f64,
    /// 盘口点差上限（bp）。
    pub spread_bps_max: f64,
    /// 触发阈值。强度按它归一化：越接近 1 越值得优先看。
    pub entry_threshold: f64,
}

/// 风险闸门。返回拒绝原因，空串表示可交易。
pub fn risk_gate(c: &Candidate, g: &ScanGates) -> String {
    if !c.data_ok {
        return "行情数据不完整".to_string();
    }
    if c.atr_pct < g.atr_pct_min {
        return format!("波动率过低 {:.3}% < {}%", c.atr_pct, g.atr_pct_min);
    }
    if c.atr_pct > g.atr_pct_max {
        return format!("波动率过高 {:.3}% > {}%", c.atr_pct, g.atr_pct_max);
    }
    if c.vol_ratio < g.volume_ratio_min {
        return format!("量能不足 {:.2} < {}", c.vol_ratio, g.volume_ratio_min);
    }
    if c.quote_volume < g.min_24h_quote_volume {
        return "24h 成交额偏低".to_string();
    }
    if c.spread_bps > g.spread_bps_max {
        return format!(
            "盘口点差过大 {:.2}bp > {}bp",
            c.spread_bps, g.spread_bps_max
        );
    }
    String::new()
}

/// 过闸门的候选按强度降序取前 `budget` 个。强度 = 方向分 / 触发阈值。
pub fn select<'a>(candidates: &'a [Candidate], g: &ScanGates, budget: usize) -> Vec<&'a Candidate> {
    let mut passed: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| risk_gate(c, g).is_empty())
        .collect();
    let threshold = g.entry_threshold.max(0.05);
    passed.sort_by(|a, b| {
        (b.strength / threshold)
            .partial_cmp(&(a.strength / threshold))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    passed.truncate(budget);
    passed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gates() -> ScanGates {
        ScanGates {
            atr_pct_min: 0.3,
            atr_pct_max: 5.0,
            volume_ratio_min: 0.8,
            min_24h_quote_volume: 3e6,
            spread_bps_max: 8.0,
            entry_threshold: 0.5,
        }
    }

    fn candidate(symbol: &str, atr: f64, vol: f64, quote: f64, strength: f64) -> Candidate {
        Candidate {
            symbol: symbol.to_string(),
            data_ok: true,
            atr_pct: atr,
            vol_ratio: vol,
            quote_volume: quote,
            spread_bps: 2.0,
            strength,
        }
    }

    #[test]
    fn risk_gate_rejects_bad_data_and_out_of_range_volatility() {
        let mut dead = candidate("BTCUSDT", 1.0, 1.0, 1e8, 0.4);
        dead.data_ok = false;
        assert_eq!(risk_gate(&dead, &gates()), "行情数据不完整");

        let quiet = candidate("ETHUSDT", 0.1, 1.0, 1e8, 0.4);
        assert!(risk_gate(&quiet, &gates()).contains("波动率过低"));

        let wild = candidate("SOLUSDT", 9.0, 1.0, 1e8, 0.4);
        assert!(risk_gate(&wild, &gates()).contains("波动率过高"));

        let thin = candidate("XRPUSDT", 1.0, 0.2, 1e8, 0.4);
        assert!(risk_gate(&thin, &gates()).contains("量能不足"));

        let mut wide = candidate("ADAUSDT", 1.0, 1.0, 1e8, 0.4);
        wide.spread_bps = 9.0;
        assert!(risk_gate(&wide, &gates()).contains("盘口点差过大"));
    }

    #[test]
    fn budget_goes_to_the_closest_setups_not_scan_order() {
        let rows = vec![
            candidate("AAAUSDT", 1.0, 1.0, 1e8, 0.1),
            candidate("BBBUSDT", 1.0, 1.0, 1e8, 0.45),
            candidate("CCCUSDT", 0.05, 1.0, 1e8, 0.9), // 最强但波动率不达标，应被剔除
            candidate("DDDUSDT", 1.0, 1.0, 1e8, 0.3),
        ];
        let picked: Vec<&str> = select(&rows, &gates(), 2)
            .iter()
            .map(|c| c.symbol.as_str())
            .collect();
        assert_eq!(
            picked,
            ["BBBUSDT", "DDDUSDT"],
            "额度应给离触发最近的，而不是扫描顺序靠前的"
        );
    }

    #[test]
    fn thin_market_is_rejected_and_zero_budget_sends_nothing() {
        let illiquid = candidate("DOGEUSDT", 1.0, 1.0, 2e6, 0.9);
        assert_eq!(risk_gate(&illiquid, &gates()), "24h 成交额偏低");

        let rows = vec![candidate("BTCUSDT", 1.0, 1.0, 1e8, 0.9)];
        assert!(
            select(&rows, &gates(), 0).is_empty(),
            "额度为 0 表示本轮不送 AI"
        );
    }
}
