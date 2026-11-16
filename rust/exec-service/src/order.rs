//! P3-2：订单状态机（Planned → Sent → Acked → Filled），幂等键 `clientOrderId`。
//!
//! 阶段目标：把「订单生命周期」从 Python 的查询式收敛（`services/order_sync.py`）
//! 前移成 Rust 侧的**权威台账**，为 P3-3 保护单并行、P3-4 单写线程落库打基础。
//!
//! 三条硬约束：
//! 1. **不发单**：本模块只记账、只做状态迁移；真正下单仍由 Python 走现有链路。
//! 2. **幂等**：同一 `clientOrderId` 重复 register 不产生第二条记录，返回已存在的那条
//!    （`duplicate=true`），避免网络重试/双写把台账撑出重复订单。
//! 3. **非法迁移必须拒绝**：例如 `Filled → Planned`。宁可报错，也不能让台账
//!    出现「已成交又回到计划中」这种会让对账逻辑误判的状态。
//!
//! 终态集合与 `services/order_sync.py::TERMINAL` 保持一致，避免两套口径。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 订单状态。与 Python 侧状态字符串一一对应（大写）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderState {
    /// 已规划，尚未发出
    Planned,
    /// 已发送，等回执
    Sent,
    /// 交易所已受理（NEW）
    Acked,
    /// 部分成交
    PartiallyFilled,
    /// 完全成交（终态）
    Filled,
    /// 已撤销（终态）
    Canceled,
    /// 已过期（终态）
    Expired,
    /// 被拒（终态）
    Rejected,
}

impl OrderState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Planned => "PLANNED",
            Self::Sent => "SENT",
            Self::Acked => "ACKED",
            Self::PartiallyFilled => "PARTIALLY_FILLED",
            Self::Filled => "FILLED",
            Self::Canceled => "CANCELED",
            Self::Expired => "EXPIRED",
            Self::Rejected => "REJECTED",
        }
    }

    /// 终态：不再接受任何迁移。
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Filled | Self::Canceled | Self::Expired | Self::Rejected)
    }

    /// 从交易所/调用方给的字符串解析（容忍 CANCELLED 与 NEW 这类别名）。
    pub fn parse(raw: &str) -> Option<Self> {
        let v = raw.trim().to_ascii_uppercase();
        Some(match v.as_str() {
            "PLANNED" | "PLAN" => Self::Planned,
            "SENT" | "SUBMITTED" => Self::Sent,
            "ACKED" | "ACK" | "NEW" | "ACCEPTED" => Self::Acked,
            "PARTIALLY_FILLED" | "PARTIAL" | "PARTIALLYFILLED" => Self::PartiallyFilled,
            "FILLED" | "TRADE" => Self::Filled,
            "CANCELED" | "CANCELLED" | "CANCEL" => Self::Canceled,
            "EXPIRED" => Self::Expired,
            "REJECTED" | "REJECT" => Self::Rejected,
            _ => return None,
        })
    }
}

/// 允许的状态迁移。任何不在表里的迁移都会被拒绝。
///
/// 设计依据：
/// - 正常主链 `Planned → Sent → Acked → PartiallyFilled → Filled`；
/// - 未发出前可直接撤销：`Planned → Canceled`；
/// - 已受理后可退化：`Acked → Filled`（全成）、`Acked/PartiallyFilled → Canceled|Expired|Rejected`；
/// - 部分成交后补完：`PartiallyFilled → Filled`；
/// - 终态不可回退，也不允许跨级跳（`Planned → Filled` 无成交过程，属异常数据）。
fn allowed(from: OrderState, to: OrderState) -> bool {
    use OrderState::*;
    if from == to {
        return true; // 幂等重放
    }
    if from.is_terminal() {
        return false;
    }
    matches!(
        (from, to),
        (Planned, Sent)
            | (Planned, Canceled)
            | (Planned, Rejected)
            | (Sent, Acked)
            | (Sent, Canceled)
            | (Sent, Expired)
            | (Sent, Rejected)
            | (Acked, PartiallyFilled)
            | (Acked, Filled)
            | (Acked, Canceled)
            | (Acked, Expired)
            | (Acked, Rejected)
            | (PartiallyFilled, Filled)
            | (PartiallyFilled, Canceled)
            | (PartiallyFilled, Expired)
            | (PartiallyFilled, Rejected)
    )
}

