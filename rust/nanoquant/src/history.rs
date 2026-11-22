//! 历史行情数据源：币安 K 线分页、限频、缓存与资金费。
//!
//! 回测只接受真实历史数据：分页完成或明确标记数据不足；相邻 K 线时间差超过
//! 一个周期时记录缺口并原样跳过，绝不插值。资金费单独拉取，失败只标记，
//! 不影响 K 线本身的可用性。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::kline_stream::BoxFuture;

/// 单次回测允许的最大 K 线数量，超过即视为区间过长。
pub const MAX_BARS: usize = 30_000;
/// 币安单页上限。
pub const PAGE_LIMIT: usize = 1_500;
/// 服务端最多翻页数，避免一次请求把上游拖死。
pub const MAX_PAGES: usize = 20;
/// 历史数据缓存时间。
const CACHE_TTL: Duration = Duration::from_secs(60);
const CACHE_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistoryBar {
    pub open_time: i64,
    pub close_time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Gap {
    pub from_ts: i64,
    pub to_ts: i64,
    pub missing_bars: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FundingPoint {
    pub ts: i64,
    pub rate: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HistorySeries {
    pub bars: Vec<HistoryBar>,
    pub gaps: Vec<Gap>,
    pub funding: Vec<FundingPoint>,
    pub truncated: bool,
    pub funding_error: String,
}

pub trait HistorySource: Send + Sync {
    fn load(
        &self,
        symbol: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> BoxFuture<'_, Result<HistorySeries, String>>;
}

pub fn interval_ms(interval: &str) -> Option<i64> {
    Some(match interval {
        "1m" => 60_000,
        "3m" => 180_000,
        "5m" => 300_000,
        "15m" => 900_000,
        "30m" => 1_800_000,
        "1h" => 3_600_000,
        "2h" => 7_200_000,
        "4h" => 14_400_000,
        "6h" => 21_600_000,
        "8h" => 28_800_000,
        "12h" => 43_200_000,
        "1d" => 86_400_000,
        _ => return None,
    })
}

fn number(row: &Value, index: usize) -> Result<f64, String> {
    row.get(index)
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("K 线第 {} 列不是有效数字", index + 1))
}

fn integer(row: &Value, index: usize) -> Result<i64, String> {
    row.get(index)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("K 线第 {} 列不是有效时间戳", index + 1))
}

/// 解析币安 K 线数组。任何一列非法都整页失败，静默丢行会让净值曲线少一段。
pub fn parse_bars(rows: &[Value]) -> Result<Vec<HistoryBar>, String> {
    let mut bars = Vec::with_capacity(rows.len());
    for row in rows {
        bars.push(HistoryBar {
            open_time: integer(row, 0)?,
            open: number(row, 1)?,
            high: number(row, 2)?,
            low: number(row, 3)?,
            close: number(row, 4)?,
            volume: number(row, 5)?,
            close_time: integer(row, 6)?,
        });
    }
    for pair in bars.windows(2) {
        if pair[1].open_time <= pair[0].open_time {
            return Err("K 线时间未严格递增，拒绝用乱序数据回测".to_string());
        }
    }
    Ok(bars)
}

/// 缺口只标注不补齐：`missing_bars` 是两段之间缺失的整根数量。
pub fn detect_gaps(bars: &[HistoryBar], step_ms: i64) -> Vec<Gap> {
    if step_ms <= 0 {
        return Vec::new();
    }
    let mut gaps = Vec::new();
    for pair in bars.windows(2) {
        let diff = pair[1].open_time - pair[0].open_time;
        if diff > step_ms {
            gaps.push(Gap {
                from_ts: pair[0].close_time,
                to_ts: pair[1].open_time,
                missing_bars: diff / step_ms - 1,
            });
        }
    }
    gaps
}

fn host(live: bool) -> &'static str {
    if live {
        "fapi.binance.com"
    } else {
        "testnet.binancefuture.com"
    }
}

pub fn klines_url(live: bool) -> String {
    format!("https://{}/fapi/v1/klines", host(live))
}

pub fn funding_url(live: bool) -> String {
    format!("https://{}/fapi/v1/fundingRate", host(live))
}

#[derive(Clone)]
pub struct BinanceHistory {
    live: bool,
    client: Result<reqwest::Client, String>,
    cache: Arc<Mutex<HashMap<(String, String, i64, i64), (Instant, HistorySeries)>>>,
}

impl BinanceHistory {
    pub fn new(live: bool) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| error.to_string());
        Self {
            live,
            client,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 当前实例是否连实盘；尚未接入调用点，接线后取消豁免。
    #[allow(dead_code)]
    pub fn is_live(&self) -> bool {
        self.live
    }

    fn cached(&self, key: &(String, String, i64, i64)) -> Option<HistorySeries> {
        let cache = self.cache.lock().expect("history cache");
        cache
            .get(key)
            .filter(|(at, _)| at.elapsed() < CACHE_TTL)
            .map(|(_, series)| series.clone())
    }

    fn remember(&self, key: (String, String, i64, i64), series: &HistorySeries) {
        let mut cache = self.cache.lock().expect("history cache");
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        if cache.len() >= CACHE_CAPACITY {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(key, (Instant::now(), series.clone()));
    }

    async fn fetch_funding(
        &self,
        client: &reqwest::Client,
        symbol: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<FundingPoint>, String> {
        let mut points = Vec::new();
        let mut cursor = start_ms;
        for _ in 0..MAX_PAGES {
            let rows = client
                .get(funding_url(self.live))
                .query(&[
                    ("symbol", symbol),
                    ("startTime", &cursor.to_string()),
                    ("endTime", &end_ms.to_string()),
                    ("limit", "1000"),
                ])
                .send()
                .await
                .and_then(|response| response.error_for_status())
                .map_err(|error| error.to_string())?
                .json::<Vec<Value>>()
                .await
                .map_err(|error| error.to_string())?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                let ts = row["fundingTime"].as_i64().unwrap_or_default();
                let rate = row["fundingRate"]
                    .as_str()
                    .and_then(|text| text.parse::<f64>().ok())
                    .unwrap_or(0.0);
                points.push(FundingPoint { ts, rate });
            }
            let last = points.last().map(|point| point.ts).unwrap_or(cursor);
            if last <= cursor || rows.len() < 1000 {
                break;
            }
            cursor = last + 1;
        }
        Ok(points)
    }
}

impl HistorySource for BinanceHistory {
    fn load(
        &self,
        symbol: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> BoxFuture<'_, Result<HistorySeries, String>> {
        let symbol = symbol.to_string();
        let interval = interval.to_string();
        Box::pin(async move {
            let step =
                interval_ms(&interval).ok_or_else(|| format!("不支持的历史周期: {interval}"))?;
            if end_ms <= start_ms {
                return Err("历史区间无效: end 必须晚于 start".to_string());
            }
            let key = (symbol.clone(), interval.clone(), start_ms, end_ms);
            if let Some(series) = self.cached(&key) {
                return Ok(series);
            }
            let client = self
                .client
                .as_ref()
                .map_err(|error| format!("历史行情客户端不可用: {error}"))?;
            let mut bars: Vec<HistoryBar> = Vec::new();
            let mut cursor = start_ms;
            let mut truncated = false;
            for page_index in 0..MAX_PAGES {
                let rows = client
                    .get(klines_url(self.live))
                    .query(&[
                        ("symbol", symbol.as_str()),
                        ("interval", interval.as_str()),
                        ("startTime", &cursor.to_string()),
                        ("endTime", &end_ms.to_string()),
                        ("limit", &PAGE_LIMIT.to_string()),
                    ])
                    .send()
                    .await
                    .and_then(|response| response.error_for_status())
                    .map_err(|error| format!("历史 K 线请求失败: {error}"))?
                    .json::<Vec<Value>>()
                    .await
                    .map_err(|error| format!("历史 K 线响应无效: {error}"))?;
                if rows.is_empty() {
                    break;
                }
                let page = parse_bars(&rows)?;
                let last = page.last().map(|bar| bar.open_time).unwrap_or(cursor);
                tracing::info!(
                    symbol = %symbol,
                    interval = %interval,
                    page = page_index + 1,
                    cursor,
                    rows = rows.len(),
                    last,
                    "历史 K 线分页"
                );
                let cutoff = bars.last().map(|bar| bar.open_time).unwrap_or(i64::MIN);
                let fresh: Vec<HistoryBar> = page
                    .into_iter()
                    .filter(|bar| bar.open_time > cutoff)
                    .collect();
                if fresh.is_empty() {
                    break;
                }
                bars.extend(fresh);
                if bars.len() >= MAX_BARS {
                    truncated = true;
                    break;
                }
                if last < cursor {
                    break;
                }
                cursor = last + step;
                if cursor >= end_ms {
                    break;
                }
            }
            if bars.len() >= MAX_BARS && !truncated {
                truncated = true;
            }
            let gaps = detect_gaps(&bars, step);
            let (funding, funding_error) =
                match self.fetch_funding(client, &symbol, start_ms, end_ms).await {
                    Ok(points) => (points, String::new()),
                    Err(error) => (Vec::new(), error),
                };
            let series = HistorySeries {
                bars,
                gaps,
                funding,
                truncated,
                funding_error,
            };
            self.remember(key, &series);
            Ok(series)
        })
    }
}

/// 任意数据源前面的 TTL 缓存，测试与回测任务共用。
#[cfg(test)]
pub struct CachedHistory<S> {
    inner: S,
    ttl: Duration,
    cache: Mutex<HashMap<(String, String, i64, i64), (Instant, HistorySeries)>>,
}

#[cfg(test)]
impl<S> CachedHistory<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            ttl: CACHE_TTL,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[cfg(test)]
impl<S: HistorySource> HistorySource for CachedHistory<S> {
    fn load(
        &self,
        symbol: &str,
        interval: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> BoxFuture<'_, Result<HistorySeries, String>> {
        let symbol = symbol.to_string();
        let interval = interval.to_string();
        Box::pin(async move {
            let key = (symbol.clone(), interval.clone(), start_ms, end_ms);
            if let Some(series) = self
                .cache
                .lock()
                .expect("cache")
                .get(&key)
                .filter(|(at, _)| at.elapsed() < self.ttl)
                .map(|(_, series)| series.clone())
            {
                return Ok(series);
            }
            let series = self
                .inner
                .load(&symbol, &interval, start_ms, end_ms)
                .await?;
            self.cache
                .lock()
                .expect("cache")
                .insert(key, (Instant::now(), series.clone()));
            Ok(series)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    fn row(open_time: i64, open: &str, close: &str) -> Value {
        json!([
            open_time,
            open,
            "110.0",
            "90.0",
            close,
            "12.5",
            open_time + 899_999,
        ])
    }

    #[test]
    fn parses_real_binance_rows_and_rejects_bad_ones() {
        let bars = parse_bars(&[
            row(1_700_000_000_000, "100.0", "101.0"),
            row(1_700_000_900_000, "101.0", "102.0"),
        ])
        .expect("合法 K 线应解析");
        assert_eq!(bars.len(), 2);
        assert_eq!(bars[0].close_time, 1_700_000_899_999);
        assert!(parse_bars(&[json!([1, "bad", "1", "1", "1", "1", 2])]).is_err());
        assert!(
            parse_bars(&[row(2, "1", "1"), row(1, "1", "1")]).is_err(),
            "乱序数据必须拒绝"
        );
    }

    #[test]
    fn gaps_are_marked_but_never_interpolated() {
        let bars = parse_bars(&[
            row(0, "1", "1"),
            row(900_000, "1", "1"),
            row(3_600_000, "1", "1"),
        ])
        .unwrap();
        let gaps = detect_gaps(&bars, 900_000);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].missing_bars, 2, "缺 1.5h/0.9h-1 = 2 根");
        assert_eq!(bars.len(), 3, "缺口不插值，原始长度不变");
    }

    struct CountingSource {
        calls: StdArc<AtomicUsize>,
    }

    impl HistorySource for CountingSource {
        fn load(
            &self,
            _symbol: &str,
            _interval: &str,
            _start_ms: i64,
            _end_ms: i64,
        ) -> BoxFuture<'_, Result<HistorySeries, String>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(HistorySeries {
                    bars: parse_bars(&[row(0, "1", "1")]).unwrap(),
                    ..HistorySeries::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn cache_reuses_the_same_window() {
        let calls = StdArc::new(AtomicUsize::new(0));
        let source = CachedHistory::new(CountingSource {
            calls: calls.clone(),
        });
        let first = source.load("BTCUSDT", "15m", 0, 900_000).await.unwrap();
        let second = source.load("BTCUSDT", "15m", 0, 900_000).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "同窗口第二次应命中缓存");
        source.load("BTCUSDT", "15m", 0, 1_800_000).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "不同窗口要重新取数");
    }

    #[test]
    fn urls_follow_the_trading_environment() {
        assert!(klines_url(false).contains("testnet.binancefuture.com"));
        assert!(klines_url(true).contains("fapi.binance.com"));
        assert!(funding_url(true).ends_with("/fapi/v1/fundingRate"));
        assert_eq!(interval_ms("15m"), Some(900_000));
        assert_eq!(interval_ms("3m"), Some(180_000));
    }
}
