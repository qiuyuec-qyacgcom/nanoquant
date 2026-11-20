//! 回测撮合器：逐根推进，复用实盘信号、准入与部分止盈规则。
//!
//! 保真度边界与 legacy 内核（已退役） `backtest/engine.py` 一致：不含 AI 复核、不含盘口点差闸门，
//! 点差按成本计入；资金费按假设费率在每个 8h 结算点扣减；历史缺口只标注不插值；
//! 同一根 K 线内止损与部分止盈冲突时按止损优先。结果仅用于参数粗筛与逻辑回归。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use rand::prelude::IndexedRandom;
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::history::HistoryBar;
use crate::manage::{self, ManageConfig};
use crate::providers::{candle_metrics, CandleSnapshot};
use crate::trade_policy::{self, MarketFeatures, TradePolicy};

pub const WARMUP: usize = 60;
pub const DEFAULT_MAX_BARS_HOLD: usize = 0;
pub const FUNDING_INTERVAL_MS: i64 = 8 * 3_600 * 1_000;
pub const MAX_GRID_RUNS: usize = 48;
pub const MIN_GRID_TRADES: usize = 8;
pub const MAX_FOLDS: usize = 6;
pub const MIN_FOLDS: usize = 2;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub symbol: String,
    pub interval_ms: i64,
    pub fee_bps: f64,
    pub slippage_bps: f64,
    pub spread_bps: f64,
    pub funding_bps_8h: f64,
    pub initial_equity: f64,
    pub risk_per_trade_pct: f64,
    pub margin_pct: f64,
    pub leverage: f64,
    pub maintenance_margin_rate: f64,
    pub max_bars_hold: Option<usize>,
    pub policy: TradePolicy,
    pub manage: ManageConfig,
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestTrade {
    pub side: String,
    pub entry_ts: i64,
    pub exit_ts: i64,
    pub entry: f64,
    pub exit: f64,
    pub qty: f64,
    pub pnl: f64,
    pub pnl_pct: f64,
    pub bars: i64,
    pub reason: String,
    pub score: f64,
    pub sl_pct: f64,
    pub tp_pct: f64,
    pub entry_fee: f64,
    pub funding: f64,
    pub partial_tp: bool,
}

#[derive(Debug, Clone, Default)]
pub struct BacktestResult {
    pub trades: Vec<BacktestTrade>,
    pub equity_curve: Vec<f64>,
    pub summary: Value,
}

impl BacktestResult {
    pub fn number(&self, key: &str) -> f64 {
        self.summary
            .get(key)
            .and_then(Value::as_f64)
            .unwrap_or_default()
    }
    pub fn count(&self, key: &str) -> i64 {
        self.summary
            .get(key)
            .and_then(Value::as_i64)
            .unwrap_or_default()
    }
}

struct OpenPosition {
    side: String,
    entry_ts: i64,
    entry: f64,
    qty: f64,
    initial_qty: f64,
    sl_pct: f64,
    tp_pct: f64,
    stop_price: f64,
    tp_price: f64,
    initial_fee: f64,
    realized: f64,
    funding: f64,
    partial: bool,
    score: f64,
    entry_index: usize,
    peak: f64,
    liq_price: f64,
}

struct Sim<'a> {
    bars: &'a [HistoryBar],
    candles: Vec<CandleSnapshot>,
    config: &'a EngineConfig,
    cancel: &'a AtomicBool,
    equity: f64,
    peak_equity: f64,
    max_drawdown: f64,
    fee_total: f64,
    funding_total: f64,
    partial_count: i64,
    liquidations: i64,
    trades: Vec<BacktestTrade>,
    curve: Vec<f64>,
    position: Option<OpenPosition>,
}

impl<'a> Sim<'a> {
    fn new(bars: &'a [HistoryBar], config: &'a EngineConfig, cancel: &'a AtomicBool) -> Self {
        let step = config.interval_ms.max(1);
        let candles = bars
            .iter()
            .map(|bar| CandleSnapshot {
                close_time: bar.close_time.max(bar.open_time + step - 1),
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
            })
            .collect();
        Self {
            bars,
            candles,
            config,
            cancel,
            equity: config.initial_equity,
            peak_equity: config.initial_equity,
            max_drawdown: 0.0,
            fee_total: 0.0,
            funding_total: 0.0,
            partial_count: 0,
            liquidations: 0,
            trades: Vec::new(),
            curve: Vec::new(),
            position: None,
        }
    }

    fn slip_bps(&self) -> f64 {
        self.config.slippage_bps + self.config.spread_bps / 2.0
    }

    fn fee_rate(&self) -> f64 {
        self.config.fee_bps / 10_000.0
    }

    fn book(&mut self, amount: f64) {
        self.equity += amount;
        self.peak_equity = self.peak_equity.max(self.equity);
        if self.peak_equity > 0.0 {
            let drawdown = (self.peak_equity - self.equity) / self.peak_equity * 100.0;
            self.max_drawdown = self.max_drawdown.max(drawdown);
        }
    }

