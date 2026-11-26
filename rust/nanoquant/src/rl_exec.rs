//! RL 执行层：只用轻量表格 Q-learning 决定「怎么下单」。
//!
//! 边界：只影响下单时机、限价偏移与拆单，绝不参与方向、杠杆、止损止盈
//! 与仓位大小。默认关闭；关闭时不读 Q 表，完全走原执行路径。
//!
//! 离线回放用历史 orders/trades 的盘口特征构造状态-动作-奖励样本，
//! 训练出的 Q 表持久化在 `engine_state` 的 `rl_exec.q_table`。

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Map, Value};

/// Q 表在 `engine_state` 表里的键。
pub(crate) const Q_TABLE_KEY: &str = "rl_exec.q_table";

const Q_TABLE_VERSION: u64 = 1;

/// 执行状态分桶。桶位越少越容易在历史数据上收敛，且便于人工解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RlState {
    /// 买一量/卖一量比：-2..2。
    pub imbalance_bucket: i8,
    /// 买卖价差 bps：0..3。
    pub spread_bucket: i8,
    /// 波动率（ATR%）：0..3。
    pub vol_bucket: i8,
    /// 订单名义额相对权益：0..3。
    pub size_bucket: i8,
}

impl RlState {
    pub(crate) fn new(
        imbalance_bucket: i8,
        spread_bucket: i8,
        vol_bucket: i8,
        size_bucket: i8,
    ) -> Self {
        Self {
            imbalance_bucket: imbalance_bucket.clamp(-2, 2),
            spread_bucket: spread_bucket.clamp(0, 3),
            vol_bucket: vol_bucket.clamp(0, 3),
            size_bucket: size_bucket.clamp(0, 3),
        }
    }

    /// 状态键：桶位固定顺序，改动顺序会让旧 Q 表失配。
    pub(crate) fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.imbalance_bucket, self.spread_bucket, self.vol_bucket, self.size_bucket
        )
    }
}

/// 执行动作。价格偏移单位 bps；等待单位毫秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RlAction {
    Market,
    Limit(i8),
    Wait(u32),
    Split(u8),
}

impl RlAction {
    pub(crate) fn key(self) -> String {
        match self {
            Self::Market => "market".to_string(),
            Self::Limit(offset) => format!("limit:{}", offset.clamp(0, 2)),
            Self::Wait(ms) => format!("wait:{}", ms),
            Self::Split(parts) => format!("split:{}", parts.clamp(2, 3)),
        }
    }
}

/// 候选动作集。表里没有的动作视为 Q=0，平局时保留 `Market`，
/// 所以空 Q 表的默认行为就是「不干预」。
pub(crate) const ACTIONS: [RlAction; 9] = [
    RlAction::Market,
    RlAction::Limit(0),
    RlAction::Limit(1),
    RlAction::Limit(2),
    RlAction::Wait(500),
    RlAction::Wait(1000),
    RlAction::Wait(2000),
    RlAction::Split(2),
    RlAction::Split(3),
];

/// 表格 Q-learning。键固定为 `(state_key, action_key)`，便于 JSON 持久化。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct QTable {
    q: HashMap<(String, String), f64>,
}

