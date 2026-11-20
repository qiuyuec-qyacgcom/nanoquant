//! AI 分析与决策预览所需的行情来源。
//!
//! 与引擎共用 `BinanceProvider`，按当前交易环境选测试网/主网域名；
//! 测试用 `AiMarketSource` 的确定性实现替换，不触网。

use serde_json::Value;

use crate::ai_client::BoxFuture;
use crate::db::Db;
use crate::providers::{BinanceProvider, SnapshotProvider, TradingSnapshot};

pub trait AiMarketSource: Send + Sync {
    fn snapshot<'a>(
        &'a self,
        symbol: &'a str,
        interval: &'a str,
    ) -> BoxFuture<'a, Result<TradingSnapshot, String>>;
}

pub struct BinanceMarketSource {
    db: Db,
}

impl BinanceMarketSource {
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    fn mode(&self) -> String {
        self.db
            .conn
            .lock()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT value FROM engine_state WHERE key = 'mode'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            })
            .unwrap_or_else(|| "demo".to_string())
    }

    fn data_config(&self) -> Value {
        self.db
            .setting("data")
            .ok()
            .flatten()
            .unwrap_or(Value::Null)
    }
}

impl AiMarketSource for BinanceMarketSource {
    fn snapshot<'a>(
        &'a self,
        symbol: &'a str,
        interval: &'a str,
    ) -> BoxFuture<'a, Result<TradingSnapshot, String>> {
        Box::pin(async move {
            let data = self.data_config();
            let mode = self.mode();
            let default_base = if mode == "live" {
                "https://fapi.binance.com"
            } else {
                "https://testnet.binancefuture.com"
            };
            let base = data
                .get(if mode == "live" {
                    "live_base_url"
                } else {
                    "demo_base_url"
                })
                .and_then(Value::as_str)
                .filter(|url| !url.trim().is_empty())
                .unwrap_or(default_base);
            let provider = BinanceProvider::with_symbols_public(
                base,
                vec![symbol.to_uppercase()],
                self.db.clone(),
            )
            .map_err(|error| format!("{error:?}"))?
            .with_timeframe(interval)
            .map_err(|error| format!("{error:?}"))?;
            let snapshot = provider
                .snapshot()
                .await
                .map_err(|error| format!("{error:?}"))?;
            if snapshot
                .markets
                .iter()
                .all(|market| market.symbol != symbol.to_uppercase())
            {
                return Err(format!("{symbol} 行情不可用"));
            }
            Ok(snapshot)
        })
    }
}

/// 未配置行情来源时的明确失败，不伪造行情。
pub struct UnavailableMarket;

impl AiMarketSource for UnavailableMarket {
    fn snapshot<'a>(
        &'a self,
        _symbol: &'a str,
        _interval: &'a str,
    ) -> BoxFuture<'a, Result<TradingSnapshot, String>> {
        Box::pin(async move { Err("行情来源未配置".to_string()) })
    }
}
