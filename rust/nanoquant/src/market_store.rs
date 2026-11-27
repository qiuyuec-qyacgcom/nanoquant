//! 行情留存：把引擎每轮已经拉到的收盘 K 线落库，供离线回放与复盘回看。
//!
//! 本模块只做存取，不含任何业务判断；取数频率由调用方决定。
use rusqlite::{params, Connection};

use crate::providers::{CandleSnapshot, MarketSnapshot};

pub(crate) const CREATE_TABLE: &str = "CREATE TABLE IF NOT EXISTS market_candles(\
    symbol TEXT NOT NULL,\
    interval TEXT NOT NULL,\
    close_time INTEGER NOT NULL,\
    open FLOAT NOT NULL,\
    high FLOAT NOT NULL,\
    low FLOAT NOT NULL,\
    close FLOAT NOT NULL,\
    volume FLOAT NOT NULL,\
    PRIMARY KEY(symbol, interval, close_time)\
)";

pub(crate) fn ensure_table(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(CREATE_TABLE)
        .map_err(|e| format!("建表 market_candles 失败: {e}"))?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS ix_market_candles_symbol_interval \
         ON market_candles(symbol, interval, close_time)",
    )
    .map_err(|e| format!("建索引 market_candles 失败: {e}"))?;
    Ok(())
}

/// 写入一轮快照里的 K 线。同一根 K 线重复写入是幂等的（覆盖写）。
///
/// `conn` 可以是裸连接（每条语句各自提交），也可以是 `&Transaction` 的解引用
/// （此时写入并入外层事务）。
pub(crate) fn upsert_candles(
    conn: &Connection,
    interval: &str,
    markets: &[MarketSnapshot],
) -> Result<usize, String> {
    // 幂等 DDL：测试夹具与迁移前的老库都不必额外改表结构。
    ensure_table(conn)?;
    let mut stmt = conn
        .prepare(
            "INSERT INTO market_candles(symbol, interval, close_time, open, high, low, close, volume) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8) \
             ON CONFLICT(symbol, interval, close_time) DO UPDATE SET \
               open=excluded.open, high=excluded.high, low=excluded.low, \
               close=excluded.close, volume=excluded.volume",
        )
        .map_err(|e| format!("准备写入行情失败: {e}"))?;
    let mut written = 0usize;
    for market in markets {
        for candle in &market.candles {
            stmt.execute(params![
                market.symbol,
                interval,
                candle.close_time,
                candle.open,
                candle.high,
                candle.low,
                candle.close,
                candle.volume,
            ])
            .map_err(|e| format!("写入 {} 行情失败: {e}", market.symbol))?;
            written += 1;
        }
    }
    Ok(written)
}

/// 读回某个标的某个周期的 K 线，按 close_time 升序，最多 `limit` 根。
pub(crate) fn load_candles(
    conn: &Connection,
    symbol: &str,
    interval: &str,
    limit: usize,
) -> Result<Vec<CandleSnapshot>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT close_time, open, high, low, close, volume FROM market_candles \
             WHERE symbol=?1 AND interval=?2 ORDER BY close_time DESC LIMIT ?3",
        )
        .map_err(|e| format!("准备读取行情失败: {e}"))?;
    let rows = stmt
        .query_map(params![symbol, interval, limit as i64], |r| {
            Ok(CandleSnapshot {
                close_time: r.get(0)?,
                open: r.get(1)?,
                high: r.get(2)?,
                low: r.get(3)?,
                close: r.get(4)?,
                volume: r.get(5)?,
            })
        })
        .map_err(|e| format!("读取行情失败: {e}"))?;
    let mut out: Vec<CandleSnapshot> = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("解析行情失败: {e}"))?;
    out.reverse();
    Ok(out)
}

/// 删除早于 `cutoff_ms` 的行，返回删除行数。
pub(crate) fn purge_before(conn: &Connection, cutoff_ms: i64) -> Result<usize, String> {
    conn.execute(
        "DELETE FROM market_candles WHERE close_time < ?1",
        params![cutoff_ms],
    )
    .map_err(|e| format!("清理过期行情失败: {e}"))?;
    Ok(conn.changes() as usize)
}

