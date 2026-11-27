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
    #[allow(dead_code)]
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

    #[test]
    fn no_unknown_reason_literal_is_left_in_the_source() {
        let source = include_str!("manage.rs");
        for line in source.lines() {
            let Some(start) = line.find("reason: \"") else {
                continue;
            };
            let rest = &line[start + 9..];
            let Some(end) = rest.find('"') else { continue };
            let value = &rest[..end];
            assert!(
                ExitReason::from_str(value).is_some(),
                "manage.rs 出现了枚举外的退出原因 {value}"
            );
        }
    }
}