impl QTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn len(&self) -> usize {
        self.q.len()
    }

    pub(crate) fn get(&self, state: &RlState, action: RlAction) -> f64 {
        self.q
            .get(&(state.key(), action.key()))
            .copied()
            .unwrap_or(0.0)
    }

    pub(crate) fn set(&mut self, state: &RlState, action: RlAction, value: f64) {
        if value.is_finite() {
            self.q.insert((state.key(), action.key()), value);
        }
    }

    fn max_q(&self, state: &RlState) -> f64 {
        ACTIONS
            .iter()
            .map(|action| self.get(state, *action))
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// 贪心动作。空表或全平局时返回 `Market`，保证默认零干预。
    pub(crate) fn best_action(&self, state: &RlState) -> RlAction {
        let mut best = ACTIONS[0];
        let mut best_q = f64::NEG_INFINITY;
        for action in ACTIONS {
            let q = self.get(state, action);
            if q > best_q {
                best = action;
                best_q = q;
            }
        }
        best
    }

    /// epsilon-greedy 决策。epsilon 上限 0.5，避免实盘执行长期随机化。
    pub(crate) fn decide(&self, state: &RlState, epsilon: f64) -> RlAction {
        use rand::Rng;
        let roll: f64 = rand::rng().random();
        self.decide_with_roll(state, epsilon, roll)
    }

    /// 纯函数版本：roll 落在 [0,1)，便于测试探索分支。
    pub(crate) fn decide_with_roll(&self, state: &RlState, epsilon: f64, roll: f64) -> RlAction {
        let eps = sanitize(epsilon, 0.0, 0.5);
        let roll = sanitize(roll, 0.0, 1.0 - f64::EPSILON);
        if eps > 0.0 && roll < eps {
            let span = roll / eps;
            let index = (span * ACTIONS.len() as f64) as usize;
            return ACTIONS[index.min(ACTIONS.len() - 1)];
        }
        self.best_action(state)
    }

    /// 标准 Q-learning 更新：Q(s,a) += α·(r + γ·max Q(s',·) − Q(s,a))。
    pub(crate) fn learn(
        &mut self,
        state: &RlState,
        action: RlAction,
        reward: f64,
        next_state: &RlState,
        alpha: f64,
        gamma: f64,
    ) {
        if !reward.is_finite() {
            return;
        }
        let alpha = sanitize(alpha, 0.01, 1.0);
        let gamma = sanitize(gamma, 0.5, 0.99);
        let current = self.get(state, action);
        let target = reward + gamma * self.max_q(next_state);
        self.set(state, action, current + alpha * (target - current));
    }

    /// `{ "version": 1, "q": { state: { action: value } } }`。
    fn to_value(&self) -> Value {
        let mut states: Map<String, Value> = Map::new();
        for ((state, action), value) in &self.q {
            let entry = states
                .entry(state.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if let Some(object) = entry.as_object_mut() {
                object.insert(action.clone(), json!(value));
            }
        }
        json!({ "version": Q_TABLE_VERSION, "q": Value::Object(states) })
    }

    fn from_value(value: &Value) -> Self {
        let root = value.get("q").unwrap_or(value);
        let Some(states) = root.as_object() else {
            return Self::new();
        };
        let mut table = Self::new();
        for (state, actions) in states {
            let Some(actions) = actions.as_object() else {
                continue;
            };
            for (action, value) in actions {
                let Some(q) = value.as_f64().filter(|q| q.is_finite()) else {
                    continue;
                };
                table.q.insert((state.clone(), action.clone()), q);
            }
        }
        table
    }
}

fn sanitize(value: f64, min: f64, max: f64) -> f64 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        min
    }
}

/// 盘口不平衡：买一量/卖一量比。
pub(crate) fn imbalance_bucket(bid_qty: f64, ask_qty: f64) -> i8 {
    if !bid_qty.is_finite() || !ask_qty.is_finite() || bid_qty <= 0.0 || ask_qty <= 0.0 {
        return 0;
    }
    let ratio = bid_qty / ask_qty;
    if ratio >= 3.0 {
        2
    } else if ratio >= 1.5 {
        1
    } else if ratio > 1.0 / 1.5 {
        0
    } else if ratio > 1.0 / 3.0 {
        -1
    } else {
        -2
    }
}

/// 价差分桶：0=<2bps、1=2–5、2=5–10、3=>=10。
pub(crate) fn spread_bucket(spread_bps: f64) -> i8 {
    if !spread_bps.is_finite() || spread_bps < 0.0 {
        return 0;
    }
    if spread_bps < 2.0 {
        0
    } else if spread_bps < 5.0 {
        1
    } else if spread_bps < 10.0 {
        2
    } else {
        3
    }
}

/// 波动率分桶：0=<0.5%、1=0.5–1、2=1–2、3=>=2（ATR%）。
pub(crate) fn vol_bucket(atr_pct: f64) -> i8 {
    if !atr_pct.is_finite() || atr_pct < 0.0 {
        return 0;
    }
    if atr_pct < 0.5 {
        0
    } else if atr_pct < 1.0 {
        1
    } else if atr_pct < 2.0 {
        2
    } else {
        3
    }
}

/// 订单大小分桶：名义额/权益，0=<10%、1=10–25%、2=25–50%、3=>=50%。
pub(crate) fn size_bucket(notional: f64, equity: f64) -> i8 {
    if !notional.is_finite() || !equity.is_finite() || notional <= 0.0 || equity <= 0.0 {
        return 0;
    }
    let ratio = notional / equity;
    if ratio < 0.1 {
        0
    } else if ratio < 0.25 {
        1
    } else if ratio < 0.5 {
        2
    } else {
        3
    }
}