    fn close_position(&mut self, price: f64, index: usize, reason: &str) {
        let Some(position) = self.position.take() else {
            return;
        };
        let direction = direction_of(&position.side);
        let exit_fill = fill_price(price, self.slip_bps(), direction, false);
        let gross = (exit_fill - position.entry) * position.qty * direction;
        let exit_fee = exit_fill * position.qty * self.fee_rate();
        let entry_share =
            position.initial_fee * (position.qty / position.initial_qty.max(f64::EPSILON));
        let net = gross - exit_fee - entry_share;
        self.fee_total += exit_fee;
        self.book(net);
        self.trades.push(BacktestTrade {
            side: position.side,
            entry_ts: position.entry_ts,
            exit_ts: self.bars[index].open_time,
            entry: round(position.entry, 8),
            exit: round(exit_fill, 8),
            qty: round(position.initial_qty, 8),
            pnl: round(position.realized + net, 6),
            pnl_pct: round((exit_fill / position.entry - 1.0) * 100.0 * direction, 4),
            bars: (index as i64) - (position.entry_index as i64),
            reason: reason.to_string(),
            score: position.score,
            sl_pct: position.sl_pct,
            tp_pct: position.tp_pct,
            entry_fee: round(position.initial_fee, 6),
            funding: round(position.funding, 6),
            partial_tp: position.partial,
        });
        self.curve.push(round(self.equity, 6));
    }

    fn take_partial(&mut self, price: f64) {
        let slip = self.slip_bps();
        let fee_rate = self.fee_rate();
        let ratio = self.config.manage.partial_tp_ratio;
        let Some(position) = self.position.as_ref() else {
            return;
        };
        let direction = direction_of(&position.side);
        let leg_qty = position.qty * ratio;
        if leg_qty <= 0.0 {
            return;
        }
        let entry = position.entry;
        let initial_fee = position.initial_fee;
        let initial_qty = position.initial_qty.max(f64::EPSILON);
        let fill = fill_price(price, slip, direction, false);
        let gross = (fill - entry) * leg_qty * direction;
        let leg_fee = fill * leg_qty * fee_rate;
        let entry_share = initial_fee * (leg_qty / initial_qty);
        let net = gross - leg_fee - entry_share;
        self.fee_total += leg_fee;
        self.book(net);
        if let Some(position) = self.position.as_mut() {
            position.realized += net;
            position.partial = true;
            position.qty -= leg_qty;
            position.stop_price = position.entry;
        }
        self.partial_count += 1;
    }

    fn run(mut self) -> Result<BacktestResult, String> {
        if self.bars.len() <= WARMUP {
            return Ok(empty_result(format!("K 线不足 {} 根", WARMUP + 1)));
        }
        let bar_minutes = (self.config.interval_ms as f64 / 60_000.0).max(1.0);
        let at_r = self.config.manage.partial_tp_at_r;
        let ratio = self.config.manage.partial_tp_ratio;
        let funding_rate = self.config.funding_bps_8h / 10_000.0;
        let leverage = self.config.leverage.max(1.0);
        let mmr = self.config.maintenance_margin_rate.max(0.0);
        let hold_limit = self.config.max_bars_hold.unwrap_or(0);

        for index in WARMUP..self.bars.len() - 1 {
            if index % 256 == 0 && self.cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.to_string());
            }
            let bar = self.bars[index];
            if let Some(position) = self.position.as_ref() {
                let direction = direction_of(&position.side);
                let entry = position.entry;
                let qty = position.qty;
                let initial_sl_pct = position.sl_pct;
                let stop_price = position.stop_price;
                let tp_price = position.tp_price;
                let liq_price = position.liq_price;
                let held_bars = index - position.entry_index;
                let peak = position.peak;
                let partial = position.partial;

                let previous_ts = self.bars[index - 1].open_time;
                let crossings = funding_crossings(previous_ts, bar.open_time);
                if crossings > 0 && funding_rate != 0.0 && qty > 0.0 {
                    let notional = bar.open * qty;
                    let delta = -direction * funding_rate * notional * crossings as f64;
                    self.book(delta);
                    self.funding_total += delta;
                    if let Some(position) = self.position.as_mut() {
                        position.realized += delta;
                        position.funding += delta;
                    }
                }

                if liq_price > 0.0
                    && ((direction > 0.0 && bar.low <= liq_price)
                        || (direction < 0.0 && bar.high >= liq_price))
                {
                    self.close_position(
                        gap_fill(bar, liq_price, direction, true),
                        index,
                        "liquidation",
                    );
                    self.liquidations += 1;
                    continue;
                }

                if stop_price > 0.0
                    && ((direction > 0.0 && bar.low <= stop_price)
                        || (direction < 0.0 && bar.high >= stop_price))
                {
                    self.close_position(
                        gap_fill(bar, stop_price, direction, true),
                        index,
                        "stop_loss",
                    );
                    continue;
                }

                if tp_price > 0.0
                    && ((direction > 0.0 && bar.high >= tp_price)
                        || (direction < 0.0 && bar.low <= tp_price))
                {
                    self.close_position(
                        gap_fill(bar, tp_price, direction, false),
                        index,
                        "take_profit",
                    );
                    continue;
                }

                if at_r > 0.0 && ratio > 0.0 && !partial && initial_sl_pct > 0.0 {
                    let trigger = entry * (1.0 + direction * at_r * initial_sl_pct / 100.0);
                    let reached = (direction > 0.0 && bar.high >= trigger)
                        || (direction < 0.0 && bar.low <= trigger);
                    if reached {
                        self.take_partial(trigger);
                        continue;
                    }
                }

                if self.config.manage.trailing_pct > 0.0 {
                    let new_peak = if direction > 0.0 {
                        peak.max(bar.high)
                    } else if peak > 0.0 {
                        peak.min(bar.low)
                    } else {
                        bar.low
                    };
                    if let Some(position) = self.position.as_mut() {
                        position.peak = new_peak;
                    }
                    let risk_pct = initial_sl_pct;
                    let favor_pct = if entry > 0.0 {
                        (new_peak - entry) * direction / entry * 100.0
                    } else {
                        0.0
                    };
                    let armed = favor_pct >= risk_pct * self.config.manage.trailing_arm_r;
                    let pulled_back = if direction > 0.0 {
                        bar.close <= new_peak * (1.0 - self.config.manage.trailing_pct / 100.0)
                    } else {
                        bar.close >= new_peak * (1.0 + self.config.manage.trailing_pct / 100.0)
                    };
                    if armed && pulled_back {
                        self.close_position(bar.close, index, "trailing_stop");
                        continue;
                    }
                }

                if let Some(target) = manage::roi_target(
                    &self.config.manage.roi_table,
                    held_bars as f64 * bar_minutes,
                ) {
                    let profit_pct = if entry > 0.0 {
                        (bar.close - entry) * direction / entry * 100.0
                    } else {
                        0.0
                    };
                    if target > 0.0 && profit_pct >= target {
                        self.close_position(bar.close, index, "roi");
                        continue;
                    }
                }

                if hold_limit > 0 && held_bars >= hold_limit {
                    self.close_position(bar.close, index, "timeout");
                    continue;
                }
                continue;
            }

