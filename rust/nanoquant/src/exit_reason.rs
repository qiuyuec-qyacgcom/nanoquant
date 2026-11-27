//! 退出原因枚举：管仓、快平、回测、风控平仓共用同一套取值。
//!
//! 这些字符串会落进 `trades.exit_reason` 与 `orders.purpose`，是历史统计的口径，
//! **只增不改**：改名字会让旧交易的归因对不上。

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    StopLoss,
    GlobalStopLoss,
    GlobalTakeProfit,
    TakeProfit,
    RoiTarget,
    TrailingStop,
    SignalFlip,
    Invalidation,
    Liquidation,
    Timeout,
    RiskHalt,
    /// 部分止盈不是整仓退出，但审计需要同一套取值。
    PartialTakeProfit,
    /// 人工平仓由外部调用方触发，保留枚举值供解析。
    #[allow(dead_code)]
    Manual,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::StopLoss => "stop_loss",
            ExitReason::GlobalStopLoss => "global_stop_loss",
            ExitReason::GlobalTakeProfit => "global_take_profit",
            ExitReason::TakeProfit => "take_profit",
            ExitReason::RoiTarget => "roi_target",
            ExitReason::TrailingStop => "trailing_stop",
            ExitReason::SignalFlip => "signal_flip",
            ExitReason::Invalidation => "invalidation",
            ExitReason::Liquidation => "liquidation",
            ExitReason::Timeout => "timeout",
            ExitReason::RiskHalt => "halt_flatten",
            ExitReason::PartialTakeProfit => "partial_take_profit",
            ExitReason::Manual => "manual",
        }
    }

    /// 解析历史退出原因；未知值返回 None。
    #[allow(dead_code)] // 供审计侧解析历史数据与漂移测试使用
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "stop_loss" => ExitReason::StopLoss,
            "global_stop_loss" => ExitReason::GlobalStopLoss,
            "global_take_profit" => ExitReason::GlobalTakeProfit,
            "take_profit" => ExitReason::TakeProfit,
            "roi_target" => ExitReason::RoiTarget,
            "trailing_stop" => ExitReason::TrailingStop,
            "signal_flip" => ExitReason::SignalFlip,
            "invalidation" => ExitReason::Invalidation,
            "liquidation" => ExitReason::Liquidation,
            "timeout" => ExitReason::Timeout,
            "halt_flatten" => ExitReason::RiskHalt,
            "partial_take_profit" => ExitReason::PartialTakeProfit,
            "manual" => ExitReason::Manual,
            _ => return None,
        })
    }

    /// 全部取值：漂移守卫测试据此检查字符串集合。
    #[allow(dead_code)] // 目前仅漂移测试使用
    pub fn all() -> &'static [ExitReason] {
        &[
            ExitReason::StopLoss,
            ExitReason::GlobalStopLoss,
            ExitReason::GlobalTakeProfit,
            ExitReason::TakeProfit,
            ExitReason::RoiTarget,
            ExitReason::TrailingStop,
            ExitReason::SignalFlip,
            ExitReason::Invalidation,
            ExitReason::Liquidation,
            ExitReason::Timeout,
            ExitReason::RiskHalt,
            ExitReason::PartialTakeProfit,
            ExitReason::Manual,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reason_round_trips_through_its_string() {
        for reason in ExitReason::all() {
            assert_eq!(ExitReason::from_str(reason.as_str()), Some(*reason));
        }
    }

    #[test]
    fn the_string_set_is_frozen() {
        // 退出原因是历史数据的一部分：新增取值要显式改这里，否则旧统计口径会漂移。
        let mut names: Vec<&str> = ExitReason::all().iter().map(|r| r.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "global_stop_loss",
                "global_take_profit",
                "halt_flatten",
                "invalidation",
                "liquidation",
                "manual",
                "partial_take_profit",
                "roi_target",
                "signal_flip",
                "stop_loss",
                "take_profit",
                "timeout",
                "trailing_stop",
            ]
        )
    }

    /// 会落库到 `trades.exit_reason` / `orders.purpose` 的生产模块。
    const EXIT_REASON_SOURCES: &[(&str, &str)] = &[
        ("manage.rs", include_str!("manage.rs")),
        ("execution.rs", include_str!("execution.rs")),
        ("runtime.rs", include_str!("runtime.rs")),
        ("live.rs", include_str!("live.rs")),
        ("cycle.rs", include_str!("cycle.rs")),
        ("fast_risk.rs", include_str!("fast_risk.rs")),
        ("backtest_engine.rs", include_str!("backtest_engine.rs")),
    ];

    /// `purpose` 位置允许的非退出原因订单用途。
    const NON_EXIT_PURPOSES: &[&str] = &["entry", "close", "partial-tp", "protection"];

    #[test]
    fn no_unknown_reason_literal_is_left_in_the_production_path() {
        // 只扫生产代码：测试里用字面量断言是允许的。
        // 覆盖范围从 manage.rs 扩到所有会写退出原因/平仓用途的模块，
        // 防止 `let purpose = "take_profit"` 这类裸字面量绕过枚举。
        for (name, source) in EXIT_REASON_SOURCES {
            let production = source
                .find("\nmod tests {")
                .map_or(*source, |index| &source[..index]);
            for (index, line) in production.lines().enumerate() {
                let line_no = index + 1;
                for needle in ["reason: \"", "purpose: \""] {
                    let Some(start) = line.find(needle) else {
                        continue;
                    };
                    let rest = &line[start + needle.len()..];
                    let Some(end) = rest.find('"') else { continue };
                    let value = &rest[..end];
                    if needle.starts_with("purpose") && NON_EXIT_PURPOSES.contains(&value) {
                        continue;
                    }
                    assert!(
                        ExitReason::from_str(value).is_some(),
                        "{name}:{line_no} 出现了枚举外的退出原因 {value}"
                    );
                }
            }
        }
    }
}