pub(crate) fn state_from_market(
    bid_qty: f64,
    ask_qty: f64,
    spread_bps: f64,
    atr_pct: f64,
    notional: f64,
    equity: f64,
) -> RlState {
    RlState::new(
        imbalance_bucket(bid_qty, ask_qty),
        spread_bucket(spread_bps),
        vol_bucket(atr_pct),
        size_bucket(notional, equity),
    )
}

/// `rl_exec` 配置组。缺键或越界一律回落到默认值，配置坏了不能阻断交易。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RlExecConfig {
    pub enabled: bool,
    pub epsilon: f64,
    pub max_wait_ms: u32,
    pub alpha: f64,
    pub gamma: f64,
}

impl Default for RlExecConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            epsilon: 0.05,
            max_wait_ms: 3000,
            alpha: 0.1,
            gamma: 0.9,
        }
    }
}

impl RlExecConfig {
    fn from_value(value: &Value) -> Self {
        let defaults = Self::default();
        let get_number = |key: &str, fallback: f64| {
            value
                .get(key)
                .and_then(Value::as_f64)
                .filter(|item| item.is_finite())
                .unwrap_or(fallback)
        };
        Self {
            enabled: value
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(defaults.enabled),
            epsilon: sanitize(get_number("epsilon", defaults.epsilon), 0.0, 0.5),
            max_wait_ms: sanitize(
                get_number("max_wait_ms", defaults.max_wait_ms as f64),
                0.0,
                10_000.0,
            ) as u32,
            alpha: sanitize(get_number("alpha", defaults.alpha), 0.01, 1.0),
            gamma: sanitize(get_number("gamma", defaults.gamma), 0.5, 0.99),
        }
    }
}