            if self.equity <= 0.0 {
                continue;
            }
            let slice_start = index.saturating_sub(180);
            let slice = &self.candles[slice_start..=index];
            let Some(metrics) = candle_metrics(slice, 14) else {
                continue;
            };
            let Some(signal) = trade_policy::evaluate(
                slice,
                bar.close,
                metrics.atr,
                self.config.policy,
                MarketFeatures::default(),
            ) else {
                continue;
            };
            let direction = direction_of(signal.side);
            let next = self.bars[index + 1];
            let entry = fill_price(next.open, self.slip_bps(), direction, true);
            let sl_pct = ((bar.close - signal.stop_price) / bar.close * 100.0).abs();
            let tp_pct = ((signal.take_profit - bar.close) / bar.close * 100.0).abs();
            if sl_pct <= 0.0 || entry <= 0.0 {
                continue;
            }
            let risk_amount = self.equity * self.config.risk_per_trade_pct / 100.0;
            let mut qty = risk_amount / (entry * sl_pct / 100.0);
            let max_notional = self.equity * (self.config.margin_pct / 100.0) * leverage;
            if qty * entry > max_notional {
                qty = max_notional / entry;
            }
            if qty <= 0.0 {
                continue;
            }
            let entry_fee = entry * qty * self.fee_rate();
            self.fee_total += entry_fee;
            self.position = Some(OpenPosition {
                side: signal.side.to_string(),
                entry_ts: next.open_time,
                entry: round(entry, 8),
                qty,
                initial_qty: qty,
                sl_pct,
                tp_pct,
                stop_price: entry * (1.0 - direction * sl_pct / 100.0),
                tp_price: entry * (1.0 + direction * tp_pct / 100.0),
                initial_fee: entry_fee,
                realized: 0.0,
                funding: 0.0,
                partial: false,
                score: signal.score,
                entry_index: index + 1,
                peak: next.open,
                liq_price: entry * (1.0 - direction * (1.0 / leverage - mmr)),
            });
        }

        if self.position.is_some() {
            let last = self.bars.len() - 1;
            let close = self.bars[last].close;
            self.close_position(close, last, "timeout");
        }

        self.summary(bar_minutes, leverage, mmr)
    }

    fn summary(&self, bar_minutes: f64, leverage: f64, mmr: f64) -> Result<BacktestResult, String> {
        let trades = &self.trades;
        let wins = trades.iter().filter(|trade| trade.pnl > 0.0).count();
        let losses = trades.iter().filter(|trade| trade.pnl <= 0.0).count();
        let gross_win: f64 = trades
            .iter()
            .filter(|trade| trade.pnl > 0.0)
            .map(|trade| trade.pnl)
            .sum();
        let gross_loss: f64 = trades
            .iter()
            .filter(|trade| trade.pnl <= 0.0)
            .map(|trade| trade.pnl.abs())
            .sum();
        let profit_factor = if gross_loss > 0.0 {
            round(gross_win / gross_loss, 3)
        } else if gross_win > 0.0 {
            round(gross_win, 3)
        } else {
            0.0
        };
        let avg_bars = if trades.is_empty() {
            0.0
        } else {
            round(
                trades.iter().map(|trade| trade.bars as f64).sum::<f64>() / trades.len() as f64,
                1,
            )
        };
        let mut summary = Map::new();
        summary.insert("trades".into(), json!(trades.len()));
        summary.insert("wins".into(), json!(wins));
        summary.insert("losses".into(), json!(losses));
        summary.insert(
            "win_rate".into(),
            json!(if trades.is_empty() {
                0.0
            } else {
                round(wins as f64 / trades.len() as f64 * 100.0, 2)
            }),
        );
        summary.insert("profit_factor".into(), json!(profit_factor));
        summary.insert(
            "avg_win".into(),
            json!(if wins > 0 {
                round(gross_win / wins as f64, 6)
            } else {
                0.0
            }),
        );
        summary.insert(
            "avg_loss".into(),
            json!(if losses > 0 {
                round(-gross_loss / losses as f64, 6)
            } else {
                0.0
            }),
        );
        summary.insert(
            "expectancy".into(),
            json!(if trades.is_empty() {
                0.0
            } else {
                round(
                    trades.iter().map(|trade| trade.pnl).sum::<f64>() / trades.len() as f64,
                    6,
                )
            }),
        );
        summary.insert("avg_bars".into(), json!(avg_bars));
        summary.insert(
            "total_return_pct".into(),
            json!(round(
                (self.equity / self.config.initial_equity - 1.0) * 100.0,
                3
            )),
        );
        summary.insert(
            "max_drawdown_pct".into(),
            json!(round(self.max_drawdown, 3)),
        );
        summary.insert("fee_total".into(), json!(round(self.fee_total, 6)));
        summary.insert("funding_total".into(), json!(round(self.funding_total, 6)));
        summary.insert("partial_tp_count".into(), json!(self.partial_count));
        summary.insert("liquidations".into(), json!(self.liquidations));
        summary.insert("leverage".into(), json!(round(leverage, 2)));
        summary.insert("maintenance_margin_rate".into(), json!(round(mmr, 5)));
        summary.insert("final_equity".into(), json!(round(self.equity, 6)));
        summary.insert(
            "disclaimer".into(),
            json!(
                "不含 AI 复核与点差闸门; 资金费为假设费率; 强平按维持保证金率估算、跳空按开盘价成交; 仅用于参数粗筛, 不代表实盘收益"
            ),
        );
        for (key, value) in segments(trades, bar_minutes) {
            summary.insert(key.to_string(), value);
        }
        Ok(BacktestResult {
            trades: self.trades.clone(),
            equity_curve: self.curve.clone(),
            summary: Value::Object(summary),
        })
    }
}