/// 把库里的收盘 K 线转回 `HistoryBar`；`open_time` 由 `close_time` 与周期反推。
pub(crate) fn load_history_bar(
    conn: &Connection,
    symbol: &str,
    interval: &str,
    start_ms: i64,
    end_ms: i64,
    limit: usize,
) -> Vec<crate::history::HistoryBar> {
    let step = crate::history::interval_ms(interval).unwrap_or(60_000);
    load_candles(conn, symbol, interval, limit)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.close_time >= start_ms && c.close_time <= end_ms)
        .map(|c| crate::history::HistoryBar {
            open_time: c.close_time - step + 1,
            close_time: c.close_time,
            open: c.open,
            high: c.high,
            low: c.low,
            close: c.close,
            volume: c.volume,
        })
        .collect()
}

/// 行情保留期清理任务：每 6 小时一次，删掉超过 `data.keep_days` 的行。
pub(crate) fn spawn_retention_task(
    db: crate::db::Db,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.changed() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)) => {}
            }
            let keep_days = db
                .setting("data")
                .ok()
                .flatten()
                .and_then(|value| value["keep_days"].as_i64())
                .unwrap_or(30);
            if keep_days <= 0 {
                continue;
            }
            let cutoff = crate::now() * 1000 - keep_days * 24 * 3600 * 1000;
            if let Ok(conn) = db.conn.lock() {
                if let Err(error) = purge_before(&conn, cutoff) {
                    tracing::warn!(%error, "清理过期行情失败");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{CandleSnapshot, MarketSnapshot};

    fn conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        ensure_table(&conn).unwrap();
        conn
    }

    fn candle(close_time: i64, close: f64) -> CandleSnapshot {
        CandleSnapshot {
            close_time,
            open: close - 0.5,
            high: close + 0.5,
            low: close - 1.0,
            close,
            volume: 10.0,
        }
    }

    fn market(symbol: &str, candles: Vec<CandleSnapshot>) -> MarketSnapshot {
        MarketSnapshot {
            symbol: symbol.to_string(),
            mark_price: 100.0,
            timestamp: 1_700_000_000,
            quote_volume: 1_000_000.0,
            funding_rate: 0.0,
            open_interest: 0.0,
            open_interest_change_pct: 0.0,
            bid: 100.0,
            ask: 100.01,
            spread_bps: 1.0,
            candles,
        }
    }

    #[test]
    fn upsert_candles_is_idempotent_by_close_time() {
        let conn = conn();
        let market = market("BTCUSDT", vec![candle(1_700_000_059_999, 100.0)]);
        upsert_candles(&conn, "15m", std::slice::from_ref(&market)).unwrap();
        upsert_candles(&conn, "15m", std::slice::from_ref(&market)).unwrap();
        let (rows, close): (i64, f64) = conn
            .query_row(
                "SELECT COUNT(*), MAX(close) FROM market_candles WHERE symbol='BTCUSDT'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, 1, "同一根 K 线重复写入必须幂等");
        assert_eq!(close, 100.0);
    }

    #[test]
    fn load_candles_returns_ascending_close_time() {
        let conn = conn();
        let bars = vec![
            candle(1_700_000_059_999, 101.0),
            candle(1_700_000_119_999, 100.0),
        ];
        upsert_candles(&conn, "15m", std::slice::from_ref(&market("ETHUSDT", bars))).unwrap();
        let loaded = load_candles(&conn, "ETHUSDT", "15m", 10).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(
            loaded[0].close_time < loaded[1].close_time,
            "必须按时间升序"
        );
    }

    #[test]
    fn purge_before_removes_only_expired_rows() {
        let conn = conn();
        let old = market("BTCUSDT", vec![candle(1_000, 100.0)]);
        let fresh = market("BTCUSDT", vec![candle(2_000_000_000_000, 101.0)]);
        upsert_candles(&conn, "15m", &[old, fresh]).unwrap();
        let removed = purge_before(&conn, 1_000_000_000_000).unwrap();
        assert_eq!(removed, 1);
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM market_candles", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 1, "未过期的行必须留下");
    }

    #[test]
    fn purge_before_respects_keep_days_cutoff() {
        let conn = conn();
        let market = market("BTCUSDT", vec![candle(1_000, 100.0)]);
        upsert_candles(&conn, "15m", std::slice::from_ref(&market)).unwrap();
        let cutoff = crate::now() * 1000 - 3 * 24 * 3600 * 1000;
        assert_eq!(
            purge_before(&conn, cutoff).unwrap(),
            1,
            "超期数据必须被清掉"
        );
    }
}
