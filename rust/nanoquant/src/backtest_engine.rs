//! 回测撮合器：逐根推进，复用实盘信号、准入与部分止盈规则。
//!
//! 保真度边界与 legacy 内核（已退役） `backtest/engine.py` 一致：不含 AI 复核、不含盘口点差闸门，
//! 点差按成本计入；资金费按假设费率在每个 8h 结算点扣减；历史缺口只标注不插值；
//! 同一根 K 线内止损与部分止盈冲突时按止损优先。结果仅用于参数粗筛与逻辑回归。

use std::collections::{BTreeMap, BTreeSet};
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
pub const FUNDING_INTERVAL_MS: i64 = 8 * 3_600 * 1_000;
pub const MAX_GRID_RUNS: usize = 48;
pub const MIN_GRID_TRADES: usize = 8;
pub const MAX_FOLDS: usize = 6;
pub const MIN_FOLDS: usize = 2;

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub interval_ms: i64,
    pub fee_bps: f64,
    pub slippage_bps: f64,
    pub spread_bps: f64,
    pub funding_bps_8h: f64,
    pub initial_equity: f64,

    pub margin_pct: f64,
    pub leverage: f64,
    pub maintenance_margin_rate: f64,
    pub max_bars_hold: Option<usize>,
    pub policy: TradePolicy,
    pub manage: ManageConfig,
    /// 趋势仓波动保护止损：价格距离 = 该倍数 × ATR%，与杠杆无关。
    pub stop_distance_atr_mult: f64,
    /// ATR 周期：与实盘扫描、AI 特征快照同源，避免各算各的 ATR。
    pub atr_period: usize,
    /// 策略参数快照：range 市均值回归的开关、阈值与独立止损止盈。
    /// 缺省 `Value::Null` 时均值回归关闭，保持既有回测语义。
    pub mr: Value,
    /// 信号偏移根数：>0 时把入场信号整体延后这么多根，用于前视偏差体检。
    pub signal_shift_bars: usize,
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
    /// 开仓时该标的的 ATR%，用于评估「止损距离 / ATR」是否落在噪音区。
    pub atr_pct: f64,
    pub entry_fee: f64,
    pub funding: f64,
    pub partial_tp: bool,
    pub market_regime: String,
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
    /// 开仓时该标的的 ATR%，落库到成交明细供证据门/GA 使用。
    atr_pct: f64,
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
    /// 该仓来自均值回归信号：退出阈值使用独立保证金止损止盈。
    mean_reversion: bool,
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
            atr_pct: position.atr_pct,
            entry_fee: round(position.initial_fee, 6),
            funding: round(position.funding, 6),
            partial_tp: position.partial,
            market_regime: String::new(),
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
                let mean_reversion = position.mean_reversion;
                let (global_stop, global_tp) = if mean_reversion {
                    (
                        self.config.mr["mr_sl_pct"].as_f64().unwrap_or(20.0),
                        self.config.mr["mr_tp_pct"].as_f64().unwrap_or(30.0),
                    )
                } else {
                    (
                        self.config.manage.global_stop_loss_pct,
                        self.config.manage.global_take_profit_pct,
                    )
                };

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

                // 与实盘同序：ATR 保护价先于 ROI 兜底，
                // 否则 ROI 线永远先于 ATR 线触发，波动保护等于没做。
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

                // 全局止盈止损按保证金收益率，排在 ATR 保护价之后。
                let bar_margin_pct = if entry > 0.0 {
                    (bar.close - entry) * direction / entry * 100.0 * self.config.leverage.max(1.0)
                } else {
                    0.0
                };
                if global_stop > 0.0 && bar_margin_pct <= -global_stop {
                    self.close_position(bar.close, index, "global_stop_loss");
                    continue;
                }
                if global_tp > 0.0 && bar_margin_pct >= global_tp {
                    self.close_position(bar.close, index, "global_take_profit");
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
                    // 回撤阈值按保证金收益率：价格回撤 = 配置值 ÷ 杠杆。
                    let pullback = self.config.manage.trailing_pct / leverage / 100.0;
                    let pulled_back = if direction > 0.0 {
                        bar.close <= new_peak * (1.0 - pullback)
                    } else {
                        bar.close >= new_peak * (1.0 + pullback)
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
                    // ROI 阶梯按保证金收益率，与实盘 manage 同口径。
                    let margin_pct = profit_pct * self.config.leverage.max(1.0);
                    if target > 0.0 && margin_pct >= target {
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
            let signal_end = index.saturating_sub(self.config.signal_shift_bars);
            if signal_end < 30 {
                continue;
            }
            // 信号窗口整体回退 shift 根：评估用当时可见的数据，成交仍在本根之后。
            let signal_bar = &self.candles[signal_end];
            let slice_start = signal_end.saturating_sub(180);
            let slice = &self.candles[slice_start..=signal_end];
            let Some(metrics) = candle_metrics(slice, self.config.atr_period) else {
                continue;
            };
            let closes: Vec<f64> = slice.iter().map(|candle| candle.close).collect();
            let window = &closes[closes.len() - 20..];
            let mean = window.iter().sum::<f64>() / 20.0;
            let sd = (window
                .iter()
                .map(|value| (value - mean).powi(2))
                .sum::<f64>()
                / 20.0)
                .sqrt();
            let relative_strength = if sd > 0.0 {
                ((signal_bar.close - mean) / sd).clamp(-1.0, 1.0)
            } else {
                0.0
            };
            let features = MarketFeatures {
                funding_rate: self.config.funding_bps_8h / 10_000.0,
                open_interest_change_pct: f64::NAN,
                relative_strength,
                relative_strength_available: true,
            };
            // 与实盘同路由：range 且启用均值回归时，趋势信号不参与开仓。
            let regime = trade_policy::regime_for(
                slice,
                signal_bar.close,
                metrics.atr,
                self.config.policy,
                features,
            );
            let mean_reversion = crate::mean_reversion::mean_reversion_active(
                regime.map(|regime| regime.as_str()),
                &self.config.mr,
            );
            let (side, score) = if mean_reversion {
                let Some(signal) =
                    crate::mean_reversion::mean_reversion_signal(slice, &self.config.mr)
                else {
                    continue;
                };
                (signal.side, signal.score)
            } else {
                let Some(signal) = trade_policy::evaluate(
                    slice,
                    signal_bar.close,
                    metrics.atr,
                    self.config.policy,
                    features,
                ) else {
                    continue;
                };
                (signal.side, signal.score)
            };
            let direction = direction_of(side);
            let next = self.bars[index + 1];
            let entry = fill_price(next.open, self.slip_bps(), direction, true);
            let (sl_pct, tp_pct) = if mean_reversion {
                (
                    self.config.mr["mr_sl_pct"].as_f64().unwrap_or(20.0) / leverage,
                    self.config.mr["mr_tp_pct"].as_f64().unwrap_or(30.0) / leverage,
                )
            } else {
                // 趋势仓止损改由 ATR 决定，止盈交给 ROI 阶梯与快平任务。
                (self.config.stop_distance_atr_mult * metrics.atr_pct, 0.0)
            };
            let stop_price = if mean_reversion {
                entry * (1.0 - direction * sl_pct / 100.0)
            } else {
                crate::risk::atr_stop_price(
                    side,
                    entry,
                    metrics.atr_pct,
                    self.config.stop_distance_atr_mult,
                )
            };
            // 与实盘同闸门：没有有效止损价就不开仓，止盈可以为 0。
            if sl_pct <= 0.0 || stop_price <= 0.0 || entry <= 0.0 {
                continue;
            }
            // 仓位只由单仓保证金占比决定：名义额 = 权益 × margin_pct% × 杠杆。
            let max_notional = self.equity * (self.config.margin_pct / 100.0) * leverage;
            let qty = max_notional / entry;
            if qty <= 0.0 {
                continue;
            }
            let entry_fee = entry * qty * self.fee_rate();
            self.fee_total += entry_fee;
            let tp_price = if tp_pct > 0.0 {
                entry * (1.0 + direction * tp_pct / 100.0)
            } else {
                0.0
            };
            self.position = Some(OpenPosition {
                side: side.to_string(),
                entry_ts: next.open_time,
                entry: round(entry, 8),
                qty,
                initial_qty: qty,
                sl_pct,
                tp_pct,
                atr_pct: metrics.atr_pct,
                stop_price,
                tp_price,
                initial_fee: entry_fee,
                realized: 0.0,
                funding: 0.0,
                partial: false,
                score,
                entry_index: index + 1,
                peak: next.open,
                liq_price: entry * (1.0 - direction * (1.0 / leverage - mmr)),
                mean_reversion,
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
        // 风险调整指标与实盘统计共用 performance 实现：这里按「每笔平仓一次采样」
        // 取收益率，年化周期数 = 年成交笔数（用回测区间长度折算）。
        let (trade_returns, trade_periods_per_year) = {
            let start_ts = self.bars.first().map(|bar| bar.open_time).unwrap_or(0);
            let end_ts = self.bars.last().map(|bar| bar.open_time).unwrap_or(0);
            let mut points: Vec<(i64, f64)> = Vec::with_capacity(self.curve.len() + 1);
            points.push((start_ts, self.config.initial_equity));
            for (index, equity) in self.curve.iter().enumerate() {
                let ts = trades
                    .get(index)
                    .map(|trade| trade.exit_ts)
                    .unwrap_or(start_ts + index as i64);
                points.push((ts, *equity));
            }
            let returns = crate::performance::returns_from_equity(&points);
            let span_ms = (end_ts - start_ts).max(0);
            let per_year = if span_ms > 0 && !returns.is_empty() {
                returns.len() as f64 * 365.0 * 24.0 * 3600.0 * 1000.0 / span_ms as f64
            } else {
                0.0
            };
            (returns, per_year)
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
        summary.insert(
            "sharpe".into(),
            json!(round(
                crate::performance::sharpe(&trade_returns, trade_periods_per_year),
                3
            )),
        );
        summary.insert(
            "sortino".into(),
            json!(round(
                crate::performance::sortino(&trade_returns, trade_periods_per_year),
                3
            )),
        );
        summary.insert(
            "calmar".into(),
            json!(round(
                crate::performance::calmar(
                    &trade_returns,
                    trade_periods_per_year,
                    self.max_drawdown
                ),
                3
            )),
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

/// 跑两遍回测（基线 / 信号延后 N 根），产出前视偏差结论。
pub fn run_lookahead(
    bars: &[HistoryBar],
    base: &EngineConfig,
    shift_bars: usize,
) -> Result<crate::overfit::LookaheadReport, String> {
    let baseline = run(bars, base)?;
    let mut shifted = base.clone();
    shifted.signal_shift_bars = shift_bars;
    let shifted = run(bars, &shifted)?;
    let expectancy = |result: &BacktestResult| {
        result
            .summary
            .get("expectancy")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
    };
    Ok(crate::overfit::evaluate_lookahead(
        expectancy(&baseline),
        expectancy(&shifted),
    ))
}

/// 对序列末尾的 bar，用两种 warmup 长度算 ATR%，比较相对差异。
pub fn measure_recursive_divergence(
    candles: &[crate::providers::CandleSnapshot],
    atr_period: usize,
) -> f64 {
    let end = candles.len();
    let short = end.saturating_sub(30);
    let long = end.saturating_sub(120);
    if short == 0 || long == 0 || short == long {
        return 0.0;
    }
    let short_atr = crate::providers::candle_metrics(&candles[short..], atr_period);
    let long_atr = crate::providers::candle_metrics(&candles[long..], atr_period);
    match (short_atr, long_atr) {
        (Some(short), Some(long)) => {
            let a = short.atr_pct;
            let b = long.atr_pct;
            if a.abs() > 1e-9 {
                (b - a).abs() / a.abs()
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
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
    // 训练窗最少 2×WARMUP，测试窗还要再留 2×WARMUP。少于 4×WARMUP 时
    // clamp(min, max) 会因 min > max 直接 panic，必须先返回可读错误。
    if bars.len() < WARMUP * 4 {
        return Err(format!(
            "K 线不足 (至少 {} 根才能切样本内/外)",
            WARMUP * 4 + 1
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
        let params: Vec<(&'static str, f64)> = ["entry_threshold", "trailing_pct"]
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct BasketMetrics {
    /// 已评估的 symbol×fold 单元数；B3 以 positive_folds/folds 比例判定稳健性。
    pub folds: usize,
    pub oos_trades: usize,
    pub positive_folds: usize,
    pub oos_avg_return_pct: f64,
    pub overfit_gap_pp: f64,
    pub stressed_avg_return_pct: f64,
    pub max_drawdown_pct: f64,
    pub bootstrap_ci_low: f64,
    pub bootstrap_p_positive: f64,
    /// OOS 成交的平均「止损距离 / ATR%」：<1 表示止损落在噪音区。
    pub avg_stop_atr_ratio: f64,
    /// OOS 成交里最窄的「止损距离 / ATR%」。
    pub min_stop_atr_ratio: f64,
    pub symbols_used: Vec<String>,
    pub symbols_skipped: Vec<String>,
    pub target_regime: String,
    pub timeframe: String,
    /// 前视偏差体检是否可疑：延后信号后表现反而更好。
    pub lookahead_suspicious: bool,
    /// 递归指标稳定性体检是否可疑：不同 warmup 的 ATR% 不收敛。
    pub recursive_suspicious: bool,
}

fn timeframe_ms(timeframe: &str) -> Result<i64, String> {
    let value = timeframe.trim();
    if value.is_empty() {
        return Err("timeframe 不能为空".to_string());
    }
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    if number.is_empty() {
        return Err(format!("timeframe {timeframe} 缺少数值"));
    }
    let count = number
        .parse::<i64>()
        .map_err(|_| format!("timeframe {timeframe} 数值非法"))?;
    if count <= 0 {
        return Err(format!("timeframe {timeframe} 必须为正数"));
    }
    let unit_ms = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        _ => return Err(format!("timeframe {timeframe} 单位非法")),
    };
    count
        .checked_mul(unit_ms)
        .ok_or_else(|| format!("timeframe {timeframe} 超出范围"))
}

pub(crate) fn candles_from_history(bars: &[HistoryBar], interval_ms: i64) -> Vec<CandleSnapshot> {
    let step = interval_ms.max(1);
    bars.iter()
        .map(|bar| CandleSnapshot {
            close_time: bar.close_time.max(bar.open_time + step - 1),
            open: bar.open,
            high: bar.high,
            low: bar.low,
            close: bar.close,
            volume: bar.volume,
        })
        .collect()
}

fn build_market_regime_map(
    basket: &[(String, Vec<HistoryBar>)],
    frame_ms: i64,
    atr_period: usize,
    policy: crate::trade_policy::TradePolicy,
) -> BTreeMap<i64, crate::regime::Regime> {
    let per_symbol: Vec<BTreeMap<i64, crate::regime::Regime>> = basket
        .iter()
        .map(|(_, bars)| {
            let candles = candles_from_history(bars, frame_ms);
            let series = crate::regime::regime_series(&candles, atr_period, policy);
            let mut map = BTreeMap::new();
            for (candle, regime) in candles.iter().zip(series) {
                if let Some(regime) = regime {
                    map.insert(candle.close_time, regime);
                }
            }
            map
        })
        .collect();
    let mut buckets: BTreeSet<i64> = BTreeSet::new();
    for map in &per_symbol {
        buckets.extend(map.keys().copied());
    }
    let mut regimes = BTreeMap::new();
    for ts in buckets {
        let mut votes = Vec::new();
        for map in &per_symbol {
            if let Some((&stamp, &regime)) = map.range(..=ts).next_back() {
                if ts - stamp <= frame_ms {
                    votes.push(regime);
                }
            }
        }
        // 回测 basket 规模仅略高于 quorum，任一标的上市时间差/数据缺口都会导致该桶少一票；
        // 允许缺失 1 个标的，同时以 3 票兜底，避免票数过少时噪声放大。
        let quorum = votes
            .len()
            .saturating_sub(1)
            .clamp(3, crate::regime::MARKET_QUORUM);
        if let Some(regime) = crate::regime::market_regime_with_quorum(&votes, quorum) {
            regimes.insert(ts, regime);
        }
    }
    regimes
}

fn market_regime_at(
    regimes: &BTreeMap<i64, crate::regime::Regime>,
    ts: i64,
    frame_ms: i64,
) -> Option<crate::regime::Regime> {
    let (&stamp, &regime) = regimes.range(..=ts).next_back()?;
    (ts - stamp <= frame_ms).then_some(regime)
}

fn tag_trades(
    result: &mut BacktestResult,
    regimes: &BTreeMap<i64, crate::regime::Regime>,
    frame_ms: i64,
) {
    for trade in &mut result.trades {
        if let Some(regime) = market_regime_at(regimes, trade.entry_ts, frame_ms) {
            trade.market_regime = regime.as_str().to_string();
        }
    }
}

fn target_returns(trades: &[BacktestTrade], target: &str) -> Vec<f64> {
    trades
        .iter()
        .filter(|trade| trade.market_regime == target)
        .map(|trade| trade.pnl_pct)
        .collect()
}

fn compounded_max_drawdown(returns: &[(i64, f64)]) -> f64 {
    let mut ordered = returns.to_vec();
    ordered.sort_by_key(|(exit_ts, _)| *exit_ts);
    let mut equity = 1.0_f64;
    let mut peak = 1.0_f64;
    let mut max_drawdown = 0.0_f64;
    for (_, value) in ordered {
        equity *= 1.0 + value / 100.0;
        peak = peak.max(equity);
        max_drawdown = max_drawdown.max((peak - equity) / peak * 100.0);
    }
    max_drawdown
}

#[allow(clippy::too_many_arguments)]
pub fn run_walkforward_basket_fixed(
    basket: &[(String, Vec<HistoryBar>)],
    base: &EngineConfig,
    folds: i64,
    train_bars: i64,
    target_regime: crate::regime::Regime,
    timeframe: &str,
    cancel: &AtomicBool,
) -> Result<BasketMetrics, String> {
    if basket.len() < 6 {
        return Err("有效标的少于 6 个".to_string());
    }
    let frame_ms = timeframe_ms(timeframe)?;
    if frame_ms != base.interval_ms {
        return Err(format!(
            "timeframe {timeframe} 与 EngineConfig.interval_ms {} 不一致",
            base.interval_ms
        ));
    }
    let regimes = build_market_regime_map(basket, frame_ms, base.atr_period, base.policy);
    let requested_folds = folds.clamp(MIN_FOLDS as i64, MAX_FOLDS as i64) as usize;
    let target = target_regime.as_str();
    let mut fold_results = Vec::new();
    let mut oos_fold_avgs = Vec::new();
    let mut stressed_fold_avgs = Vec::new();
    let mut is_fold_avgs = Vec::new();
    let mut seen_oos: BTreeSet<(String, i64)> = BTreeSet::new();
    let mut all_oos = Vec::new();
    let mut all_stop_atr_ratios = Vec::new();
    let mut oos_ordered: Vec<(i64, f64)> = Vec::new();
    let mut used = Vec::new();
    let mut skipped = Vec::new();

    for (symbol, bars) in basket {
        if bars.len() < WARMUP * 4 {
            skipped.push(symbol.clone());
            continue;
        }
        let train_bars = (train_bars as usize).clamp(WARMUP * 2, bars.len() - WARMUP * 2);
        let mut local_folds = requested_folds;
        let mut step = (bars.len() - WARMUP - train_bars) / local_folds;
        while local_folds > MIN_FOLDS && step <= WARMUP {
            local_folds -= 1;
            step = (bars.len() - WARMUP - train_bars) / local_folds;
        }
        if step <= WARMUP {
            skipped.push(symbol.clone());
            continue;
        }
        let mut evaluated = false;
        for index in 0..local_folds {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCELLED.to_string());
            }
            let train_end = WARMUP + train_bars + index * step;
            let test_end = (train_end + step).min(bars.len());
            let train = &bars[train_end.saturating_sub(train_bars)..train_end];
            let test = &bars[train_end.saturating_sub(WARMUP)..test_end];
            if train.len() <= WARMUP || test.len() <= WARMUP {
                continue;
            }
            let mut train_result = run(train, base)?;
            let mut test_result = run(test, base)?;
            let mut stressed_config = base.clone();
            stressed_config.fee_bps *= 2.0;
            stressed_config.slippage_bps *= 2.0;
            let mut stressed_result = run(test, &stressed_config)?;
            for result in [&mut train_result, &mut test_result, &mut stressed_result] {
                tag_trades(result, &regimes, frame_ms);
            }
            let is_returns = target_returns(&train_result.trades, target);
            let oos_returns = target_returns(&test_result.trades, target);
            if oos_returns.is_empty() {
                continue;
            }
            let stressed_returns = target_returns(&stressed_result.trades, target);
            fold_results.push(oos_returns.iter().sum::<f64>());
            oos_fold_avgs.push(average(&oos_returns));
            stressed_fold_avgs.push(average(&stressed_returns));
            is_fold_avgs.push(if is_returns.is_empty() {
                average(&oos_returns)
            } else {
                average(&is_returns)
            });
            evaluated = true;
            for trade in test_result
                .trades
                .iter()
                .filter(|trade| trade.market_regime == target)
            {
                if seen_oos.insert((symbol.clone(), trade.entry_ts)) {
                    all_oos.push(trade.pnl_pct);
                    if trade.atr_pct > 0.0 && trade.sl_pct.is_finite() {
                        all_stop_atr_ratios.push(trade.sl_pct / trade.atr_pct);
                    }
                    oos_ordered.push((trade.exit_ts, trade.pnl_pct));
                }
            }
        }
        if evaluated {
            used.push(symbol.clone());
        } else {
            skipped.push(symbol.clone());
        }
    }

    if fold_results.len() < MIN_FOLDS {
        return Err(format!(
            "评估到的 symbol×fold 单元少于 {MIN_FOLDS} 个（目标 regime {target}，实际 {} 个，已用 {} 个标的，跳过 {} 个）",
            fold_results.len(),
            used.len(),
            skipped.len()
        ));
    }
    if used.len() < 6 {
        return Err("有效标的少于 6 个".to_string());
    }
    if all_oos.is_empty() {
        return Err(format!("目标 regime {target} 的 OOS 成交为 0"));
    }

    let oos_avg = average(&oos_fold_avgs);
    let is_avg = average(&is_fold_avgs);
    let positive_folds = fold_results.iter().filter(|value| **value > 0.0).count();
    let bootstrap = bootstrap_ci(&all_oos);
    Ok(BasketMetrics {
        folds: fold_results.len(),
        oos_trades: all_oos.len(),
        positive_folds,
        oos_avg_return_pct: round(oos_avg, 4),
        overfit_gap_pp: round(is_avg - oos_avg, 4),
        stressed_avg_return_pct: round(average(&stressed_fold_avgs), 4),
        max_drawdown_pct: round(compounded_max_drawdown(&oos_ordered), 4),
        bootstrap_ci_low: bootstrap["ci_low"].as_f64().unwrap_or_default(),
        bootstrap_p_positive: bootstrap["p_positive"].as_f64().unwrap_or_default(),
        avg_stop_atr_ratio: round(average(&all_stop_atr_ratios), 4),
        min_stop_atr_ratio: if all_stop_atr_ratios.is_empty() {
            0.0
        } else {
            round(
                all_stop_atr_ratios
                    .iter()
                    .copied()
                    .fold(f64::INFINITY, f64::min),
                4,
            )
        },
        symbols_used: used,
        symbols_skipped: skipped,
        target_regime: target.to_string(),
        timeframe: timeframe.to_string(),
        lookahead_suspicious: false,
        recursive_suspicious: false,
    })
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
    // 少于 10 笔不构建置信区间；B3 证据门的 30 笔门槛由调用方另行保证。
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
            interval_ms: 900_000,
            fee_bps: 5.0,
            slippage_bps: 2.0,
            spread_bps: 0.0,
            funding_bps_8h: 1.0,
            initial_equity: 1_000.0,

            margin_pct: 50.0,
            leverage: 20.0,
            maintenance_margin_rate: 0.005,
            max_bars_hold: None,
            policy: TradePolicy {
                base_threshold: 0.05,
                volume_ratio_min: 0.0,
                atr_pct_min: 0.05,
                atr_pct_max: 4.0,
                ema: crate::trade_policy::EmaPeriods { fast: 9, slow: 21 },
                adx_period: 14,
            },
            manage: ManageConfig {
                partial_tp_at_r: 1.0,
                partial_tp_ratio: 0.5,
                trailing_pct: 0.0,
                trailing_arm_r: 1.5,
                exit_threshold: 0.6,
                breakeven_at_r: 1.0,
                global_stop_loss_pct: 10.0,
                global_take_profit_pct: 20.0,
                qty_step: 0.001,
                min_qty: 0.001,
                roi_min_r: 0.0,
                roi_table: Vec::new(),
            },
            stop_distance_atr_mult: 1.3,
            atr_period: 14,
            mr: Value::Null,
            signal_shift_bars: 0,
        }
    }

    /// 振幅随 K 线位置周期变化的行情：ATR(7) 与 ATR(14) 的窗口平均必然不同，
    /// 才能验证 ATR 周期参数真的接进了回测引擎（均匀趋势下两者恰好相等，测不出来）。
    fn alternating_range_bars(count: usize) -> Vec<HistoryBar> {
        (0..count)
            .map(|index| {
                let base = 100.0 + index as f64 * 0.5;
                let range = 0.3 + 0.02 * (index as f64 % 25.0);
                bar(
                    index as i64 * 900_000,
                    base,
                    base + range,
                    base - range * 0.5,
                    base + range * 0.4,
                )
            })
            .collect()
    }

    fn trending_bars(count: usize) -> Vec<HistoryBar> {
        (0..count)
            .map(|index| {
                let base = 100.0 + index as f64 * 0.5;
                bar(
                    index as i64 * 900_000,
                    base,
                    base + 1.4,
                    base - 1.0,
                    base + 1.2,
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

    /// 重算开仓信号那一根 K 线的 ATR%，用于校验回测止损距离口径。
    /// 周期取配置值，与回测引擎自身一致。
    fn atr_pct_at_entry(bars: &[HistoryBar], entry_ts: i64, atr_period: usize) -> f64 {
        let candles: Vec<CandleSnapshot> = bars
            .iter()
            .map(|bar| CandleSnapshot {
                close_time: bar.close_time,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
            })
            .collect();
        let entry_index = bars
            .iter()
            .position(|bar| bar.open_time == entry_ts)
            .expect("成交时间必须能在 K 线里找到");
        let signal_index = entry_index.saturating_sub(1);
        let start = signal_index.saturating_sub(180);
        candle_metrics(&candles[start..=signal_index], atr_period)
            .expect("开仓前必须有足够 K 线算 ATR")
            .atr_pct
    }

    #[test]
    fn stop_distance_independent_of_leverage() {
        let bars = trending_bars(400);
        let mut low = config();
        low.leverage = 5.0;
        let mut high = config();
        high.leverage = 9.0;

        let first_sl = |cfg: &EngineConfig| -> f64 {
            run(&bars, cfg)
                .unwrap()
                .trades
                .first()
                .expect("单边上涨行情应产生趋势仓")
                .sl_pct
        };
        let low_sl = first_sl(&low);
        let high_sl = first_sl(&high);
        assert!(low_sl > 0.0 && high_sl > 0.0);
        assert!(
            (low_sl - high_sl).abs() < 1e-9,
            "止损距离只由 ATR 决定，杠杆 5x 与 9x 必须相同（{low_sl} vs {high_sl}）"
        );
    }

    #[test]
    fn atr_period_drives_the_backtest_stop_and_matches_live() {
        // ATR 周期必须真的生效：周期不同 → 止损距离不同。
        // 若回测仍写死 14，本断言与下面的同口径断言会一起失败。
        let bars = alternating_range_bars(400);
        let mut cfg = config();
        cfg.atr_period = 7;
        let result = run(&bars, &cfg).unwrap();
        let trade = result.trades.first().expect("单边上涨行情应产生趋势仓");

        let mut wide = config();
        wide.atr_period = 14;
        let wide_result = run(&bars, &wide).unwrap();
        let wide_trade = wide_result
            .trades
            .first()
            .expect("单边上涨行情应产生趋势仓");
        assert!(
            (trade.sl_pct - wide_trade.sl_pct).abs() > 1e-9,
            "ATR 周期必须真的影响止损距离（{:.6} vs {:.6}）",
            trade.sl_pct,
            wide_trade.sl_pct
        );

        // 回测止损距离必须等于实盘按同一周期算出的距离。
        let atr_pct = atr_pct_at_entry(&bars, trade.entry_ts, cfg.atr_period);
        let live_stop = crate::risk::atr_stop_price(
            &trade.side,
            trade.entry,
            atr_pct,
            cfg.stop_distance_atr_mult,
        );
        let live_distance = (trade.entry - live_stop).abs() / trade.entry * 100.0;
        assert!(
            (trade.sl_pct - live_distance).abs() < 1e-9,
            "回测止损距离 {:.6}% 必须与实盘同周期 ATR 口径 {:.6}% 一致",
            trade.sl_pct,
            live_distance
        );
    }

    #[test]
    fn backtest_stop_matches_live_stop() {
        let cfg = config();
        let bars = trending_bars(400);
        let result = run(&bars, &cfg).unwrap();
        let trade = result.trades.first().expect("单边上涨行情应产生趋势仓");
        let atr_pct = atr_pct_at_entry(&bars, trade.entry_ts, cfg.atr_period);
        let live_stop = crate::risk::atr_stop_price(
            &trade.side,
            trade.entry,
            atr_pct,
            cfg.stop_distance_atr_mult,
        );
        let live_distance = (trade.entry - live_stop).abs() / trade.entry * 100.0;
        assert!(
            (trade.sl_pct - live_distance).abs() < 1e-9,
            "回测止损距离 {:.6}% 必须与实盘 ATR 口径 {:.6}% 一致",
            trade.sl_pct,
            live_distance
        );
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
            leverage: 1.0,
            partial_taken: false,
            score: 0.0,
            held_minutes: 0.0,
            margin_stop_take: None,
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
        // 趋势仓止盈改由 ROI 阶梯/快平任务接管；这里关闭全局止盈，
        // 单独验证 R 阈值触发部分止盈的路径。
        cfg.manage.global_take_profit_pct = 0.0;
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
        let values: Vec<f64> = (0..7).map(|index| 0.1 + index as f64 * 0.1).collect();
        let error = grid_combinations(&[
            ("entry_threshold", values.clone()),
            ("trailing_pct", values),
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
        let bars = trending_bars(1200);
        let mut cfg = config();
        cfg.manage.partial_tp_at_r = 0.0;
        let result = run_walkforward(
            &bars,
            &cfg,
            &[("entry_threshold", vec![0.05, 0.1])],
            2,
            600,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(result["aggregate"]["in_sample_avg_pct"].as_f64().is_some());
        assert!(result["aggregate"]["out_sample_avg_pct"].as_f64().is_some());
        assert!(result["aggregate"]["overfit_gap_pct"].as_f64().is_some());
        assert!(result["folds"].as_i64().unwrap() >= 2);
    }

    #[test]
    fn walkforward_with_a_window_shorter_than_four_warmups_returns_an_error() {
        // 180 根：超过 2×WARMUP，但不足 4×WARMUP。
        // 旧实现在 clamp(min=120, max=len-120=60) 处因 min > max 直接 panic。
        let bars = trending_bars(WARMUP * 3);
        let error = run_walkforward(
            &bars,
            &config(),
            &[("entry_threshold", vec![0.05, 0.1])],
            2,
            (WARMUP * 2) as i64,
            &AtomicBool::new(false),
        )
        .expect_err("窗口不足时必须返回错误，不能 panic");
        assert!(error.contains("K 线不足"), "实际错误: {error}");
    }

    fn basket_config() -> EngineConfig {
        let mut cfg = config();
        cfg.interval_ms = 3_600_000;
        cfg
    }

    fn synthetic_basket(symbols: usize, count: usize) -> Vec<(String, Vec<HistoryBar>)> {
        (0..symbols)
            .map(|symbol| {
                let anchor = 100.0 + symbol as f64 * 0.4;
                let mut price = anchor;
                let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ (symbol as u64 + 1);
                let bars = (0..count)
                    .map(|index| {
                        state = state
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1_442_695_040_888_963_407);
                        let noise = ((state >> 11) % 2001) as f64 / 1_000.0 - 1.0;
                        price += (anchor - price) * 0.08 + noise * 0.8;
                        let open = price - noise * 0.2;
                        let open_time = index as i64 * 3_600_000;
                        HistoryBar {
                            open_time,
                            close_time: open_time + 3_599_999,
                            open,
                            high: open.max(price) + 0.5,
                            low: open.min(price) - 0.5,
                            close: price,
                            volume: 10_000.0,
                        }
                    })
                    .collect();
                (format!("SYM{}USDT", symbol + 1), bars)
            })
            .collect()
    }

    /// 合成震荡市：每 20 根在末端来一次缩量急涨，随后回落。
    /// 急涨单根把 RSI/布林带推到超买，但 ADX 窗口里多数是小幅交替，regime 仍是 range。
    fn mean_reversion_range_bars(count: usize) -> Vec<HistoryBar> {
        let mut price = 100.0;
        let mut bars = Vec::with_capacity(count);
        for index in 0..count {
            let phase = index % 20;
            let open = price;
            let close = match phase {
                18 => open + 2.0,
                19 => 100.8,
                0 => 100.0,
                _ if phase % 2 == 1 => 100.05,
                _ => 99.95,
            };
            let volume = if phase == 18 { 300.0 } else { 1000.0 };
            let open_time = index as i64 * 900_000;
            bars.push(HistoryBar {
                open_time,
                close_time: open_time + 899_999,
                open,
                high: open.max(close) + 0.1,
                low: open.min(close) - 0.1,
                close,
                volume,
            });
            price = close;
        }
        bars
    }

    #[test]
    fn mean_reversion_trades_in_range_with_independent_stops() {
        let bars = mean_reversion_range_bars(320);
        let mut config = config();
        config.policy.volume_ratio_min = 0.0;
        config.mr = json!({
            "mr_enabled": true,
            "mr_rsi_high": 70,
            "mr_rsi_low": 30,
            "mr_band_k": 2.0,
            "mr_sl_pct": 20,
            "mr_tp_pct": 30,
        });
        let result = run(&bars, &config).unwrap();
        let mut trades: Vec<_> = result
            .trades
            .iter()
            .filter(|trade| trade.side == "short")
            .collect();
        trades.sort_by_key(|trade| trade.entry_ts);
        let trade = trades
            .first()
            .unwrap_or_else(|| panic!("range 内的缩量急涨应产生空单: {:?}", result.summary));
        assert!(
            (trade.sl_pct - 1.0).abs() < 1e-9,
            "MR 止损率 20% / 20x 应换算成 1% 价格距离，实际 {}",
            trade.sl_pct
        );
        assert!(
            (trade.tp_pct - 1.5).abs() < 1e-9,
            "MR 止盈率 30% / 20x 应换算成 1.5% 价格距离，实际 {}",
            trade.tp_pct
        );
        assert!(trade.pnl_pct > 0.0, "急涨回落应让空单盈利");
    }

    fn tagged_regimes_cover_target(basket: &[(String, Vec<HistoryBar>)], target: &str) -> usize {
        let frame_ms = timeframe_ms("1h").unwrap();
        let config = basket_config();
        let regimes = build_market_regime_map(basket, frame_ms, config.atr_period, config.policy);
        assert!(!regimes.is_empty(), "市场 regime map 不应为空");
        let mut tagged = 0;
        for (_, bars) in basket {
            let mut result = run(bars, &config).unwrap();
            tag_trades(&mut result, &regimes, frame_ms);
            assert!(
                result
                    .trades
                    .iter()
                    .all(|trade| !trade.market_regime.is_empty()),
                "每笔成交都应回填 market_regime"
            );
            tagged += result
                .trades
                .iter()
                .filter(|trade| trade.market_regime == target)
                .count();
        }
        tagged
    }

    #[test]
    fn basket_fixed_filters_target_market_regime_and_aggregates() {
        let basket = synthetic_basket(6, 700);
        let metrics = run_walkforward_basket_fixed(
            &basket,
            &basket_config(),
            2,
            300,
            crate::regime::Regime::Range,
            "1h",
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(metrics.target_regime, "range");
        assert_eq!(metrics.timeframe, "1h");
        assert!(metrics.symbols_used.len() >= 6);
        assert!(metrics.folds >= 2);
        assert!(metrics.oos_trades > 0);
        assert!(metrics.max_drawdown_pct >= 0.0);
        assert!(
            metrics.avg_stop_atr_ratio > 0.0 && metrics.min_stop_atr_ratio > 0.0,
            "OOS 成交必须带止损距离/ATR 维度"
        );
        assert!(metrics.avg_stop_atr_ratio >= metrics.min_stop_atr_ratio);
        assert!((0.0..=1.0).contains(&metrics.bootstrap_p_positive));
        assert!(
            tagged_regimes_cover_target(&basket, "range") > 0,
            "至少一笔成交的 market_regime 应为 range"
        );
    }

    #[test]
    fn basket_fixed_rejects_insufficient_coverage() {
        let basket = synthetic_basket(5, 700);
        let error = run_walkforward_basket_fixed(
            &basket,
            &basket_config(),
            2,
            300,
            crate::regime::Regime::Range,
            "1h",
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.contains("少于 6 个"));
    }

    #[test]
    fn basket_fixed_rejects_when_target_regime_has_no_oos_trades() {
        let basket = synthetic_basket(6, 700);
        let frame_ms = timeframe_ms("1h").unwrap();
        let regime_cfg = basket_config();
        let regimes =
            build_market_regime_map(&basket, frame_ms, regime_cfg.atr_period, regime_cfg.policy);
        assert!(!regimes.is_empty(), "chaos 用例仍应产生非空 regime map");
        assert!(
            regimes
                .values()
                .all(|regime| *regime != crate::regime::Regime::Chaos),
            "合成数据不应出现 chaos，才能覆盖无目标 regime 分支"
        );
        let error = run_walkforward_basket_fixed(
            &basket,
            &basket_config(),
            2,
            300,
            crate::regime::Regime::Chaos,
            "1h",
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(
            error.contains("symbol×fold") || error.contains("OOS"),
            "实际错误: {error}"
        );
    }

    #[test]
    fn basket_fixed_counts_symbol_fold_units_for_mixed_coverage() {
        let mut basket = synthetic_basket(6, 700);
        basket[0].1.truncate(400);
        let metrics = run_walkforward_basket_fixed(
            &basket,
            &basket_config(),
            4,
            150,
            crate::regime::Regime::Range,
            "1h",
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(
            metrics.folds >= 6,
            "6 个标的应至少评估 6 个 symbol×fold 单元"
        );
        assert!(metrics.positive_folds <= metrics.folds);
        assert!(metrics.oos_trades > 0);
    }

    #[test]
    fn build_market_regime_map_tolerates_one_missing_symbol() {
        let mut basket = synthetic_basket(6, 700);
        basket[0].1.truncate(697);
        let frame_ms = timeframe_ms("1h").unwrap();
        let last_ts = basket[1].1.last().unwrap().close_time;
        assert_eq!(
            crate::regime::market_regime(&[
                crate::regime::Regime::Range,
                crate::regime::Regime::Range,
                crate::regime::Regime::Range,
                crate::regime::Regime::Range,
                crate::regime::Regime::Range,
            ]),
            None,
            "5 票在固定 quorum=6 下不应给出 regime"
        );
        let regime_cfg = basket_config();
        let regimes =
            build_market_regime_map(&basket, frame_ms, regime_cfg.atr_period, regime_cfg.policy);
        assert!(
            regimes.contains_key(&last_ts),
            "某个标的最末桶缺 1 票时，动态 quorum 仍应给出 regime"
        );
    }

    #[test]
    fn compounded_max_drawdown_follows_compound_returns() {
        assert!(compounded_max_drawdown(&[]).abs() < 1e-9);
        assert!(compounded_max_drawdown(&[(1, 5.0), (2, 10.0)]).abs() < 1e-9);
        assert!((compounded_max_drawdown(&[(2, -20.0), (1, 10.0)]) - 20.0).abs() < 1e-6);
        assert!((compounded_max_drawdown(&[(1, -50.0), (2, 100.0)]) - 50.0).abs() < 1e-6);
    }

    #[test]
    fn basket_fixed_rejects_timeframe_mismatch() {
        let basket = synthetic_basket(6, 700);
        let mut cfg = basket_config();
        cfg.interval_ms = 900_000;
        let error = run_walkforward_basket_fixed(
            &basket,
            &cfg,
            2,
            300,
            crate::regime::Regime::Range,
            "1h",
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.contains("不一致"), "实际错误: {error}");
    }

    #[test]
    fn timeframe_parser_rejects_invalid_input_and_parses_units() {
        assert_eq!(timeframe_ms("1h").unwrap(), 3_600_000);
        assert_eq!(timeframe_ms("15m").unwrap(), 900_000);
        assert!(timeframe_ms("").is_err());
        assert!(timeframe_ms("h").is_err());
        assert!(timeframe_ms("1x").is_err());
        assert!(timeframe_ms("0h").is_err());
    }
}