pub const CANCELLED: &str = "回测已取消";

pub fn run_with_cancel(
    bars: &[HistoryBar],
    config: &EngineConfig,
    cancel: &AtomicBool,
) -> Result<BacktestResult, String> {
    Sim::new(bars, config, cancel).run()
}

pub fn run(bars: &[HistoryBar], config: &EngineConfig) -> Result<BacktestResult, String> {
    run_with_cancel(bars, config, &AtomicBool::new(false))
}

pub fn funding_crossings(previous_ts: i64, ts: i64) -> i64 {
    if ts <= previous_ts {
        return 0;
    }
    ts / FUNDING_INTERVAL_MS - previous_ts / FUNDING_INTERVAL_MS
}

fn direction_of(side: &str) -> f64 {
    if side == "long" {
        1.0
    } else {
        -1.0
    }
}

fn fill_price(price: f64, bps: f64, direction: f64, opening: bool) -> f64 {
    let rate = bps / 10_000.0;
    let sign = if (opening && direction > 0.0) || (!opening && direction < 0.0) {
        1.0
    } else {
        -1.0
    };
    price * (1.0 + sign * rate)
}

fn gap_fill(bar: HistoryBar, trigger: f64, direction: f64, adverse: bool) -> f64 {
    let open = if bar.open > 0.0 { bar.open } else { trigger };
    let crossed = if direction > 0.0 {
        if adverse {
            open <= trigger
        } else {
            open >= trigger
        }
    } else if adverse {
        open >= trigger
    } else {
        open <= trigger
    };
    if crossed {
        open
    } else {
        trigger
    }
}

fn round(value: f64, digits: i32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let factor = 10_f64.powi(digits);
    (value * factor).round() / factor
}

fn empty_result(message: String) -> BacktestResult {
    BacktestResult {
        trades: Vec::new(),
        equity_curve: Vec::new(),
        summary: json!({"error": message}),
    }
}

fn bucket_stats(trades: &[&BacktestTrade]) -> Value {
    if trades.is_empty() {
        return json!({"trades":0,"wins":0,"win_rate":0.0,"pnl":0.0});
    }
    let wins = trades.iter().filter(|trade| trade.pnl > 0.0).count();
    json!({
        "trades": trades.len(),
        "wins": wins,
        "win_rate": round(wins as f64 / trades.len() as f64 * 100.0, 2),
        "pnl": round(trades.iter().map(|trade| trade.pnl).sum::<f64>(), 6),
    })
}