pub(crate) fn config_from_conn(conn: &Connection) -> RlExecConfig {
    conn.query_row(
        "SELECT value FROM app_settings WHERE key='rl_exec'",
        [],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .map(|value| RlExecConfig::from_value(&value))
    .unwrap_or_default()
}

/// 启动/每轮读取 Q 表。缺失或损坏时返回空表，等价于不干预。
pub(crate) fn load_q_table(conn: &Connection) -> QTable {
    let raw = conn
        .query_row(
            "SELECT value FROM engine_state WHERE key=?1",
            [Q_TABLE_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten();
    let Some(raw) = raw else {
        return QTable::new();
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(value) => QTable::from_value(&value),
        Err(error) => {
            tracing::warn!(%error, "rl_exec Q 表解析失败，按空表执行");
            QTable::new()
        }
    }
}

pub(crate) fn save_q_table(conn: &Connection, table: &QTable) -> Result<(), String> {
    let payload = serde_json::to_string(&table.to_value()).map_err(|error| error.to_string())?;
    conn.execute(
        "INSERT INTO engine_state(key,value,updated_at) VALUES(?1,?2,?3) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        (Q_TABLE_KEY, payload, now_millis()),
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 下单方式，与 runtime 的 `LiveOrderType` 对应；`Wait` 表示本轮不生成订单。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrderKind {
    Market,
    Limit,
    Wait,
}

/// 执行器真正能落地的下单方式。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResolvedExecution {
    pub kind: OrderKind,
    pub price: Option<f64>,
    pub action_key: String,
    /// 动作在当前执行器不可实现，已退化。
    pub degraded: bool,
    pub note: Option<String>,
    /// 拆单笔数；非拆单为 1。
    pub parts: u8,
}

/// 把 RL 动作落成真实下单参数。
///
/// - 关闭时原样返回默认执行方式（市场/限价与价格都不动）。
/// - `Market` 保持默认。
/// - `Limit(offset)` 在默认限价上按 bps 偏移：买单下移、卖单上移。
/// - `Wait(ms)` 在 `max_wait_ms` 内本轮不下单，候选留待下一轮；超上限才退化为 `Limit(0)`。
/// - `Split(n)` 返回拆单笔数，由调用方按 `n` 笔真实下单。
pub(crate) fn resolve_execution(
    enabled: bool,
    default_kind: OrderKind,
    base_limit_price: Option<f64>,
    action: RlAction,
    side: &str,
    max_wait_ms: u32,
) -> ResolvedExecution {
    let default_price = base_limit_price.filter(|price| price.is_finite() && *price > 0.0);
    let untouched = ResolvedExecution {
        kind: default_kind,
        price: match default_kind {
            OrderKind::Market | OrderKind::Wait => None,
            OrderKind::Limit => default_price,
        },
        action_key: "market".to_string(),
        degraded: false,
        note: None,
        parts: 1,
    };
    if !enabled {
        return untouched;
    }
    match action {
        RlAction::Market => ResolvedExecution {
            action_key: action.key(),
            ..untouched
        },
        RlAction::Limit(offset) => {
            let offset = offset.clamp(0, 2);
            let Some(base) = default_price else {
                return ResolvedExecution {
                    action_key: action.key(),
                    degraded: true,
                    note: Some("缺少可信限价，回退默认执行".to_string()),
                    ..untouched
                };
            };
            let ratio = offset as f64 / 10_000.0;
            let price = if side == "BUY" {
                base * (1.0 - ratio)
            } else {
                base * (1.0 + ratio)
            };
            ResolvedExecution {
                kind: OrderKind::Limit,
                price: Some(price),
                action_key: action.key(),
                degraded: false,
                note: None,
                parts: 1,
            }
        }
        RlAction::Wait(ms) => {
            if ms > 0 && ms <= max_wait_ms {
                return ResolvedExecution {
                    kind: OrderKind::Wait,
                    price: None,
                    action_key: "wait".to_string(),
                    degraded: false,
                    note: Some(format!("本轮等待 {ms}ms，候选留待下一轮重新评估")),
                    parts: 1,
                };
            }
            let Some(base) = default_price else {
                return ResolvedExecution {
                    action_key: action.key(),
                    degraded: true,
                    note: Some("缺少可信限价，回退默认执行".to_string()),
                    ..untouched
                };
            };
            ResolvedExecution {
                kind: OrderKind::Limit,
                price: Some(base),
                action_key: action.key(),
                degraded: true,
                note: Some(format!(
                    "等待 {ms}ms 超过上限 {max_wait_ms}ms，退化为限价挂单"
                )),
                parts: 1,
            }
        }
        RlAction::Split(parts) => {
            let parts = parts.clamp(2, 3);
            ResolvedExecution {
                action_key: action.key(),
                degraded: false,
                note: Some(format!("拆 {parts} 笔下单")),
                parts,
                ..untouched
            }
        }
    }
}

/// 拆单数量：每笔按 `floor` 归整，余量并入最后一笔。
///
/// `floor` 由调用方传入 `SymbolFilter::floor_qty`；单笔归整后为 0
/// 或余量非正时退化为单笔，避免生成无法下单的子单。
pub(crate) fn split_qtys<F>(qty: f64, parts: u8, floor: F) -> Vec<f64>
where
    F: Fn(f64) -> f64,
{
    let parts = parts.clamp(2, 3);
    if !qty.is_finite() || qty <= 0.0 {
        return Vec::new();
    }
    let per = floor(qty / parts as f64);
    if !per.is_finite() || per <= 0.0 {
        return vec![qty];
    }
    let last = qty - per * (parts - 1) as f64;
    if !last.is_finite() || last <= 0.0 {
        return vec![qty];
    }
    let mut qtys = vec![per; parts as usize];
    if let Some(slot) = qtys.last_mut() {
        *slot = last;
    }
    qtys
}

/// 滑点模型：市价吃半价差，限价挂 offset 换来更优价，拆单摊薄冲击。
///
/// 离线回放没有真实撮合，只能用历史盘口特征做反事实估计，
/// 因此结果只用于相对比较，不等价于实测滑点。
pub(crate) fn estimate_slippage_bps(action: RlAction, spread_bps: f64) -> f64 {
    let spread = spread_bps.max(0.0);
    match action {
        RlAction::Market => spread * 0.5,
        RlAction::Limit(offset) => (spread * 0.5 - offset.clamp(0, 2) as f64).max(-2.0),
        RlAction::Wait(_) => spread * 0.5,
        RlAction::Split(parts) => spread * 0.5 / parts.clamp(2, 3) as f64,
    }
}

/// 机会成本：挂单/等待可能错过行情，按价差的一部分计入奖励。
pub(crate) fn estimate_opportunity_bps(action: RlAction, spread_bps: f64) -> f64 {
    let spread = spread_bps.max(0.0);
    match action {
        RlAction::Market => 0.0,
        RlAction::Limit(_) => spread * 0.1,
        RlAction::Wait(_) => spread * 0.25,
        RlAction::Split(parts) => spread * 0.05 * (parts.clamp(2, 3) - 1) as f64,
    }
}

/// 手续费口径：市价吃单，限价/等待/拆单都按 maker 计。
pub(crate) fn estimate_fee_bps(action: RlAction, taker_bps: f64, maker_bps: f64) -> f64 {
    match action {
        RlAction::Market => taker_bps,
        _ => maker_bps,
    }
}

/// 离线回放结果。滑点均为 bps。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReplayStats {
    pub samples: usize,
    pub trained_pairs: usize,
    /// 历史订单的实际平均滑点（绝对值）。
    pub baseline_slippage_bps: f64,
    /// 训练后贪心动作按同一模型估算的平均滑点。
    pub policy_slippage_bps: f64,
    /// 回放策略的平均滑点，与 `policy_slippage_bps` 同口径。
    pub avg_slippage_bps: f64,
    /// 相对历史基线的改善百分比，正数代表变好。
    pub improvement_pct: f64,
    /// 样本不足，改善结果仅供参考。
    pub insufficient: bool,
}

const REPLAY_SQL_FULL: &str =
    "SELECT t.symbol, t.side, t.notional, t.fees, t.slippage_bps, t.features, \
     COALESCE((SELECT o.order_type FROM orders o WHERE o.symbol=t.symbol AND o.mode=t.mode \
       AND o.purpose='entry' AND o.ts<=t.opened_at ORDER BY o.ts DESC LIMIT 1), 'MARKET') \
     FROM (SELECT * FROM trades ORDER BY closed_at DESC LIMIT ?1) t ORDER BY t.closed_at ASC";

const REPLAY_SQL_BASIC: &str =
    "SELECT t.symbol, t.side, t.notional, t.fees, t.slippage_bps, NULL, \
     COALESCE((SELECT o.order_type FROM orders o WHERE o.symbol=t.symbol AND o.mode=t.mode \
       AND o.purpose='entry' AND o.ts<=t.opened_at ORDER BY o.ts DESC LIMIT 1), 'MARKET') \
     FROM (SELECT * FROM trades ORDER BY closed_at DESC LIMIT ?1) t ORDER BY t.closed_at ASC";

struct ReplaySample {
    state: RlState,
    action: RlAction,
    actual_slippage_bps: f64,
    spread_bps: f64,
    fee_bps: f64,
}

fn replay_samples(conn: &Connection, episodes: u32) -> Result<Vec<ReplaySample>, String> {
    let limit = if episodes == 0 {
        10_000
    } else {
        episodes as i64
    };

    let collect = |stmt: &mut rusqlite::Statement<'_>| -> Result<Vec<ReplaySample>, String> {
        let mapped = stmt
            .query_map([limit], |row| {
                let _symbol: String = row.get(0)?;
                let _side: String = row.get(1)?;
                let notional: f64 = row.get(2)?;
                let fees: f64 = row.get(3)?;
                let slippage_bps: f64 = row.get(4)?;
                let features: Option<String> = row.get(5)?;
                let order_type: String = row.get(6)?;
                Ok((notional, fees, slippage_bps, features, order_type))
            })
            .map_err(|error| error.to_string())?;
        let mut samples = Vec::new();
        for row in mapped {
            let (notional, fees, slippage_bps, features, order_type) =
                row.map_err(|error| error.to_string())?;
            let features: Value = features
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or_else(|| json!({}));
            let spread_bps = features
                .get("spread_bps")
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite())
                .unwrap_or(0.0)
                .max(0.0);
            let atr_pct = features
                .get("atr_pct")
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite())
                .unwrap_or(0.0);
            let state = state_from_market(0.0, 0.0, spread_bps, atr_pct, notional.abs(), 1_000.0);
            let action = if order_type.eq_ignore_ascii_case("LIMIT") {
                RlAction::Limit(0)
            } else {
                RlAction::Market
            };
            let actual = if slippage_bps.is_finite() {
                slippage_bps.abs()
            } else {
                0.0
            };
            let fee_bps = if notional.is_finite() && notional.abs() > 1e-9 {
                (fees.abs() / notional.abs() * 10_000.0).clamp(0.0, 100.0)
            } else {
                0.0
            };
            samples.push(ReplaySample {
                state,
                action,
                actual_slippage_bps: actual,
                spread_bps,
                fee_bps,
            });
        }
        Ok(samples)
    };
    match conn.prepare(REPLAY_SQL_FULL) {
        Ok(mut stmt) => collect(&mut stmt),
        Err(_) => {
            let mut stmt = conn
                .prepare(REPLAY_SQL_BASIC)
                .map_err(|error| error.to_string())?;
            collect(&mut stmt)
        }
    }
}

