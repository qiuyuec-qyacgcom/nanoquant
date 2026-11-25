use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MarketSnapshot {
    pub symbol: String,
    pub mark_price: f64,
    pub timestamp: i64,
    pub quote_volume: f64,
    pub funding_rate: f64,
    pub open_interest: f64,
    pub open_interest_change_pct: f64,
    pub bid: f64,
    pub ask: f64,
    pub spread_bps: f64,
    pub candles: Vec<CandleSnapshot>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CandleSnapshot {
    pub close_time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExchangeTicker {
    pub symbol: String,
    pub quote_volume: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BookTop {
    pub symbol: String,
    // 盘口买一/卖一暂未被扫描读取，仅用于计算/审计点差。
    #[allow(dead_code)]
    pub bid: f64,
    #[allow(dead_code)]
    pub ask: f64,
    pub spread_bps: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AccountSnapshot {
    pub equity: f64,
    pub wallet: f64,
    pub available_balance: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PositionSnapshot {
    pub symbol: String,
    pub side: String,
    pub qty: f64,
    pub entry_price: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TradingSnapshot {
    pub markets: Vec<MarketSnapshot>,
    pub account: AccountSnapshot,
    pub positions: Vec<PositionSnapshot>,
    pub open_orders: Vec<OpenOrderSnapshot>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OpenOrderSnapshot {
    pub symbol: String,
    pub order_id: i64,
    pub client_order_id: String,
    pub side: String,
    pub order_type: String,
    pub status: String,
    pub original_qty: f64,
    pub executed_qty: f64,
    pub price: f64,
    pub stop_price: f64,
    pub reduce_only: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderError {
    Unavailable(String),
    Timeout,
    InvalidData(String),
}

pub trait SnapshotProvider: Send + Sync {
    fn snapshot(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<TradingSnapshot, ProviderError>> + Send + '_>>;
}

#[derive(Debug, Clone)]
#[cfg(test)]
pub struct DemoProvider {
    mark_price: f64,
}

#[cfg(test)]
impl DemoProvider {
    pub fn new() -> Self {
        Self {
            mark_price: 50_000.0,
        }
    }

    pub fn with_mark_price(mark_price: f64) -> Self {
        Self { mark_price }
    }
}

#[cfg(test)]
impl Default for DemoProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl SnapshotProvider for DemoProvider {
    fn snapshot(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<TradingSnapshot, ProviderError>> + Send + '_>> {
        Box::pin(async move {
            if !self.mark_price.is_finite() || self.mark_price <= 0.0 {
                return Err(ProviderError::InvalidData(
                    "mark_price 必须是正的有限数值".into(),
                ));
            }
            let timestamp = crate::now();
            let interval_ms = 4 * 60 * 60 * 1_000;
            let last_close = timestamp * 1_000 / interval_ms * interval_ms - 1;
            let candles = (0..250)
                .rev()
                .map(|index| CandleSnapshot {
                    close_time: last_close - index * interval_ms,
                    open: self.mark_price,
                    high: self.mark_price,
                    low: self.mark_price,
                    close: self.mark_price,
                    volume: 100.0,
                })
                .collect();
            Ok(TradingSnapshot {
                markets: vec![MarketSnapshot {
                    symbol: "BTCUSDT".into(),
                    mark_price: self.mark_price,
                    timestamp,
                    quote_volume: 5_000_000.0,
                    funding_rate: 0.0,
                    open_interest: 1_000_000.0,
                    open_interest_change_pct: 0.0,
                    bid: self.mark_price * 0.9999,
                    ask: self.mark_price * 1.0001,
                    spread_bps: (0.0002 / self.mark_price * 10000.0).max(2.0),
                    candles,
                }],
                account: AccountSnapshot {
                    equity: 10_000.0,
                    wallet: 10_000.0,
                    available_balance: 10_000.0,
                },
                positions: Vec::new(),
                open_orders: Vec::new(),
            })
        })
    }
}

#[derive(Clone)]
pub struct BinanceProvider {
    client: reqwest::Client,
    base_url: String,
    symbols: Vec<String>,
    api_key: String,
    secret: String,
    timeframe: String,

    /// demo 无凭证时，账户/持仓/订单从这里读本地账本。
    public_book: Option<crate::db::Db>,
}

impl BinanceProvider {
    #[cfg(test)]
    pub fn new(
        base_url: impl Into<String>,
        symbol: impl Into<String>,
        api_key: String,
        secret: String,
    ) -> Result<Self, ProviderError> {
        Self::with_symbols(base_url, vec![symbol.into()], api_key, secret)
    }

    pub fn with_symbols(
        base_url: impl Into<String>,
        symbols: Vec<String>,
        api_key: String,
        secret: String,
    ) -> Result<Self, ProviderError> {
        if api_key.trim().is_empty() || secret.trim().is_empty() {
            return Err(ProviderError::Unavailable("未配置交易所 API 凭证".into()));
        }
        if symbols.is_empty() || symbols.iter().any(|symbol| symbol.trim().is_empty()) {
            return Err(ProviderError::InvalidData(
                "必须配置至少一个有效交易对".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| ProviderError::Unavailable(e.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            symbols,
            api_key,
            secret,
            timeframe: "4h".into(),

            public_book: None,
        })
    }

    /// 全市场行情读取：无 API 凭证，只访问币安公开接口。
    pub fn public(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ProviderError::Unavailable(e.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            symbols: Vec::new(),
            api_key: String::new(),
            secret: String::new(),
            timeframe: "4h".into(),
            public_book: None,
        })
    }

    /// demo 无 API 凭证：行情走公开接口，账户与持仓/订单读本地账本。
    pub fn with_symbols_public(
        base_url: impl Into<String>,
        symbols: Vec<String>,
        db: crate::db::Db,
    ) -> Result<Self, ProviderError> {
        if symbols.is_empty() || symbols.iter().any(|symbol| symbol.trim().is_empty()) {
            return Err(ProviderError::InvalidData(
                "必须配置至少一个有效交易对".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| ProviderError::Unavailable(e.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            symbols,
            api_key: String::new(),
            secret: String::new(),
            timeframe: "4h".into(),

            public_book: Some(db),
        })
    }

    fn local_book(
        &self,
        mode: &str,
    ) -> Result<
        (
            AccountSnapshot,
            Vec<PositionSnapshot>,
            Vec<OpenOrderSnapshot>,
        ),
        ProviderError,
    > {
        let db = self
            .public_book
            .as_ref()
            .ok_or_else(|| ProviderError::Unavailable("本地账本不可用".into()))?;
        let conn = db
            .conn
            .lock()
            .map_err(|_| ProviderError::Unavailable("本地账本锁不可用".into()))?;
        let latest: Option<(f64, f64)> = conn
            .query_row(
                "SELECT wallet, margin_used FROM equity_points WHERE mode=?1 ORDER BY ts DESC LIMIT 1",
                [mode],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        let (equity, margin_used) = latest.unwrap_or_else(|| {
            let initial = conn
                .query_row(
                    "SELECT value FROM app_settings WHERE key='engine'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|value| value["initial_equity"].as_f64())
                .unwrap_or(1000.0);
            (initial, 0.0)
        });
        let positions = {
            let mut stmt = conn
                .prepare(
                    "SELECT symbol, side, qty, entry_price FROM positions WHERE mode=?1 AND status='open'",
                )
                .map_err(|error| ProviderError::InvalidData(error.to_string()))?;
            let rows = stmt
                .query_map([mode], |row| {
                    Ok(PositionSnapshot {
                        symbol: row.get(0)?,
                        side: row.get(1)?,
                        qty: row.get(2)?,
                        entry_price: row.get(3)?,
                    })
                })
                .map_err(|error| ProviderError::InvalidData(error.to_string()))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| ProviderError::InvalidData(error.to_string()))?
        };
        let open_orders = {
            let mut stmt = conn
                .prepare(
                    "SELECT id, symbol, client_order_id, side, order_type, status, qty, price, reduce_only \
                     FROM orders WHERE mode=?1 AND status NOT IN ('FILLED','CANCELED','REJECTED','EXPIRED')",
                )
                .map_err(|error| ProviderError::InvalidData(error.to_string()))?;
            let rows = stmt
                .query_map([mode], |row| {
                    Ok(OpenOrderSnapshot {
                        symbol: row.get(1)?,
                        order_id: row.get(0)?,
                        client_order_id: row.get(2)?,
                        side: row.get(3)?,
                        order_type: row.get(4)?,
                        status: row.get(5)?,
                        original_qty: row.get(6)?,
                        executed_qty: 0.0,
                        price: row.get(7)?,
                        stop_price: 0.0,
                        reduce_only: row.get::<_, i64>(8)? != 0,
                    })
                })
                .map_err(|error| ProviderError::InvalidData(error.to_string()))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| ProviderError::InvalidData(error.to_string()))?
        };
        Ok((
            AccountSnapshot {
                equity,
                wallet: equity,
                available_balance: (equity - margin_used).max(0.0),
            },
            positions,
            open_orders,
        ))
    }

    /// 回测历史行情源：环境域名与当前 provider 一致，避免回测取到主网数据。
    #[cfg(test)]
    pub fn history(&self) -> crate::history::BinanceHistory {
        crate::history::BinanceHistory::new(!self.base_url.contains("testnet"))
    }

    pub fn with_timeframe(mut self, timeframe: &str) -> Result<Self, ProviderError> {
        self.timeframe = match timeframe {
            "1m" | "3m" | "5m" => "15m",
            "15m" | "30m" | "1h" | "4h" => timeframe,
            _ => return Err(ProviderError::InvalidData("策略 K 线周期无效".into())),
        }
        .into();
        Ok(self)
    }

    /// 只拉交易所持仓，不拉行情/K 线。用于手动平仓后的快速对账。
    pub async fn positions_only(&self) -> Result<Vec<PositionSnapshot>, ProviderError> {
        let value = self.get_json("/fapi/v3/positionRisk", &[], true).await?;
        let rows = value
            .as_array()
            .ok_or_else(|| ProviderError::InvalidData("持仓响应不是数组".into()))?;
        let mut positions = Vec::new();
        for row in rows {
            if let Some(position) = parse_position(row)? {
                positions.push(position);
            }
        }
        Ok(positions)
    }

    /// 单个标的的最新标记价，用于补记手动平仓的出场价。
    pub async fn mark_price(&self, symbol: &str) -> Result<f64, ProviderError> {
        let params = vec![("symbol".to_string(), symbol.to_string())];
        let value = self
            .get_json("/fapi/v1/premiumIndex", &params, true)
            .await?;
        number(&value, "markPrice")
    }

    /// 公开标记价：不依赖 API 凭证，供账户面板的高频刷新使用。
    pub async fn public_mark_price(&self, symbol: &str) -> Result<f64, ProviderError> {
        let params = vec![("symbol".to_string(), symbol.to_string())];
        let value = self
            .get_json("/fapi/v1/premiumIndex", &params, false)
            .await?;
        number(&value, "markPrice")
    }

    /// 全市场 24 小时成交额，供扫描按流动性筛选标的。
    pub async fn tickers_24h(&self) -> Result<Vec<ExchangeTicker>, ProviderError> {
        let value = self.get_json("/fapi/v1/ticker/24hr", &[], false).await?;
        parse_tickers_24h(&value)
    }

    /// 全市场买一/卖一，供扫描估算点差成本。
    pub async fn book_tops(&self) -> Result<Vec<BookTop>, ProviderError> {
        let value = self
            .get_json("/fapi/v1/ticker/bookTicker", &[], false)
            .await?;
        parse_book_tops(&value)
    }

    /// 指定标的的已收盘 K 线，周期沿用 provider 的 timeframe。
    /// 只返回已收盘 K 线，条数可能少于 limit（未收盘的最新一根会被剔除）。
    pub async fn klines(
        &self,
        symbol: &str,
        limit: usize,
    ) -> Result<Vec<CandleSnapshot>, ProviderError> {
        if !(1..=1500).contains(&limit) {
            return Err(ProviderError::InvalidData(
                "K 线条数必须在 1..=1500 之间".into(),
            ));
        }
        let value = self
            .get_json(
                "/fapi/v1/klines",
                &[
                    ("symbol".into(), symbol.to_string()),
                    ("interval".into(), self.timeframe.clone()),
                    ("limit".into(), limit.to_string()),
                ],
                false,
            )
            .await?;
        parse_closed_candles(&value, crate::now_millis())
    }

    async fn get_json(
        &self,
        path: &str,
        params: &[(String, String)],
        signed: bool,
    ) -> Result<Value, ProviderError> {
        if signed && (self.api_key.trim().is_empty() || self.secret.trim().is_empty()) {
            return Err(ProviderError::Unavailable(
                "未配置交易所 API 凭证，公开 provider 不支持签名接口".into(),
            ));
        }
        let timestamp = crate::now_millis();
        let query = if signed {
            let refs: Vec<(&str, &str)> = params
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            crate::sign::signed_query(&refs, &self.secret, timestamp, 5_000).wire
        } else {
            params
                .iter()
                .map(|(key, value)| format!("{}={}", key, crate::sign::urlencode_component(value)))
                .collect::<Vec<_>>()
                .join("&")
        };
        let url = format!("{}{path}?{query}", self.base_url);
        let mut request = self.client.get(url);
        if signed {
            request = request.header("X-MBX-APIKEY", &self.api_key);
        }
        let transport_error = |error: reqwest::Error| {
            if error.is_timeout() {
                ProviderError::Timeout
            } else {
                ProviderError::Unavailable(format!("交易所请求传输失败 ({path})"))
            }
        };
        let response = request.send().await.map_err(transport_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(ProviderError::Unavailable(format!(
                "交易所请求失败 ({path}, HTTP {status})"
            )));
        }
        let body = response.bytes().await.map_err(transport_error)?;
        serde_json::from_slice(&body)
            .map_err(|_| ProviderError::InvalidData(format!("交易所响应不是合法 JSON ({path})")))
    }
}

impl SnapshotProvider for BinanceProvider {
    fn snapshot(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<TradingSnapshot, ProviderError>> + Send + '_>> {
        Box::pin(async move {
            let mut markets = Vec::with_capacity(self.symbols.len());
            for symbol in &self.symbols {
                let ticker = self
                    .get_json(
                        "/fapi/v1/premiumIndex",
                        &[("symbol".into(), symbol.clone())],
                        false,
                    )
                    .await?;
                let mark_price = number(&ticker, "markPrice")?;
                let timestamp = ticker["time"]
                    .as_i64()
                    .filter(|time| *time > 0)
                    .ok_or_else(|| ProviderError::InvalidData("标记价格时间无效".into()))?
                    / 1_000;
                if mark_price <= 0.0 || ticker["symbol"].as_str() != Some(symbol.as_str()) {
                    return Err(ProviderError::InvalidData(format!(
                        "{symbol} 标记价格或交易对无效"
                    )));
                }
                let quote = self
                    .get_json(
                        "/fapi/v1/ticker/24hr",
                        &[("symbol".into(), symbol.clone())],
                        false,
                    )
                    .await?;
                if quote["symbol"].as_str() != Some(symbol.as_str()) {
                    return Err(ProviderError::InvalidData(format!(
                        "{symbol} 24 小时成交额标的无效"
                    )));
                }
                let quote_volume = number(&quote, "quoteVolume")?;
                let funding_rate = number(&ticker, "lastFundingRate")?;
                let book = self
                    .get_json(
                        "/fapi/v1/ticker/bookTicker",
                        &[("symbol".into(), symbol.clone())],
                        false,
                    )
                    .await?;
                let bid = number(&book, "bidPrice")?;
                let ask = number(&book, "askPrice")?;
                if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= 0.0 || ask < bid {
                    return Err(ProviderError::InvalidData(format!(
                        "{symbol} 盘口买一/卖一无效"
                    )));
                }
                let mid = (bid + ask) / 2.0;
                let spread_bps = ((ask - bid) / mid * 10_000.0).max(0.0);
                let (open_interest, open_interest_change_pct) = match self
                    .get_json(
                        "/futures/data/openInterestHist",
                        &[
                            ("symbol".into(), symbol.clone()),
                            ("period".into(), self.timeframe.clone()),
                            ("limit".into(), "2".into()),
                        ],
                        false,
                    )
                    .await
                {
                    Ok(payload) => payload
                        .as_array()
                        .and_then(|rows| parse_open_interest_history(rows))
                        .unwrap_or((0.0, f64::NAN)),
                    Err(_) => (0.0, f64::NAN),
                };
                let klines = self
                    .get_json(
                        "/fapi/v1/klines",
                        &[
                            ("symbol".into(), symbol.clone()),
                            ("interval".into(), self.timeframe.clone()),
                            ("limit".into(), "250".into()),
                        ],
                        false,
                    )
                    .await?;
                let candles = parse_closed_candles(&klines, timestamp * 1_000)?;
                if quote_volume < 0.0 || candles.is_empty() {
                    return Err(ProviderError::InvalidData(format!(
                        "{symbol} 行情数据无效或 K 线不足"
                    )));
                }
                markets.push(MarketSnapshot {
                    symbol: symbol.clone(),
                    mark_price,
                    timestamp,
                    quote_volume,
                    funding_rate,
                    open_interest,
                    open_interest_change_pct,
                    bid,
                    ask,
                    spread_bps,
                    candles,
                });
            }
            if self.public_book.is_some() {
                let (account, positions, open_orders) = self.local_book("demo")?;
                return Ok(TradingSnapshot {
                    markets,
                    account,
                    positions,
                    open_orders,
                });
            }
            let account = self.get_json("/fapi/v3/account", &[], true).await?;
            let equity = number(&account, "totalMarginBalance")?;
            let available_balance = number(&account, "availableBalance")?;
            let wallet = number(&account, "totalWalletBalance").unwrap_or(equity);
            let positions_value = self.get_json("/fapi/v3/positionRisk", &[], true).await?;
            let rows = positions_value
                .as_array()
                .ok_or_else(|| ProviderError::InvalidData("持仓响应不是数组".into()))?;
            let mut positions = Vec::new();
            for row in rows {
                if let Some(position) = parse_position(row)? {
                    positions.push(position);
                }
            }
            let orders = self.get_json("/fapi/v1/openOrders", &[], true).await?;
            let open_orders = orders
                .as_array()
                .ok_or_else(|| ProviderError::InvalidData("未结订单响应不是数组".into()))?
                .iter()
                .map(parse_open_order)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(TradingSnapshot {
                markets,
                account: AccountSnapshot {
                    equity,
                    wallet,
                    available_balance,
                },
                positions,
                open_orders,
            })
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct CandleMetrics {
    pub atr: f64,
    pub atr_pct: f64,
    pub vol_ratio: f64,
}

/// Mirrors legacy 内核（已退役） `indicators.atr` and the rolling volume ratio used by strategy features.
pub fn candle_metrics(candles: &[CandleSnapshot], atr_period: usize) -> Option<CandleMetrics> {
    if atr_period == 0 || candles.len() <= atr_period || candles.len() < 20 {
        return None;
    }
    let start = candles.len() - atr_period - 1;
    let range_sum: f64 = candles[start..]
        .windows(2)
        .map(|pair| {
            let current = &pair[1];
            (current.high - current.low)
                .max((current.high - pair[0].close).abs())
                .max((current.low - pair[0].close).abs())
        })
        .sum();
    let atr = range_sum / atr_period as f64;
    let last = candles.last()?;
    let recent = &candles[candles.len() - 20..];
    let average_volume = recent
        .iter()
        .map(|candle| candle.volume / 20.0)
        .sum::<f64>();
    let atr_pct = atr / last.close * 100.0;
    let vol_ratio = if average_volume > 0.0 {
        last.volume / average_volume
    } else {
        0.0
    };
    if !atr_pct.is_finite() || !vol_ratio.is_finite() {
        return None;
    }
    let round4 = |value: f64| (value * 10_000.0).round_ties_even() / 10_000.0;
    Some(CandleMetrics {
        atr,
        atr_pct: round4(atr_pct),
        vol_ratio: round4(vol_ratio),
    })
}

fn parse_closed_candles(value: &Value, now_ms: i64) -> Result<Vec<CandleSnapshot>, ProviderError> {
    let rows = value
        .as_array()
        .ok_or_else(|| ProviderError::InvalidData("K 线响应不是数组".into()))?;
    let mut candles = Vec::with_capacity(rows.len());
    let mut previous_close_time = 0_i64;
    for row in rows {
        let fields = row
            .as_array()
            .ok_or_else(|| ProviderError::InvalidData("K 线行不是数组".into()))?;
        if fields.len() < 7 {
            return Err(ProviderError::InvalidData("K 线字段不足".into()));
        }
        let timestamp = |index: usize| {
            fields[index]
                .as_i64()
                .ok_or_else(|| ProviderError::InvalidData("K 线时间无效".into()))
        };
        let numeric = |index: usize| {
            fields[index]
                .as_str()
                .and_then(|value| value.parse::<f64>().ok())
                .or_else(|| fields[index].as_f64())
                .filter(|value| value.is_finite())
                .ok_or_else(|| ProviderError::InvalidData("K 线价量无效".into()))
        };
        let close_time = timestamp(6)?;
        if close_time >= now_ms {
            continue;
        }
        let open = numeric(1)?;
        let high = numeric(2)?;
        let low = numeric(3)?;
        let close = numeric(4)?;
        let volume = numeric(5)?;
        if timestamp(0)? >= close_time
            || close_time <= previous_close_time
            || low <= 0.0
            || high < low
            || open < low
            || open > high
            || close < low
            || close > high
            || volume < 0.0
        {
            return Err(ProviderError::InvalidData("K 线 OHLCV 或顺序无效".into()));
        }
        previous_close_time = close_time;
        candles.push(CandleSnapshot {
            close_time,
            open,
            high,
            low,
            close,
            volume,
        });
    }
    Ok(candles)
}

pub fn parse_tickers_24h(value: &Value) -> Result<Vec<ExchangeTicker>, ProviderError> {
    let rows = value
        .as_array()
        .ok_or_else(|| ProviderError::InvalidData("全市场 24 小时行情响应不是数组".into()))?;
    if rows.is_empty() {
        return Err(ProviderError::InvalidData(
            "全市场行情无有效行".into(),
        ));
    }
    let mut tickers = Vec::with_capacity(rows.len());
    let mut skipped = 0_usize;
    for row in rows {
        let Some(symbol) = row["symbol"]
            .as_str()
            .filter(|symbol| !symbol.is_empty())
        else {
            skipped += 1;
            continue;
        };
        // 单行缺字段/非有限/负成交额只跳过该行，不能让整批行情失败。
        let Ok(quote_volume) = number(row, "quoteVolume") else {
            skipped += 1;
            continue;
        };
        if !quote_volume.is_finite() || quote_volume < 0.0 {
            skipped += 1;
            continue;
        }
        tickers.push(ExchangeTicker {
            symbol: symbol.to_string(),
            quote_volume,
        });
    }
    if tickers.is_empty() {
        return Err(ProviderError::InvalidData(
            "全市场行情无有效行".into(),
        ));
    }
    if skipped > 0 {
        tracing::warn!(skipped, "24 小时行情存在非法行，已跳过");
    }
    Ok(tickers)
}

pub fn parse_book_tops(value: &Value) -> Result<Vec<BookTop>, ProviderError> {
    let rows = value
        .as_array()
        .ok_or_else(|| ProviderError::InvalidData("全市场盘口响应不是数组".into()))?;
    if rows.is_empty() {
        return Err(ProviderError::InvalidData(
            "全市场行情无有效行".into(),
        ));
    }
    let mut tops = Vec::with_capacity(rows.len());
    let mut skipped = 0_usize;
    for row in rows {
        let Some(symbol) = row["symbol"]
            .as_str()
            .filter(|symbol| !symbol.is_empty())
        else {
            skipped += 1;
            continue;
        };
        // 休市/异常标的常返回 0 或交叉盘口，逐行跳过而不是整批失败。
        let (Ok(bid), Ok(ask)) = (number(row, "bidPrice"), number(row, "askPrice")) else {
            skipped += 1;
            continue;
        };
        if !bid.is_finite()
            || !ask.is_finite()
            || bid <= 0.0
            || ask <= 0.0
            || ask < bid
        {
            skipped += 1;
            continue;
        }
        let mid = (bid + ask) / 2.0;
        let spread_bps = (ask - bid) / mid * 10_000.0;
        tops.push(BookTop {
            symbol: symbol.to_string(),
            bid,
            ask,
            spread_bps,
        });
    }
    if tops.is_empty() {
        return Err(ProviderError::InvalidData(
            "全市场行情无有效行".into(),
        ));
    }
    if skipped > 0 {
        tracing::warn!(skipped, "全市场盘口存在非法行，已跳过");
    }
    Ok(tops)
}

fn number(value: &Value, key: &str) -> Result<f64, ProviderError> {
    value[key]
        .as_str()
        .and_then(|v| v.parse().ok())
        .or_else(|| value[key].as_f64())
        .filter(|v: &f64| v.is_finite())
        .ok_or_else(|| ProviderError::InvalidData(format!("账户字段 {key} 无效")))
}

fn parse_open_interest_history(rows: &[Value]) -> Option<(f64, f64)> {
    let values: Vec<f64> = rows
        .iter()
        .filter_map(|row| {
            row.get("sumOpenInterest")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && *value > 0.0)
        })
        .collect();
    let last = *values.last()?;
    let previous = *values.get(values.len().checked_sub(2)?)?;
    Some((last, (last - previous) / previous * 100.0))
}

fn parse_open_order(value: &Value) -> Result<OpenOrderSnapshot, ProviderError> {
    let text = |key: &str| {
        value[key]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| ProviderError::InvalidData(format!("未结订单字段 {key} 无效")))
    };
    let order_id = value["orderId"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or_else(|| ProviderError::InvalidData("未结订单 ID 无效".into()))?;
    let side = text("side")?;
    let status = text("status")?;
    let original_qty = number(value, "origQty")?;
    let executed_qty = number(value, "executedQty")?;
    let price = number(value, "price")?;
    let stop_price = number(value, "stopPrice")?;
    if !matches!(side.as_str(), "BUY" | "SELL")
        || !matches!(status.as_str(), "NEW" | "PARTIALLY_FILLED")
        || original_qty <= 0.0
        || executed_qty < 0.0
        || executed_qty > original_qty
        || price < 0.0
        || stop_price < 0.0
        || !price.is_finite()
        || !stop_price.is_finite()
    {
        return Err(ProviderError::InvalidData(
            "未结订单方向、状态或价量无效".into(),
        ));
    }
    Ok(OpenOrderSnapshot {
        symbol: text("symbol")?,
        order_id,
        client_order_id: text("clientOrderId")?,
        side,
        order_type: text("type")?,
        status,
        original_qty,
        executed_qty,
        price,
        stop_price,
        reduce_only: value["reduceOnly"]
            .as_bool()
            .ok_or_else(|| ProviderError::InvalidData("未结订单 reduceOnly 无效".into()))?,
    })
}

fn parse_position(value: &Value) -> Result<Option<PositionSnapshot>, ProviderError> {
    let qty = number(value, "positionAmt")?;
    if qty == 0.0 {
        return Ok(None);
    }
    let symbol = value["symbol"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ProviderError::InvalidData("持仓缺少 symbol".into()))?;
    let entry_price = number(value, "entryPrice")?;
    if !entry_price.is_finite() || entry_price <= 0.0 {
        return Err(ProviderError::InvalidData("持仓开仓价无效".into()));
    }
    Ok(Some(PositionSnapshot {
        symbol: symbol.into(),
        side: if qty > 0.0 { "long" } else { "short" }.into(),
        qty: qty.abs(),
        entry_price,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn demo_provider_returns_deterministic_market_and_account_snapshot() {
        let provider = DemoProvider::new();
        let snapshot = provider.snapshot().await.unwrap();
        assert_eq!(snapshot.markets[0].symbol, "BTCUSDT");
        assert!(snapshot.markets[0].mark_price > 0.0);
        assert_eq!(snapshot.account.equity, 10_000.0);
        assert!(snapshot.positions.is_empty());
    }

    #[tokio::test]
    async fn demo_provider_exposes_sufficient_closed_candles_for_cycle() {
        let snapshot = DemoProvider::new().snapshot().await.unwrap();
        let market = &snapshot.markets[0];
        assert!(market.candles.len() >= 34);
        assert!(market
            .candles
            .windows(2)
            .all(|pair| pair[0].close_time < pair[1].close_time));
        assert!(market
            .candles
            .iter()
            .all(|candle| candle.close_time < market.timestamp * 1_000));
    }

    #[tokio::test]
    async fn provider_rejects_non_finite_snapshot_values() {
        let provider = DemoProvider::with_mark_price(f64::NAN);
        let error = provider.snapshot().await.unwrap_err();
        assert!(matches!(error, ProviderError::InvalidData(_)));
    }

    #[test]
    fn candle_metrics_use_previous_close_and_include_latest_volume() {
        let candle = |close, high, low, volume| CandleSnapshot {
            close_time: 0,
            open: close,
            high,
            low,
            close,
            volume,
        };
        let mut candles = vec![candle(100.0, 101.0, 99.0, 10.0); 19];
        candles.push(candle(110.0, 112.0, 108.0, 30.0));
        let metrics = candle_metrics(&candles, 2).unwrap();
        assert_eq!(metrics.atr_pct, 6.3636);
        assert_eq!(metrics.vol_ratio, 2.7273);
        assert!(candle_metrics(&candles[..2], 2).is_none());
    }

    #[test]
    fn binance_provider_requires_credentials() {
        assert!(matches!(
            BinanceProvider::new("https://example", "BTCUSDT", String::new(), "secret".into()),
            Err(ProviderError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn malformed_position_rejects_entire_snapshot() {
        let app = axum::Router::new()
            .route("/fapi/v1/premiumIndex", axum::routing::get(|uri: axum::http::Uri| async move {
                let symbol = uri.query().and_then(|query| query.strip_prefix("symbol=")).unwrap_or("unknown");
                axum::Json(serde_json::json!({"symbol":symbol,"markPrice":"100","lastFundingRate":"0.0001","time":crate::now_millis()}))
            }))
            .route("/fapi/v1/ticker/bookTicker", axum::routing::get(|| async {
                axum::Json(serde_json::json!({"symbol":"BTCUSDT","bidPrice":"99","askPrice":"101"}))
            }))
            .route("/futures/data/openInterestHist", axum::routing::get(|| async {
                axum::Json(serde_json::json!([
                    {"sumOpenInterest":"1000"},
                    {"sumOpenInterest":"1100"}
                ]))
            }))
            .route("/fapi/v1/ticker/24hr", axum::routing::get(|| async {
                axum::Json(serde_json::json!({"symbol":"BTCUSDT","quoteVolume":"5000000"}))
            }))
            .route("/fapi/v1/klines", axum::routing::get(|| async {
                axum::Json(serde_json::json!([[1_700_000_000_000_i64,"100","110","95","105","8",1_700_014_399_999_i64]]))
            }))
            .route("/fapi/v3/account", axum::routing::get(|| async {
                axum::Json(serde_json::json!({"totalMarginBalance":"125","availableBalance":"75"}))
            }))
            .route("/fapi/v3/positionRisk", axum::routing::get(|| async {
                axum::Json(serde_json::json!([{"symbol":"BTCUSDT","positionAmt":"invalid"}]))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = BinanceProvider::new(
            url,
            "BTCUSDT",
            "fixture-key".into(),
            "fixture-secret".into(),
        )
        .unwrap();
        let result = provider.snapshot().await;
        server.abort();
        assert!(
            matches!(result, Err(ProviderError::InvalidData(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn open_interest_failure_degrades_instead_of_failing_snapshot() {
        let app = axum::Router::new()
            .route(
                "/fapi/v1/premiumIndex",
                axum::routing::get(|uri: axum::http::Uri| async move {
                    let symbol = uri
                        .query()
                        .and_then(|query| query.strip_prefix("symbol="))
                        .unwrap_or("unknown");
                    axum::Json(serde_json::json!({"symbol":symbol,"markPrice":"100","lastFundingRate":"0.0001","time":crate::now_millis()}))
                }),
            )
            .route(
                "/fapi/v1/ticker/bookTicker",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({"symbol":"BTCUSDT","bidPrice":"99","askPrice":"101"}))
                }),
            )
            .route(
                "/futures/data/openInterestHist",
                axum::routing::get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .route(
                "/fapi/v1/ticker/24hr",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({"symbol":"BTCUSDT","quoteVolume":"5000000"}))
                }),
            )
            .route(
                "/fapi/v1/klines",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!([[1_700_000_000_000_i64,"100","110","95","105","8",1_700_014_399_999_i64]]))
                }),
            )
            .route(
                "/fapi/v3/account",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({"totalMarginBalance":"125","availableBalance":"75"}))
                }),
            )
            .route(
                "/fapi/v3/positionRisk",
                axum::routing::get(|| async { axum::Json(serde_json::json!([])) }),
            )
            .route(
                "/fapi/v1/openOrders",
                axum::routing::get(|| async { axum::Json(serde_json::json!([])) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = BinanceProvider::new(
            url,
            "BTCUSDT",
            "fixture-key".into(),
            "fixture-secret".into(),
        )
        .unwrap();
        let result = provider.snapshot().await;
        server.abort();
        let snapshot = result.expect("OI 失败不应拖垮整轮快照");
        let market = &snapshot.markets[0];
        assert_eq!(market.mark_price, 100.0);
        assert!(!market.candles.is_empty());
        assert_eq!(market.open_interest, 0.0);
        assert!(
            market.open_interest_change_pct.is_nan(),
            "OI 不可用必须以 NaN 标记，融合模型才能跳过该因子"
        );
    }

    #[tokio::test]
    async fn reads_multiple_markets_and_authenticates_account_requests() {
        use axum::{
            http::{HeaderMap, Uri},
            routing::get,
            Json, Router,
        };
        use serde_json::json;

        fn assert_signed(headers: &HeaderMap, uri: &Uri) {
            assert_eq!(headers["X-MBX-APIKEY"], "fixture-key");
            let (query, signature) = uri.query().unwrap().rsplit_once("&signature=").unwrap();
            assert!(query.starts_with("timestamp="));
            assert!(query.ends_with("&recvWindow=5000"));
            assert_eq!(signature, crate::sign::sign_hmac("fixture-secret", query));
        }

        let app = Router::new()
            .route(
                "/fapi/v1/premiumIndex",
                get(|headers: HeaderMap, uri: Uri| async move {
                    assert!(!headers.contains_key("X-MBX-APIKEY"));
                    let (symbol, mark) = match uri.query() {
                        Some("symbol=BTCUSDT") => ("BTCUSDT", "100"),
                        Some("symbol=ETHUSDT") => ("ETHUSDT", "50"),
                        _ => panic!("unexpected trading symbol"),
                    };
                    Json(json!({"symbol":symbol,"markPrice":mark,"lastFundingRate":"0.001","time":1_700_001_800_000_i64}))
                }),
            )
            .route(
                "/fapi/v1/ticker/bookTicker",
                get(|uri: Uri| async move {
                    let symbol = uri.query().unwrap().strip_prefix("symbol=").unwrap();
                    let (bid, ask) = match symbol {
                        "BTCUSDT" => ("99.99", "100.01"),
                        "ETHUSDT" => ("49.995", "50.005"),
                        _ => panic!("unexpected book symbol"),
                    };
                    Json(json!({"symbol":symbol,"bidPrice":bid,"askPrice":ask}))
                }),
            )
            .route(
                "/fapi/v1/openInterest",
                get(|uri: Uri| async move {
                    let symbol = uri.query().unwrap().strip_prefix("symbol=").unwrap();
                    Json(json!({"symbol":symbol,"openInterest":"1000"}))
                }),
            )
            .route(
                "/futures/data/openInterestHist",
                get(|uri: Uri| async move {
                    assert!(
                        uri.query().unwrap().contains("period=15m"),
                        "OI 历史周期必须与 K 线周期一致"
                    );
                    assert!(uri.query().unwrap().contains("limit=2"));
                    Json(json!([
                        {"sumOpenInterest":"1000"},
                        {"sumOpenInterest":"1100"}
                    ]))
                }),
            )
            .route("/fapi/v1/klines", get(|headers: HeaderMap, uri: Uri| async move {
                assert!(!headers.contains_key("X-MBX-APIKEY"));
                assert!(matches!(uri.query(), Some("symbol=BTCUSDT&interval=15m&limit=250" | "symbol=ETHUSDT&interval=15m&limit=250")));
                Json(json!([
                    [1_700_000_900_000_i64,"100","110","95","105","8",1_700_001_799_999_i64],
                    [1_700_001_800_000_i64,"105","999","100","990","999",1_700_002_699_999_i64]
                ]))
            }))
            .route("/fapi/v1/ticker/24hr", get(|headers: HeaderMap, uri: Uri| async move {
                assert!(!headers.contains_key("X-MBX-APIKEY"));
                let symbol = uri.query().unwrap().strip_prefix("symbol=").unwrap();
                Json(json!({"symbol":symbol,"quoteVolume":"5000000"}))
            }))
            .route(
                "/fapi/v3/account",
                get(|headers: HeaderMap, uri: Uri| async move {
                    assert_signed(&headers, &uri);
                    Json(json!({"totalMarginBalance":"125","availableBalance":"75"}))
                }),
            )
            .route(
                "/fapi/v3/positionRisk",
                get(|headers: HeaderMap, uri: Uri| async move {
                    assert_signed(&headers, &uri);
                    Json(json!([]))
                }),
            )
            .route(
                "/fapi/v1/openOrders",
                get(|headers: HeaderMap, uri: Uri| async move {
                    assert_signed(&headers, &uri);
                    Json(json!([{"symbol":"ETHUSDT","orderId":15,"clientOrderId":"fixture-order","side":"SELL","type":"LIMIT","status":"PARTIALLY_FILLED","origQty":"2","executedQty":"1","price":"60","stopPrice":"0","reduceOnly":true}]))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = BinanceProvider::with_symbols(
            url,
            vec!["BTCUSDT".into(), "ETHUSDT".into()],
            "fixture-key".into(),
            "fixture-secret".into(),
        )
        .unwrap()
        .with_timeframe("15m")
        .unwrap();
        let result = provider.snapshot().await;
        server.abort();
        let snapshot = result.unwrap();
        let market = serde_json::to_value(&snapshot.markets[0]).unwrap();
        assert_eq!(market["quote_volume"], 5_000_000.0);
        assert_eq!(market["candles"].as_array().unwrap().len(), 1);
        assert_eq!(market["candles"][0]["close"], 105.0);
        assert_eq!(
            snapshot.markets[0].open_interest_change_pct, 10.0,
            "新建 provider 也必须从 OI 历史算出变化，不能依赖进程内上一轮缓存"
        );
        assert_eq!(
            snapshot
                .markets
                .iter()
                .map(|m| (m.symbol.as_str(), m.mark_price))
                .collect::<Vec<_>>(),
            vec![("BTCUSDT", 100.0), ("ETHUSDT", 50.0)]
        );
        assert_eq!(
            snapshot.account,
            AccountSnapshot {
                equity: 125.0,
                wallet: 125.0,
                available_balance: 75.0
            }
        );
        assert!(snapshot.positions.is_empty());
        assert_eq!(snapshot.open_orders.len(), 1);
        assert_eq!(snapshot.open_orders[0].order_id, 15);
        assert_eq!(snapshot.open_orders[0].status, "PARTIALLY_FILLED");
        assert_eq!(snapshot.open_orders[0].executed_qty, 1.0);
        assert!(snapshot.open_orders[0].reduce_only);
    }

    #[test]
    fn parses_signed_numeric_fields_and_ignores_zero_positions() {
        let account = serde_json::json!({"totalMarginBalance":"12.5","availableBalance":"8.25"});
        assert_eq!(number(&account, "totalMarginBalance").unwrap(), 12.5);
        assert_eq!(
            parse_position(
                &serde_json::json!({"symbol":"BTCUSDT","positionAmt":"0","entryPrice":"1"})
            )
            .unwrap(),
            None
        );
        let position = parse_position(
            &serde_json::json!({"symbol":"BTCUSDT","positionAmt":"-2","entryPrice":"100"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(position.side, "short");
        assert_eq!(position.qty, 2.0);
    }

    #[test]
    fn parses_full_market_tickers_and_book_tops() {
        let tickers = parse_tickers_24h(&serde_json::json!([
            {"symbol": "BTCUSDT", "quoteVolume": "12345.6"}
        ]))
        .unwrap();
        assert_eq!(tickers[0].symbol, "BTCUSDT");
        assert_eq!(tickers[0].quote_volume, 12345.6);

        let tops = parse_book_tops(&serde_json::json!([
            {"symbol": "BTCUSDT", "bidPrice": "100", "askPrice": "101"}
        ]))
        .unwrap();
        assert_eq!(tops[0].symbol, "BTCUSDT");
        assert!((tops[0].spread_bps - 99.0).abs() < 1.0, "spread 应为约 99bp");
    }

    #[test]
    fn full_market_parsers_reject_non_array_payloads() {
        for payload in [serde_json::json!({"symbol": "BTCUSDT"}), serde_json::json!("oops")] {
            assert!(
                matches!(
                    parse_tickers_24h(&payload),
                    Err(ProviderError::InvalidData(_))
                ),
                "24 小时行情非数组必须拒绝: {payload:?}"
            );
            assert!(
                matches!(
                    parse_book_tops(&payload),
                    Err(ProviderError::InvalidData(_))
                ),
                "盘口行情非数组必须拒绝: {payload:?}"
            );
        }
    }

    #[test]
    fn full_market_parsers_skip_invalid_rows_and_keep_valid_ones() {
        let tickers = parse_tickers_24h(&serde_json::json!([
            {"symbol": "BTCUSDT", "quoteVolume": "12345.6"},
            {"quoteVolume": "1"},
            {"symbol": "", "quoteVolume": "1"},
            {"symbol": "NEGUSDT", "quoteVolume": "-1"},
            {"symbol": "NANUSDT", "quoteVolume": "NaN"}
        ]))
        .unwrap();
        assert_eq!(tickers.len(), 1, "非法行应跳过，仅保留合法行");
        assert_eq!(tickers[0].symbol, "BTCUSDT");
        assert_eq!(tickers[0].quote_volume, 12345.6);

        let tops = parse_book_tops(&serde_json::json!([
            {"symbol": "BTCUSDT", "bidPrice": "100", "askPrice": "101"},
            {"bidPrice": "100", "askPrice": "101"},
            {"symbol": "", "bidPrice": "100", "askPrice": "101"},
            {"symbol": "CROSSUSDT", "bidPrice": "101", "askPrice": "100"},
            {"symbol": "ZEROUSDT", "bidPrice": "0", "askPrice": "1"},
            {"symbol": "NEGUSDT", "bidPrice": "-1", "askPrice": "1"}
        ]))
        .unwrap();
        assert_eq!(tops.len(), 1, "非法盘口应跳过，仅保留合法行");
        assert_eq!(tops[0].symbol, "BTCUSDT");
        assert!((tops[0].spread_bps - 99.0).abs() < 1.0, "spread 应为约 99bp");
    }

    #[test]
    fn full_market_parsers_reject_payloads_without_valid_rows() {
        for payload in [
            serde_json::json!([]),
            serde_json::json!([{"quoteVolume": "1"}]),
            serde_json::json!([{"symbol": "", "quoteVolume": "1"}]),
            serde_json::json!([{"symbol": "BTCUSDT"}]),
            serde_json::json!([{"symbol": "BTCUSDT", "quoteVolume": "-1"}]),
        ] {
            assert!(
                matches!(
                    parse_tickers_24h(&payload),
                    Err(ProviderError::InvalidData(_))
                ),
                "24 小时行情无有效行必须拒绝: {payload:?}"
            );
        }

        for payload in [
            serde_json::json!([]),
            serde_json::json!([{"bidPrice": "100", "askPrice": "101"}]),
            serde_json::json!([{"symbol": "", "bidPrice": "100", "askPrice": "101"}]),
            serde_json::json!([{"symbol": "BTCUSDT", "askPrice": "101"}]),
            serde_json::json!([{"symbol": "BTCUSDT", "bidPrice": "100"}]),
            serde_json::json!([{"symbol": "BTCUSDT", "bidPrice": "101", "askPrice": "100"}]),
            serde_json::json!([{"symbol": "BTCUSDT", "bidPrice": "0", "askPrice": "1"}]),
            serde_json::json!([{"symbol": "BTCUSDT", "bidPrice": "1", "askPrice": "0"}]),
        ] {
            assert!(
                matches!(
                    parse_book_tops(&payload),
                    Err(ProviderError::InvalidData(_))
                ),
                "盘口行情无有效行必须拒绝: {payload:?}"
            );
        }
    }

    #[tokio::test]
    async fn klines_rejects_limit_out_of_range() {
        let provider = BinanceProvider::public("http://127.0.0.1:1").unwrap();
        for limit in [0_usize, 1_501] {
            assert!(
                matches!(
                    provider.klines("BTCUSDT", limit).await,
                    Err(ProviderError::InvalidData(_))
                ),
                "K 线条数 {limit} 越界必须拒绝"
            );
        }
    }

    #[tokio::test]
    async fn public_provider_rejects_signed_endpoints() {
        let provider = BinanceProvider::public("http://127.0.0.1:1").unwrap();
        for error in [
            provider.positions_only().await.unwrap_err(),
            provider.mark_price("BTCUSDT").await.unwrap_err(),
        ] {
            match error {
                ProviderError::Unavailable(message) => assert!(
                    message.contains("未配置交易所 API 凭证"),
                    "签名接口应提示缺少凭证: {message}"
                ),
                other => panic!("公开 provider 调用签名接口应为 Unavailable: {other:?}"),
            }
        }
    }

    #[test]
    fn history_source_follows_the_provider_environment() {
        let testnet = BinanceProvider::with_symbols(
            "https://testnet.binancefuture.com",
            vec!["BTCUSDT".to_string()],
            "key".into(),
            "secret".into(),
        )
        .unwrap();
        assert!(!testnet.history().is_live(), "demo 回测必须取测试网行情");

        let live = BinanceProvider::with_symbols(
            "https://fapi.binance.com",
            vec!["BTCUSDT".to_string()],
            "key".into(),
            "secret".into(),
        )
        .unwrap();
        assert!(live.history().is_live(), "live 回测应取主网历史行情");
    }
}