fn segments(trades: &[BacktestTrade], bar_minutes: f64) -> Vec<(&'static str, Value)> {
    let mut hours: BTreeMap<i64, Vec<&BacktestTrade>> = BTreeMap::new();
    let mut weekdays: BTreeMap<i64, Vec<&BacktestTrade>> = BTreeMap::new();
    let mut holds: BTreeMap<&'static str, Vec<&BacktestTrade>> = BTreeMap::new();
    let mut exits: BTreeMap<String, i64> = BTreeMap::new();
    for trade in trades {
        let hours_since_epoch = trade.entry_ts.div_euclid(3_600_000);
        hours
            .entry(hours_since_epoch.rem_euclid(24))
            .or_default()
            .push(trade);
        let days = trade.entry_ts.div_euclid(86_400_000);
        weekdays
            .entry((days + 3).rem_euclid(7))
            .or_default()
            .push(trade);
        let minutes = (trade.bars as f64 * bar_minutes).max(0.0);
        let bucket = if minutes < 15.0 {
            "<15m"
        } else if minutes < 60.0 {
            "15-60m"
        } else if minutes < 180.0 {
            "60-180m"
        } else {
            ">=180m"
        };
        holds.entry(bucket).or_default().push(trade);
        *exits.entry(trade.reason.clone()).or_insert(0) += 1;
    }
    let weekday_names = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    let by_hour: Vec<Value> = hours
        .iter()
        .map(|(hour, items)| {
            let mut row = bucket_stats(items);
            row["hour"] = json!(hour);
            row
        })
        .collect();
    let by_weekday: Vec<Value> = weekdays
        .iter()
        .map(|(day, items)| {
            let mut row = bucket_stats(items);
            row["weekday"] = json!(weekday_names[*day as usize]);
            row
        })
        .collect();
    let by_hold: Vec<Value> = ["<15m", "15-60m", "60-180m", ">=180m"]
        .iter()
        .filter_map(|key| {
            holds.get(key).map(|items| {
                let mut row = bucket_stats(items);
                row["bucket"] = json!(key);
                row
            })
        })
        .collect();
    let mut exit_rows: Vec<(String, i64)> = exits.into_iter().collect();
    exit_rows.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    let by_exit: Vec<Value> = exit_rows
        .into_iter()
        .map(|(reason, count)| json!({"reason": reason, "count": count}))
        .collect();
    vec![
        ("by_hour", json!(by_hour)),
        ("by_weekday", json!(by_weekday)),
        ("by_hold", json!(by_hold)),
        ("by_exit", json!(by_exit)),
    ]
}