/// 历史 orders/trades 回放：构造样本、训练 Q 表、落库并报告滑点。
///
/// `episodes=0` 表示使用全部可用样本。奖励口径为
/// `-slippage_bps - fee_bps - opportunity_cost_bps`。
pub(crate) fn run_replay(
    conn: &Connection,
    episodes: u32,
    alpha: f64,
    gamma: f64,
) -> Result<ReplayStats, String> {
    let samples = replay_samples(conn, episodes)?;
    if samples.is_empty() {
        return Ok(ReplayStats {
            samples: 0,
            trained_pairs: 0,
            baseline_slippage_bps: 0.0,
            policy_slippage_bps: 0.0,
            avg_slippage_bps: 0.0,
            improvement_pct: 0.0,
            insufficient: true,
        });
    }
    let taker = 5.0;
    let maker = 2.0;
    let mut table = QTable::new();
    for (index, sample) in samples.iter().enumerate() {
        let next_state = samples
            .get(index + 1)
            .map(|next| next.state)
            .unwrap_or(sample.state);
        for action in ACTIONS {
            let reward = if action == sample.action {
                -(sample.actual_slippage_bps
                    + sample.fee_bps
                    + estimate_opportunity_bps(action, sample.spread_bps))
            } else {
                -(estimate_slippage_bps(action, sample.spread_bps)
                    + estimate_fee_bps(action, taker, maker)
                    + estimate_opportunity_bps(action, sample.spread_bps))
            };
            table.learn(&sample.state, action, reward, &next_state, alpha, gamma);
        }
    }
    let baseline = samples
        .iter()
        .map(|sample| sample.actual_slippage_bps)
        .sum::<f64>()
        / samples.len() as f64;
    let policy = samples
        .iter()
        .map(|sample| {
            let action = table.best_action(&sample.state);
            estimate_slippage_bps(action, sample.spread_bps)
        })
        .sum::<f64>()
        / samples.len() as f64;
    let improvement_pct = if baseline > 1e-9 {
        (baseline - policy) / baseline * 100.0
    } else {
        0.0
    };
    save_q_table(conn, &table)?;
    Ok(ReplayStats {
        samples: samples.len(),
        trained_pairs: table.len(),
        baseline_slippage_bps: baseline,
        policy_slippage_bps: policy,
        avg_slippage_bps: policy,
        improvement_pct,
        insufficient: samples.len() < 20,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buckets(imbalance: i8, spread: i8, vol: i8, size: i8) -> RlState {
        RlState::new(imbalance, spread, vol, size)
    }

    #[test]
    fn rl_state_buckets_are_stable() {
        assert_eq!(imbalance_bucket(3.0, 1.0), 2);
        assert_eq!(imbalance_bucket(1.5, 1.0), 1);
        assert_eq!(imbalance_bucket(1.0, 1.0), 0);
        assert_eq!(imbalance_bucket(0.5, 1.0), -1);
        assert_eq!(imbalance_bucket(0.1, 1.0), -2);
        assert_eq!(imbalance_bucket(0.0, 1.0), 0);
        assert_eq!(imbalance_bucket(f64::NAN, 1.0), 0);

        assert_eq!(spread_bucket(1.9), 0);
        assert_eq!(spread_bucket(2.0), 1);
        assert_eq!(spread_bucket(4.9), 1);
        assert_eq!(spread_bucket(5.0), 2);
        assert_eq!(spread_bucket(10.0), 3);
        assert_eq!(spread_bucket(-1.0), 0);

        assert_eq!(vol_bucket(0.4), 0);
        assert_eq!(vol_bucket(0.5), 1);
        assert_eq!(vol_bucket(1.5), 2);
        assert_eq!(vol_bucket(2.0), 3);
        assert_eq!(vol_bucket(f64::NAN), 0);

        assert_eq!(size_bucket(50.0, 1000.0), 0);
        assert_eq!(size_bucket(100.0, 1000.0), 1);
        assert_eq!(size_bucket(300.0, 1000.0), 2);
        assert_eq!(size_bucket(600.0, 1000.0), 3);
        assert_eq!(size_bucket(600.0, 0.0), 0);

        assert_eq!(buckets(9, -5, 7, 9).key(), "2:0:3:3");
    }

    #[test]
    fn rl_decide_explores_with_epsilon() {
        let table = QTable::new();
        let state = buckets(0, 1, 1, 1);
        assert_eq!(table.decide_with_roll(&state, 0.0, 0.99), RlAction::Market);
        assert_eq!(
            table.decide_with_roll(&state, 0.5, 0.499_999),
            RlAction::Split(3),
            "探索分支必须能走到非贪心动作"
        );
        assert_eq!(
            table.decide_with_roll(&state, 0.5, 0.9),
            RlAction::Market,
            "roll 超过 epsilon 时走贪心"
        );
        assert_eq!(
            table.decide_with_roll(&state, 9.0, 0.9),
            RlAction::Market,
            "epsilon 越界时夹到 0.5，roll 仍可走贪心"
        );
    }

    #[test]
    fn rl_decide_exploits_best_action() {
        let state = buckets(1, 0, 2, 1);
        let mut table = QTable::new();
        table.set(&state, RlAction::Limit(1), 5.0);
        table.set(&state, RlAction::Market, -1.0);
        assert_eq!(table.best_action(&state), RlAction::Limit(1));
        assert_eq!(table.decide_with_roll(&state, 0.0, 0.0), RlAction::Limit(1));

        let other = buckets(-2, 3, 3, 3);
        assert_eq!(
            table.best_action(&other),
            RlAction::Market,
            "未知状态必须回落到零干预"
        );
    }

    #[test]
    fn rl_learn_updates_q_value() {
        let state = buckets(0, 0, 0, 0);
        let next = buckets(1, 1, 1, 1);
        let mut table = QTable::new();
        table.learn(&state, RlAction::Market, -5.0, &next, 0.5, 0.9);
        let q = table.get(&state, RlAction::Market);
        assert!((q + 2.5).abs() < 1e-9, "首次更新应为 α·reward: {q}");

        table.learn(&state, RlAction::Market, -5.0, &next, 0.5, 0.9);
        let q2 = table.get(&state, RlAction::Market);
        assert!(q2 < q, "同一负奖励继续把 Q 往下压: {q2}");
        assert!(q2.is_finite());
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn rl_disabled_falls_back_to_default_execution() {
        let resolved = resolve_execution(
            false,
            OrderKind::Market,
            Some(100.0),
            RlAction::Split(3),
            "BUY",
            3000,
        );
        assert_eq!(resolved.kind, OrderKind::Market);
        assert_eq!(resolved.price, None);
        assert!(!resolved.degraded);

        let resolved = resolve_execution(
            false,
            OrderKind::Limit,
            Some(100.0),
            RlAction::Limit(2),
            "SELL",
            3000,
        );
        assert_eq!(resolved.kind, OrderKind::Limit);
        assert_eq!(resolved.price, Some(100.0), "关闭时不得偏移价格");
    }

    #[test]
    fn rl_enabled_actions_resolve_and_degrade() {
        let limit = resolve_execution(
            true,
            OrderKind::Market,
            Some(100.0),
            RlAction::Limit(2),
            "BUY",
            3000,
        );
        assert_eq!(limit.kind, OrderKind::Limit);
        assert!((limit.price.unwrap() - 99.98).abs() < 1e-9);

        let wait = resolve_execution(
            true,
            OrderKind::Market,
            Some(100.0),
            RlAction::Wait(5000),
            "SELL",
            3000,
        );
        assert_eq!(wait.kind, OrderKind::Limit);
        assert!((wait.price.unwrap() - 100.0).abs() < 1e-9);
        assert!(wait.degraded);
        assert!(wait.note.unwrap().contains("超过上限"));

        let split = resolve_execution(
            true,
            OrderKind::Market,
            Some(100.0),
            RlAction::Split(3),
            "BUY",
            3000,
        );
        assert_eq!(split.kind, OrderKind::Market);
        assert!(!split.degraded);
        assert_eq!(split.parts, 3);
    }

    #[test]
    fn split_action_creates_n_orders() {
        let floor = |value: f64| (value * 100.0).floor() / 100.0;
        let qtys = split_qtys(1.0, 3, floor);
        assert_eq!(qtys.len(), 3, "必须拆成 3 笔订单");
        assert!((qtys[0] - 0.33).abs() < 1e-9);
        assert!((qtys[1] - 0.33).abs() < 1e-9);
        assert!((qtys[2] - 0.34).abs() < 1e-9, "余量并入最后一笔");
        assert!(
            (qtys.iter().sum::<f64>() - 1.0).abs() < 1e-9,
            "总量必须等于原 qty"
        );

        let qtys = split_qtys(0.05, 2, floor);
        assert_eq!(qtys.len(), 2);
        assert!((qtys.iter().sum::<f64>() - 0.05).abs() < 1e-9);

        let qtys = split_qtys(0.005, 2, floor);
        assert_eq!(qtys.len(), 1, "单笔归整为 0 时退化为单笔");
        assert!((qtys[0] - 0.005).abs() < 1e-9);

        let qtys = split_qtys(0.0, 3, floor);
        assert!(qtys.is_empty(), "零数量不拆单");
    }

    #[test]
    fn wait_action_skips_order_and_audits() {
        let wait = resolve_execution(
            true,
            OrderKind::Market,
            Some(100.0),
            RlAction::Wait(500),
            "BUY",
            3000,
        );
        assert_eq!(wait.kind, OrderKind::Wait, "上限内必须真实等待，不生成订单");
        assert_eq!(wait.price, None);
        assert!(!wait.degraded);
        assert!(wait.note.unwrap().contains("下一轮"));

        let over = resolve_execution(
            true,
            OrderKind::Market,
            Some(100.0),
            RlAction::Wait(5000),
            "BUY",
            3000,
        );
        assert_eq!(over.kind, OrderKind::Limit, "超出等待上限退化为限价挂单");
        assert!(over.degraded);
    }

    #[test]
    fn rl_q_table_round_trips_through_json() {
        let state = buckets(1, -1, 2, 0);
        let mut table = QTable::new();
        table.set(&state, RlAction::Limit(1), -1.25);
        table.set(&state, RlAction::Wait(500), 0.5);
        let restored = QTable::from_value(&table.to_value());
        assert_eq!(restored, table);
    }

    #[test]
    fn rl_q_table_persists_to_engine_state() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);",
        )
        .unwrap();
        let state = buckets(0, 2, 1, 3);
        let mut table = QTable::new();
        table.set(&state, RlAction::Limit(2), 3.5);
        save_q_table(&conn, &table).unwrap();
        let loaded = load_q_table(&conn);
        assert_eq!(loaded, table);
        assert_eq!(loaded.get(&state, RlAction::Limit(2)), 3.5);
    }

    #[test]
    fn rl_replay_reports_slippage() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
             CREATE TABLE orders(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, order_type TEXT,
               purpose TEXT, mode TEXT);
             CREATE TABLE trades(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, notional REAL,
               fees REAL, slippage_bps REAL, features TEXT, opened_at INTEGER, closed_at INTEGER,
               mode TEXT);",
        )
        .unwrap();
        for index in 0..30 {
            let spread = if index % 2 == 0 { 2.0 } else { 8.0 };
            let opened = 1_000 + index;
            conn.execute(
                "INSERT INTO orders(ts,symbol,order_type,purpose,mode) VALUES(?1,'BTCUSDT',?2,'entry','demo')",
                (opened, if index % 3 == 0 { "LIMIT" } else { "MARKET" }),
            )
            .unwrap();
            conn.execute(
                "INSERT INTO trades(symbol,side,notional,fees,slippage_bps,features,opened_at,closed_at,mode) \
                 VALUES('BTCUSDT','long',1000.0,1.0,?1,?2,?3,?4,'demo')",
                (
                    if index % 3 == 0 { 3.0 } else { 6.0 },
                    json!({"spread_bps": spread, "atr_pct": 1.2}).to_string(),
                    opened,
                    opened + 10,
                ),
            )
            .unwrap();
        }
        let stats = run_replay(&conn, 100, 0.1, 0.9).unwrap();
        assert_eq!(stats.samples, 30);
        assert!(stats.trained_pairs > 0);
        assert!((stats.baseline_slippage_bps - 5.0).abs() < 1e-9);
        assert!(stats.avg_slippage_bps.is_finite());
        assert!(stats.improvement_pct.is_finite());
        assert!(!stats.insufficient);
        let stored = load_q_table(&conn);
        assert_eq!(stored.len(), stats.trained_pairs, "回放训练结果必须落库");
    }

    #[test]
    fn rl_replay_handles_missing_data() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE engine_state(key TEXT PRIMARY KEY, value TEXT, updated_at INTEGER);
             CREATE TABLE orders(id INTEGER PRIMARY KEY, ts INTEGER, symbol TEXT, order_type TEXT,
               purpose TEXT, mode TEXT);
             CREATE TABLE trades(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, notional REAL,
               fees REAL, slippage_bps REAL, features TEXT, opened_at INTEGER, closed_at INTEGER,
               mode TEXT);",
        )
        .unwrap();
        let stats = run_replay(&conn, 50, 0.1, 0.9).unwrap();
        assert_eq!(stats.samples, 0);
        assert_eq!(stats.improvement_pct, 0.0);
        assert!(stats.insufficient);
    }
}