/// 单笔订单台账记录。
#[derive(Debug, Clone, Serialize)]
pub struct OrderRecord {
    /// 幂等键（币安 `newClientOrderId`），全生命周期唯一
    pub client_order_id: String,
    pub symbol: String,
    pub state: OrderState,
    /// 累计成交量（部分成交会累加）
    pub filled_qty: f64,
    /// 成交均价（0 = 尚未成交）
    pub avg_price: f64,
    /// 迁移历史：`(状态, 时间戳秒)`，便于事后复盘
    pub history: Vec<(String, u64)>,
}

impl OrderRecord {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "client_order_id": self.client_order_id,
            "symbol": self.symbol,
            "state": self.state.as_str(),
            "filled_qty": self.filled_qty,
            "avg_price": self.avg_price,
            "terminal": self.state.is_terminal(),
            "history": self.history,
        })
    }
}

/// 迁移结果。`Err` 一律带可读原因，调用方按失败处理（**不重试**）。
#[derive(Debug)]
pub struct TransitionError(pub String);

#[derive(Debug, Default)]
pub struct OrderBook {
    orders: BTreeMap<String, OrderRecord>,
    /// 幂等命中次数（观测用）
    duplicate_hits: u64,
}

impl OrderBook {
    pub fn new() -> Self {
        Self::default()
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// 登记一笔订单（幂等）。同一 `client_order_id` 重复登记返回原有记录。
    pub fn register(
        &mut self,
        client_order_id: &str,
        symbol: &str,
        initial: OrderState,
    ) -> (OrderRecord, bool) {
        if let Some(existing) = self.orders.get(client_order_id) {
            self.duplicate_hits += 1;
            return (existing.clone(), true);
        }
        let record = OrderRecord {
            client_order_id: client_order_id.to_string(),
            symbol: symbol.to_string(),
            state: initial,
            filled_qty: 0.0,
            avg_price: 0.0,
            history: vec![(initial.as_str().to_string(), Self::now())],
        };
        self.orders.insert(client_order_id.to_string(), record.clone());
        (record, false)
    }

    /// 状态迁移。非法迁移 / 未知订单 / 终态回退都返回 `Err`。
    pub fn transition(
        &mut self,
        client_order_id: &str,
        to: OrderState,
        filled_qty: Option<f64>,
        avg_price: Option<f64>,
    ) -> Result<OrderRecord, TransitionError> {
        let Some(record) = self.orders.get_mut(client_order_id) else {
            return Err(TransitionError(format!("未知订单: {client_order_id}")));
        };
        let from = record.state;
        if !allowed(from, to) {
            return Err(TransitionError(format!(
                "非法迁移 {} → {}（{}）",
                from.as_str(),
                to.as_str(),
                if from.is_terminal() { "已是终态，不可回退" } else { "不在允许的迁移表内" }
            )));
        }
        if let Some(qty) = filled_qty {
            if qty < 0.0 {
                return Err(TransitionError("filled_qty 不能为负".into()));
            }
            // 累计成交量只增不减
            record.filled_qty = record.filled_qty.max(qty);
        }
        if let Some(price) = avg_price {
            if price < 0.0 {
                return Err(TransitionError("avg_price 不能为负".into()));
            }
            if price > 0.0 {
                record.avg_price = price;
            }
        }
        if from != to {
            record.state = to;
            record.history.push((to.as_str().to_string(), Self::now()));
        }
        Ok(record.clone())
    }

    pub fn get(&self, client_order_id: &str) -> Option<&OrderRecord> {
        self.orders.get(client_order_id)
    }

    /// 全部订单 + 统计（供面板/对账）。
    pub fn snapshot(&self) -> serde_json::Value {
        let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
        for rec in self.orders.values() {
            *counts.entry(rec.state.as_str()).or_insert(0) += 1;
        }
        serde_json::json!({
            "total": self.orders.len(),
            "duplicate_hits": self.duplicate_hits,
            "by_state": counts,
            "orders": self.orders.values().map(|r| r.to_json()).collect::<Vec<_>>(),
        })
    }

    /// 清空（仅测试与人工复位用）。
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.orders.clear();
        self.duplicate_hits = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_is_idempotent() {
        let mut book = OrderBook::new();
        let (a, dup_a) = book.register("nq123", "BTCUSDT", OrderState::Planned);
        let (b, dup_b) = book.register("nq123", "BTCUSDT", OrderState::Planned);
        assert!(!dup_a);
        assert!(dup_b, "同一 clientOrderId 第二次登记必须命中去重");
        assert_eq!(a.client_order_id, b.client_order_id);
        assert_eq!(book.snapshot()["total"], 1);
        assert_eq!(book.snapshot()["duplicate_hits"], 1);
    }

    #[test]
    fn happy_path_reaches_filled() {
        let mut book = OrderBook::new();
        book.register("c1", "ETHUSDT", OrderState::Planned);
        for st in [OrderState::Sent, OrderState::Acked, OrderState::PartiallyFilled] {
            book.transition("c1", st, None, None).unwrap();
        }
        let rec = book
            .transition("c1", OrderState::Filled, Some(0.5), Some(2500.0))
            .unwrap();
        assert_eq!(rec.state, OrderState::Filled);
        assert_eq!(rec.filled_qty, 0.5);
        assert_eq!(rec.avg_price, 2500.0);
        assert!(rec.state.is_terminal());
        assert_eq!(rec.history.len(), 5);
    }

    #[test]
    fn terminal_state_cannot_go_back() {
        let mut book = OrderBook::new();
        book.register("c2", "BTCUSDT", OrderState::Planned);
        book.transition("c2", OrderState::Sent, None, None).unwrap();
        book.transition("c2", OrderState::Acked, None, None).unwrap();
        book.transition("c2", OrderState::Filled, Some(1.0), None).unwrap();
        let err = book.transition("c2", OrderState::Planned, None, None).unwrap_err();
        assert!(err.0.contains("终态"), "终态回退必须被拒: {}", err.0);
    }

    #[test]
    fn cross_level_jump_is_rejected() {
        let mut book = OrderBook::new();
        book.register("c3", "BTCUSDT", OrderState::Planned);
        let err = book.transition("c3", OrderState::Filled, None, None).unwrap_err();
        assert!(err.0.contains("非法迁移"), "Planned 直接 Filled 必须被拒");
    }

    #[test]
    fn unknown_order_is_rejected() {
        let mut book = OrderBook::new();
        let err = book.transition("nope", OrderState::Sent, None, None).unwrap_err();
        assert!(err.0.contains("未知订单"));
    }

    #[test]
    fn filled_qty_never_decreases() {
        let mut book = OrderBook::new();
        book.register("c4", "BTCUSDT", OrderState::Planned);
        book.transition("c4", OrderState::Sent, None, None).unwrap();
        book.transition("c4", OrderState::Acked, None, None).unwrap();
        book.transition("c4", OrderState::PartiallyFilled, Some(0.8), Some(100.0))
            .unwrap();
        let rec = book
            .transition("c4", OrderState::PartiallyFilled, Some(0.3), None)
            .unwrap();
        assert_eq!(rec.filled_qty, 0.8, "累计成交量只增不减");
    }

    #[test]
    fn parse_accepts_exchange_aliases() {
        assert_eq!(OrderState::parse("NEW"), Some(OrderState::Acked));
        assert_eq!(OrderState::parse("cancelled"), Some(OrderState::Canceled));
        assert_eq!(OrderState::parse("PARTIALLY_FILLED"), Some(OrderState::PartiallyFilled));
        assert_eq!(OrderState::parse("bogus"), None);
    }
}