pub fn grid_combinations(
    grid: &[(&'static str, Vec<f64>)],
) -> Result<Vec<Vec<(&'static str, f64)>>, String> {
    let active: Vec<(&'static str, Vec<f64>)> = grid
        .iter()
        .filter(|(_, values)| !values.is_empty())
        .map(|(key, values)| (*key, values.clone()))
        .collect();
    if active.is_empty() {
        return Err("至少需要给一个参数一组取值".to_string());
    }
    let total: usize = active.iter().map(|(_, values)| values.len()).product();
    if total > MAX_GRID_RUNS {
        return Err(format!("网格组合数 {total} 超过上限 {MAX_GRID_RUNS}"));
    }
    let mut combos: Vec<Vec<(&'static str, f64)>> = vec![Vec::new()];
    for (key, values) in &active {
        let mut next = Vec::with_capacity(combos.len() * values.len());
        for combo in &combos {
            for value in values {
                let mut extended = combo.clone();
                extended.push((*key, *value));
                next.push(extended);
            }
        }
        combos = next;
    }
    Ok(combos)
}

fn apply_params(config: &mut EngineConfig, params: &[(&'static str, f64)]) {
    for (key, value) in params {
        match *key {
            "entry_threshold" => config.policy.base_threshold = *value,
            "sl_atr_mult" => config.policy.sl_atr_mult = *value,
            "tp_atr_mult" => config.policy.tp_atr_mult = *value,
            "trailing_pct" => config.manage.trailing_pct = *value,
            _ => {}
        }
    }
}

fn params_value(params: &[(&'static str, f64)]) -> Value {
    let mut map = Map::new();
    for (key, value) in params {
        map.insert((*key).to_string(), json!(round(*value, 4)));
    }
    Value::Object(map)
}

pub fn run_grid(
    bars: &[HistoryBar],
    base: &EngineConfig,
    grid: &[(&'static str, Vec<f64>)],
    cancel: &AtomicBool,
) -> Result<Value, String> {
    let combos = grid_combinations(grid)?;
    let started = std::time::Instant::now();
    let mut items = Vec::with_capacity(combos.len());
    for combo in combos {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_string());
        }
        let mut config = base.clone();
        apply_params(&mut config, &combo);
        let result = run(bars, &config)?;
        items.push(json!({
            "params": params_value(&combo),
            "summary": {
                "trades": result.count("trades"),
                "win_rate": result.number("win_rate"),
                "profit_factor": result.number("profit_factor"),
                "expectancy": result.number("expectancy"),
                "total_return_pct": result.number("total_return_pct"),
                "max_drawdown_pct": result.number("max_drawdown_pct"),
                "avg_bars": result.number("avg_bars"),
            }
        }));
    }
    items.sort_by(|left, right| {
        let key = |value: &Value| {
            (
                value["summary"]["expectancy"].as_f64().unwrap_or_default(),
                value["summary"]["profit_factor"]
                    .as_f64()
                    .unwrap_or_default(),
            )
        };
        key(right)
            .partial_cmp(&key(left))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let best = items
        .iter()
        .find(|row| row["summary"]["trades"].as_i64().unwrap_or_default() >= MIN_GRID_TRADES as i64)
        .cloned();
    let grid_map: Map<String, Value> = grid
        .iter()
        .filter(|(_, values)| !values.is_empty())
        .map(|(key, values)| {
            (
                (*key).to_string(),
                json!(values
                    .iter()
                    .map(|value| round(*value, 4))
                    .collect::<Vec<_>>()),
            )
        })
        .collect();
    Ok(json!({
        "runs": items.len(),
        "truncated": false,
        "max_runs": MAX_GRID_RUNS,
        "min_trades": MIN_GRID_TRADES,
        "elapsed_ms": round(started.elapsed().as_secs_f64() * 1000.0, 1),
        "grid": grid_map,
        "best": best,
        "items": items,
        "disclaimer": format!(
            "样本内排序（期望 R，笔数≥{MIN_GRID_TRADES}）；过拟合风险自负；不含 AI 复核与盘口点差闸门"
        ),
    }))
}

pub fn run_walkforward(
    bars: &[HistoryBar],
    base: &EngineConfig,
    grid: &[(&'static str, Vec<f64>)],
    folds: i64,
    train_bars: i64,
    cancel: &AtomicBool,
) -> Result<Value, String> {
    let started = std::time::Instant::now();
    let requested_folds = folds.clamp(MIN_FOLDS as i64, MAX_FOLDS as i64) as usize;
    let mut folds = requested_folds;
    if bars.len() <= WARMUP * 2 {
        return Err(format!(
            "K 线不足 (至少 {} 根才能切样本内/外)",
            WARMUP * 2 + 1
        ));
    }
    let train_bars = (train_bars as usize).clamp(WARMUP * 2, bars.len().saturating_sub(WARMUP * 2));
    let mut step = (bars.len() - WARMUP - train_bars) / folds;
    while folds > MIN_FOLDS && step <= WARMUP {
        folds -= 1;
        step = (bars.len() - WARMUP - train_bars) / folds;
    }
    if step <= WARMUP {
        let need = WARMUP + train_bars + WARMUP + 1;
        return Err(format!(
            "K 线不足: 至少需要约 {need} 根 (当前 {})",
            bars.len()
        ));
    }

    let mut items = Vec::new();
    for index in 0..folds {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLED.to_string());
        }
        let train_end = WARMUP + train_bars + index * step;
        let test_end = (train_end + step).min(bars.len());
        let train_start = train_end.saturating_sub(train_bars);
        let train = &bars[train_start..train_end];
        let test = &bars[train_end.saturating_sub(WARMUP)..test_end];
        if train.len() <= WARMUP || test.len() <= WARMUP {
            continue;
        }
        let searched = run_grid(train, base, grid, cancel)?;
        let Some(best) = searched.get("best").filter(|value| !value.is_null()) else {
            continue;
        };
        let params: Vec<(&'static str, f64)> = [
            "entry_threshold",
            "sl_atr_mult",
            "tp_atr_mult",
            "trailing_pct",
        ]
        .iter()
        .filter_map(|key| {
            best["params"]
                .get(*key)
                .and_then(Value::as_f64)
                .map(|value| (*key, value))
        })
        .collect();
        let mut config = base.clone();
        apply_params(&mut config, &params);
        let out = run(test, &config)?;
        let mut stressed_config = config.clone();
        stressed_config.fee_bps *= 2.0;
        stressed_config.slippage_bps *= 2.0;
        let stressed = run(test, &stressed_config)?;
        let in_sample = best["summary"].clone();
        let out_sample = json!({
            "trades": out.count("trades"),
            "win_rate": out.number("win_rate"),
            "profit_factor": out.number("profit_factor"),
            "expectancy": out.number("expectancy"),
            "total_return_pct": out.number("total_return_pct"),
            "max_drawdown_pct": out.number("max_drawdown_pct"),
        });
        items.push(json!({
            "fold": index + 1,
            "train_from_ts": train[0].open_time,
            "train_to_ts": train[train.len()-1].open_time,
            "test_from_ts": test[0].open_time,
            "test_to_ts": test[test.len()-1].open_time,
            "train_bars": train.len(),
            "test_bars": test.len(),
            "params": best["params"],
            "in_sample": in_sample,
            "out_sample": out_sample,
            "out_sample_funding": out.number("funding_total"),
            "stressed_return_pct": stressed.number("total_return_pct"),
            "stressed_expectancy": stressed.number("expectancy"),
            "trade_returns_pct": out.trades.iter().map(|trade| trade.pnl_pct).collect::<Vec<_>>(),
        }));
    }
    if items.is_empty() {
        return Err("没有可用的折 (训练/测试窗口太短)".to_string());
    }

    let mut equity = base.initial_equity;
    for item in &items {
        let value = item["out_sample"]["total_return_pct"]
            .as_f64()
            .unwrap_or_default();
        equity *= 1.0 + value / 100.0;
    }
    let oos_returns: Vec<f64> = items
        .iter()
        .map(|item| {
            item["out_sample"]["total_return_pct"]
                .as_f64()
                .unwrap_or_default()
        })
        .collect();
    let is_returns: Vec<f64> = items
        .iter()
        .map(|item| {
            item["in_sample"]["total_return_pct"]
                .as_f64()
                .unwrap_or_default()
        })
        .collect();
    let positive = oos_returns.iter().filter(|value| **value > 0.0).count();
    let base_avg = average(&oos_returns);
    let is_avg = average(&is_returns);
    let stressed_returns: Vec<f64> = items
        .iter()
        .map(|item| item["stressed_return_pct"].as_f64().unwrap_or_default())
        .collect();
    let stressed_avg = average(&stressed_returns);
    let all_returns: Vec<f64> = items
        .iter()
        .flat_map(|item| {
            item["trade_returns_pct"]
                .as_array()
                .map(|rows| rows.iter().filter_map(Value::as_f64).collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    let bootstrap = bootstrap_ci(&all_returns);
    let oos_trades: i64 = items
        .iter()
        .map(|item| item["out_sample"]["trades"].as_i64().unwrap_or_default())
        .sum();

    Ok(json!({
        "folds": items.len(),
        "requested_folds": requested_folds,
        "train_bars": train_bars,
        "elapsed_ms": round(started.elapsed().as_secs_f64() * 1000.0, 1),
        "items": items,
        "aggregate": {
            "oos_return_pct": round((equity / base.initial_equity - 1.0) * 100.0, 3),
            "oos_avg_return_pct": round(base_avg, 3),
            "oos_worst_fold_pct": round(oos_returns.iter().copied().fold(f64::INFINITY, f64::min), 3),
            "positive_folds": positive,
            "consistency": round(positive as f64 / items_len(&oos_returns) * 100.0, 1),
            "in_sample_avg_pct": round(is_avg, 3),
            "out_sample_avg_pct": round(base_avg, 3),
            "overfit_gap_pct": round(is_avg - base_avg, 3),
            "oos_trades": oos_trades,
        },
        "bootstrap": bootstrap,
        "cost_sensitivity": {
            "base_avg_return_pct": round(base_avg, 3),
            "stressed_avg_return_pct": round(stressed_avg, 3),
            "delta_pct": round(stressed_avg - base_avg, 3),
            "flips_sign": (base_avg > 0.0) != (stressed_avg > 0.0),
        },
        "disclaimer": "每折参数在训练段选出、测试段冻结; 仍不含 AI 复核与点差闸门, 结论只用于判断参数稳健性",
    }))
}

fn items_len(values: &[f64]) -> f64 {
    values.len().max(1) as f64
}

fn average(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn bootstrap_ci(returns: &[f64]) -> Value {
    if returns.len() < 10 {
        return json!({"ci_low":0.0,"ci_high":0.0,"p_positive":0.0,"n_trades":returns.len()});
    }
    let mut rng = StdRng::seed_from_u64(42);
    let mut means = Vec::with_capacity(500);
    for _ in 0..500 {
        let sample: Vec<f64> = (0..returns.len())
            .map(|_| *returns.choose(&mut rng).unwrap_or(&0.0))
            .collect();
        means.push(sample.iter().sum::<f64>() / sample.len() as f64);
    }
    means.sort_by(f64::total_cmp);
    let low = means[(means.len() as f64 * 0.025) as usize];
    let high = means[(means.len() as f64 * 0.975) as usize];
    let positive = means.iter().filter(|value| **value > 0.0).count();
    json!({
        "ci_low": round(low, 3),
        "ci_high": round(high, 3),
        "p_positive": round(positive as f64 / means.len() as f64, 3),
        "n_trades": returns.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(open_time: i64, open: f64, high: f64, low: f64, close: f64) -> HistoryBar {
        HistoryBar {
            open_time,
            close_time: open_time + 899_999,
            open,
            high,
            low,
            close,
            volume: 10_000.0,
        }
    }

    fn config() -> EngineConfig {
        EngineConfig {
            symbol: "BTCUSDT".into(),
            interval_ms: 900_000,
            fee_bps: 5.0,
            slippage_bps: 2.0,
            spread_bps: 0.0,
            funding_bps_8h: 1.0,
            initial_equity: 1_000.0,
            risk_per_trade_pct: 1.0,
            margin_pct: 50.0,
            leverage: 20.0,
            maintenance_margin_rate: 0.005,
            max_bars_hold: None,
            policy: TradePolicy {
                mode: "trend",
                base_threshold: 0.05,
                volume_ratio_min: 0.0,
                rsi_long_min: 0.0,
                rsi_long_max: 100.0,
                rsi_short_min: 0.0,
                rsi_short_max: 100.0,
                adx_min: 0.0,

                mr_window: 20,
                mr_z_entry: 2.0,
                mr_min_risk_reward: 1.5,
                sl_atr_mult: 1.0,
                tp_atr_mult: 10.0,
                min_risk_reward: 0.5,
                weight_trend: 0.32,
                weight_momentum: 0.26,
                weight_volume: 0.18,
                weight_funding: 0.12,
                weight_oi: 0.12,
            },
            manage: ManageConfig {
                partial_tp_at_r: 1.0,
                partial_tp_ratio: 0.5,
                trailing_pct: 0.0,
                trailing_arm_r: 1.5,
                exit_threshold: 0.6,
                breakeven_at_r: 1.0,
                global_stop_loss_pct: 0.0,
                global_take_profit_pct: 0.0,
                qty_step: 0.001,
                min_qty: 0.001,
                roi_table: Vec::new(),
            },
        }
    }

    fn trending_bars(count: usize) -> Vec<HistoryBar> {
        (0..count)
            .map(|index| {
                let base = 100.0 + index as f64 * 0.5;
                bar(
                    index as i64 * 900_000,
                    base,
                    base + 1.0,
                    base - 1.0,
                    base + 0.4,
                )
            })
            .collect()
    }

    #[test]
    fn too_few_bars_returns_an_explicit_error_not_fake_equity() {
        let result = run(&trending_bars(30), &config()).unwrap();
        assert!(result.summary.get("error").is_some());
        assert!(result.trades.is_empty());
        assert!(result.equity_curve.is_empty());
    }

    #[test]
    fn funding_crossings_count_eight_hour_settlements() {
        assert_eq!(funding_crossings(0, FUNDING_INTERVAL_MS - 1), 0);
        assert_eq!(funding_crossings(0, FUNDING_INTERVAL_MS), 1);
        assert_eq!(
            funding_crossings(FUNDING_INTERVAL_MS - 1, 2 * FUNDING_INTERVAL_MS + 1),
            2
        );
        assert_eq!(funding_crossings(5, 5), 0);
    }

    #[test]
    fn slippage_moves_fills_in_the_adverse_direction() {
        assert!((fill_price(100.0, 10.0, 1.0, true) - 100.1).abs() < 1e-9);
        assert!((fill_price(100.0, 10.0, 1.0, false) - 99.9).abs() < 1e-9);
        assert!((fill_price(100.0, 10.0, -1.0, true) - 99.9).abs() < 1e-9);
        assert!((fill_price(100.0, 10.0, -1.0, false) - 100.1).abs() < 1e-9);
    }

    #[test]
    fn partial_take_profit_r_matches_the_live_manage_rule() {
        // 实盘 manage 在 mark=102、entry=100、initial_sl=98 时触发 1R 部分止盈。
        let position = crate::manage::ManagedPosition {
            side: "long".into(),
            entry_price: 100.0,
            mark_price: 102.0,
            peak_price: 102.0,
            sl_price: 98.0,
            initial_sl: 98.0,
            tp_price: 120.0,
            qty: 1.0,
            partial_taken: false,
            score: 0.0,
            held_minutes: 0.0,
        };
        let action = crate::manage::manage(
            &position,
            &ManageConfig {
                partial_tp_at_r: 1.0,
                partial_tp_ratio: 0.5,
                ..config().manage
            },
        );
        assert!(matches!(
            action,
            crate::manage::ManageAction::PartialTakeProfit { .. }
        ));
        let trigger: f64 = 100.0 * (1.0 + 1.0 * ((100.0 - 98.0) / 100.0 * 100.0) / 100.0);
        assert!((trigger - 102.0).abs() < 1e-9, "回测触发价应与实盘同口径");
    }

    #[test]
    fn partial_take_profit_is_recorded_and_moves_stop_to_breakeven() {
        let mut cfg = config();
        cfg.policy.tp_atr_mult = 20.0;
        let result = run(&trending_bars(400), &cfg).unwrap();
        assert!(
            result.count("partial_tp_count") >= 1,
            "R 达到 1.0 必须触发部分止盈"
        );
        assert!(
            result.trades.iter().any(|trade| trade.partial_tp),
            "成交明细要标记 partial_tp，前端才能区分半仓退出"
        );
    }

    #[test]
    fn a_known_trade_path_is_reproducible() {
        let bars = trending_bars(320);
        let first = run(&bars, &config()).unwrap();
        let second = run(&bars, &config()).unwrap();
        assert_eq!(first.summary, second.summary);
        assert_eq!(first.trades.len(), second.trades.len());
        assert_eq!(first.equity_curve, second.equity_curve);
    }

    #[test]
    fn gaps_in_input_are_not_interpolated() {
        let mut bars = trending_bars(200);
        bars[150].open_time += 900_000;
        bars[150].close_time += 900_000;
        for tail in bars.iter_mut().skip(151) {
            tail.open_time += 900_000;
            tail.close_time += 900_000;
        }
        let gaps = crate::history::detect_gaps(&bars, 900_000);
        assert_eq!(gaps.len(), 1);
        assert_eq!(bars.len(), 200, "回测不能为缺口补造 K 线");
    }

    #[test]
    fn grid_rejects_more_than_the_run_cap() {
        let values: Vec<f64> = (0..5).map(|index| 0.1 + index as f64 * 0.1).collect();
        let error = grid_combinations(&[
            ("entry_threshold", values.clone()),
            ("sl_atr_mult", values.clone()),
            ("tp_atr_mult", values),
        ])
        .unwrap_err();
        assert!(error.contains("超过上限"));
        assert_eq!(
            grid_combinations(&[("entry_threshold", vec![0.2, 0.3])])
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn grid_rows_are_comparable_and_share_the_same_metric_columns() {
        let mut cfg = config();
        cfg.manage.partial_tp_at_r = 0.0;
        cfg.policy.tp_atr_mult = 1.0;
        let result = run_grid(
            &trending_bars(400),
            &cfg,
            &[("entry_threshold", vec![0.05, 0.1])],
            &AtomicBool::new(false),
        )
        .unwrap();
        let items = result["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        for item in items {
            for key in [
                "trades",
                "win_rate",
                "profit_factor",
                "expectancy",
                "total_return_pct",
                "max_drawdown_pct",
                "avg_bars",
            ] {
                assert!(item["summary"].get(key).is_some(), "缺少指标列 {key}");
            }
        }
    }

    #[test]
    fn cancel_stops_the_run_and_returns_no_result() {
        let bars = trending_bars(1_000);
        let cancel = AtomicBool::new(true);
        let error = run_with_cancel(&bars, &config(), &cancel).unwrap_err();
        assert_eq!(error, CANCELLED);
    }

    #[test]
    fn walkforward_exposes_in_sample_out_sample_and_gap() {
        let bars = trending_bars(400);
        let mut cfg = config();
        cfg.manage.partial_tp_at_r = 0.0;
        cfg.policy.tp_atr_mult = 1.0;
        cfg.policy.sl_atr_mult = 0.5;
        let result = run_walkforward(
            &bars,
            &cfg,
            &[("entry_threshold", vec![0.05, 0.1])],
            2,
            120,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(result["aggregate"]["in_sample_avg_pct"].as_f64().is_some());
        assert!(result["aggregate"]["out_sample_avg_pct"].as_f64().is_some());
        assert!(result["aggregate"]["overfit_gap_pct"].as_f64().is_some());
        assert!(result["folds"].as_i64().unwrap() >= 2);
    }
}
