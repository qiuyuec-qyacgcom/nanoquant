use crate::manage::ManageAction;
use rusqlite::{Connection, OptionalExtension, Transaction};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DemoFill {
    pub client_order_id: String,
    pub exchange_order_id: String,
    pub symbol: String,
    pub side: String,
    pub qty: f64,
    pub price: f64,
    pub reduce_only: bool,
    pub reason: String,
}

pub(crate) fn apply_demo_fill(
    conn: &Connection,
    mode: &str,
    now: i64,
    fee_rate: f64,
    fill: &DemoFill,
) -> Result<(bool, Option<i64>), String> {
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let applied = apply_demo_fill_tx(&tx, mode, now, fee_rate, fill)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(applied)
}

pub(crate) fn apply_demo_fill_tx(
    tx: &Transaction<'_>,
    mode: &str,
    now: i64,
    fee_rate: f64,
    fill: &DemoFill,
) -> Result<(bool, Option<i64>), String> {
    if mode != "demo" {
        return Err("模拟成交只能落在 demo 环境".into());
    }
    apply_fill_tx(tx, mode, now, fee_rate, fill)
}

/// 实盘成交结算：订单行已在提交前写好，这里只推进状态并减仓。
pub(crate) fn apply_live_fill_tx(
    tx: &Transaction<'_>,
    mode: &str,
    now: i64,
    fee_rate: f64,
    fill: &DemoFill,
) -> Result<(bool, Option<i64>), String> {
    if mode != "live" {
        return Err("实盘成交只能落在 live 环境".into());
    }
    apply_fill_tx(tx, mode, now, fee_rate, fill)
}

/// 成交结算的共用实现。幂等靠订单状态：已经是 FILLED 的行不再重复减仓。
/// 返回值是本笔新产生的成交 id，供平仓后触发复盘；重复结算返回 None。
fn apply_fill_tx(
    tx: &Transaction<'_>,
    mode: &str,
    now: i64,
    fee_rate: f64,
    fill: &DemoFill,
) -> Result<(bool, Option<i64>), String> {
    if mode.is_empty() || !fill.reduce_only || !fill.price.is_finite() || fill.price <= 0.0 {
        return Err("成交参数无效".into());
    }
    if !fee_rate.is_finite() || !(0.0..=0.01).contains(&fee_rate) {
        return Err("成交手续费率无效".into());
    }
    let existing: Option<String> = tx
        .query_row(
            "SELECT status FROM orders WHERE client_order_id=?1 AND mode=?2",
            (&fill.client_order_id, mode),
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if existing.as_deref() == Some("FILLED") {
        return Ok((false, None));
    }
    let position = tx
        .query_row(
            "SELECT id, qty, entry_price, opened_at FROM positions WHERE symbol=?1 AND side=?2 AND mode=?3 AND status='open' ORDER BY id LIMIT 1",
            (&fill.symbol, if fill.side == "SELL" { "long" } else { "short" }, mode),
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?, row.get::<_, f64>(2)?, row.get::<_, i64>(3)?)),
        )
        .map_err(|error| error.to_string())?;
    if !position.1.is_finite() || position.1 <= 0.0 || !fill.qty.is_finite() || fill.qty <= 0.0 {
        return Err("成交数量无效".into());
    }
    // 交易所回报优先，但本地账本只减到 0。多出来的部分说明账本已经漂移，
    // 写负仓位只会让后面的风控算出更大的错，交给对账去修。
    let qty = fill.qty.min(position.1);
    let pnl = (fill.price - position.2) * qty * if fill.side == "SELL" { 1.0 } else { -1.0 };
    let fee = fill.price * qty * fee_rate;
    if !pnl.is_finite() || !fee.is_finite() {
        return Err("成交盈亏无效".into());
    }
    let position_side = if fill.side == "SELL" { "long" } else { "short" };
    let features = position_features(tx, position.0);
    let trade_id = record_trade(
        tx,
        mode,
        &fill.symbol,
        position_side,
        position.2,
        position.3,
        qty,
        fill.price,
        pnl,
        now,
        &fill.reason,
        features.as_deref(),
        fee,
    )?;
    if existing.is_some() {
        // 提交前已经写过 SUBMITTING 行：这里只推进状态，不能插第二条。
        tx.execute(
            "UPDATE orders SET ts=?1, status='FILLED', price=?2, qty=?3, exchange_order_id=?4 WHERE client_order_id=?5 AND mode=?6",
            (now, fill.price, fill.qty, &fill.exchange_order_id, &fill.client_order_id, mode),
        )
        .map_err(|error| error.to_string())?;
    } else {
        tx.execute(
            "INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,client_order_id,exchange_order_id,reduce_only,mode,latency_ms) VALUES(?1,?2,?3,'MARKET',?4,?5,'FILLED',?6,?7,?8,1,?9,0)",
            (now, &fill.symbol, &fill.side, fill.qty, fill.price, &fill.reason, &fill.client_order_id, &fill.exchange_order_id, mode),
        ).map_err(|error| error.to_string())?;
    }
    write_close(
        tx,
        position.0,
        position.1,
        qty,
        fill.price,
        now,
        &fill.reason,
        pnl,
        fee,
    )?;
    if let Some(id) = trade_id {
        // 老库没有 trade_id 列时只跳过回填，成交本身已落库。
        let _ = tx.execute(
            "UPDATE positions SET trade_id=?1 WHERE id=?2",
            (id, position.0),
        );
    }
    Ok((true, trade_id))
}

/// 决策时写入的 features（含心法与经验注入 ID）。旧库/测试库可能没有该列。
fn position_features(tx: &Transaction<'_>, position_id: i64) -> Option<String> {
    tx.query_row(
        "SELECT features FROM positions WHERE id=?1",
        [position_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

/// 补记一笔已发生的平仓：写成交表、改仓位，不写委托。
///
/// 交易所侧条件单成交时我们没发过任何请求，凭空插一条订单行只会让
/// 订单流水与交易所对不上；成交表才是这笔盈亏的归属。
#[allow(clippy::too_many_arguments)]
fn record_trade(
    tx: &Transaction<'_>,
    mode: &str,
    symbol: &str,
    position_side: &str,
    entry_price: f64,
    opened_at: i64,
    qty: f64,
    exit_price: f64,
    pnl: f64,
    now: i64,
    reason: &str,
    features: Option<&str>,
    fee: f64,
) -> Result<Option<i64>, String> {
    let trades_table: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='trades')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if !trades_table {
        return Ok(None);
    }
    let columns = {
        let mut stmt = tx
            .prepare("PRAGMA table_info(trades)")
            .map_err(|e| e.to_string())?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|e| e.to_string())?;
        names
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
    };
    let complete = [
        "symbol",
        "side",
        "qty",
        "entry_price",
        "exit_price",
        "pnl",
        "opened_at",
        "closed_at",
        "hold_seconds",
        "exit_reason",
        "mode",
    ]
    .iter()
    .all(|column| columns.iter().any(|name| name == column));
    if !complete {
        return Ok(None);
    }
    let full = [
        "leverage",
        "notional",
        "pnl_pct",
        "roi_pct",
        "fees",
        "ai_confidence",
        "ai_reason",
        "signal_score",
        "slippage_bps",
        "reviewed",
        "review_note",
        "features",
        "mantra_ids",
    ]
    .iter()
    .all(|column| columns.iter().any(|name| name == column));
    if full {
        let meta: Option<(f64, f64, f64, String)> = tx
            .query_row(
                "SELECT COALESCE(leverage,1.0), COALESCE(signal_score,0.0), \
                 COALESCE(ai_confidence,0.0), COALESCE(ai_reason,'') \
                 FROM positions WHERE symbol=?1 AND side=?2 AND mode=?3 AND status='open' \
                 ORDER BY id LIMIT 1",
                (symbol, position_side, mode),
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let (leverage, signal_score, ai_confidence, ai_reason) =
            meta.unwrap_or((1.0, 0.0, 0.0, String::new()));
        let leverage = if leverage.is_finite() && leverage > 0.0 {
            leverage
        } else {
            1.0
        };
        let notional = qty * entry_price;
        let pnl_pct = if notional.is_finite() && notional > 0.0 {
            pnl / notional * 100.0
        } else {
            0.0
        };
        let roi_pct = pnl_pct * leverage;
        let ai_reason = if ai_reason.trim().is_empty() {
            reason.to_string()
        } else {
            ai_reason
        };
        tx.execute(
            "INSERT INTO trades(symbol,side,qty,entry_price,exit_price,leverage,notional,pnl,pnl_pct,roi_pct,fees,opened_at,closed_at,hold_seconds,exit_reason,ai_confidence,ai_reason,signal_score,slippage_bps,mode,reviewed,review_note,features,mantra_ids) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,0,'',?21,'[]')",
            rusqlite::params![
                symbol,
                position_side,
                qty,
                entry_price,
                exit_price,
                leverage,
                notional,
                pnl,
                pnl_pct,
                roi_pct,
                fee,
                opened_at,
                now,
                now.saturating_sub(opened_at),
                reason,
                ai_confidence,
                ai_reason,
                signal_score,
                0.0_f64,
                mode,
                features,
            ],
        )
        .map_err(|error| error.to_string())?;
        return Ok(Some(tx.last_insert_rowid()));
    }
    tx.execute(
        "INSERT INTO trades(symbol,side,qty,entry_price,exit_price,pnl,opened_at,closed_at,hold_seconds,exit_reason,mode) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        (
            symbol,
            position_side,
            qty,
            entry_price,
            exit_price,
            pnl,
            opened_at,
            now,
            now.saturating_sub(opened_at),
            reason,
            mode,
        ),
    )
    .map_err(|error| error.to_string())?;
    let trade_id = tx.last_insert_rowid();
    if let Some(features) = features.filter(|_| columns.iter().any(|name| name == "features")) {
        tx.execute(
            "UPDATE trades SET features=?1 WHERE id=?2",
            (features, trade_id),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(Some(trade_id))
}

fn write_close(
    tx: &Transaction<'_>,
    position_id: i64,
    qty_before: f64,
    qty_closed: f64,
    exit_price: f64,
    now: i64,
    reason: &str,
    pnl: f64,
    fee: f64,
) -> Result<(), String> {
    let remaining = qty_before - qty_closed;
    if remaining <= 1e-12 {
        tx.execute(
            "UPDATE positions SET qty=0,status='closed',mark_price=?1,closed_at=?2,exit_price=?1,exit_reason=?3,\
             realized_pnl=COALESCE(realized_pnl,0)+?4,fees=COALESCE(fees,0)+?5 WHERE id=?6",
            (exit_price, now, reason, pnl - fee, fee, position_id),
        )
        .map_err(|e| e.to_string())?;
        register_symbol_cooldown(tx, position_id, now)?;
    } else {
        tx.execute(
            "UPDATE positions SET qty=?1,mark_price=?2,partial_taken_at=?3,\
             realized_pnl=COALESCE(realized_pnl,0)+?4,fees=COALESCE(fees,0)+?5 WHERE id=?6",
            (remaining, exit_price, now, pnl - fee, fee, position_id),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn register_symbol_cooldown(
    tx: &Transaction<'_>,
    position_id: i64,
    now: i64,
) -> Result<(), String> {
    let present: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='cooldowns')",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if !present {
        return Ok(());
    }
    let (symbol, pnl): (String, f64) = tx
        .query_row(
            "SELECT symbol,realized_pnl FROM positions WHERE id=?1",
            [position_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|e| e.to_string())?;
    if pnl >= 0.0 {
        tx.execute(
            "DELETE FROM cooldowns WHERE symbol=?1 AND kind='symbol'",
            [&symbol],
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    let raw: Option<String> = tx
        .query_row("SELECT value FROM app_settings WHERE key='risk'", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?;
    let minutes = raw
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v["symbol_cooldown_minutes"].as_i64())
        .unwrap_or(5)
        .clamp(0, 1440);
    if minutes > 0 {
        tx.execute("INSERT INTO cooldowns(symbol,until_ts,reason,kind) VALUES(?1,?2,?3,'symbol') ON CONFLICT(symbol) DO UPDATE SET until_ts=excluded.until_ts,reason=excluded.reason,kind=excluded.kind WHERE cooldowns.until_ts<excluded.until_ts", (&symbol, now.saturating_add(minutes * 60), format!("本笔亏损 {pnl:.4} USDT"))).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 交易所侧已经没有这个仓位（保护单被触发、手动平掉）：本地补记平仓。
///
/// 不能只把 `status` 改成 closed —— 那样成交表和复盘都会漏掉这笔。
/// 出场价优先用交易所成交价，拿不到时退回标记价，与 legacy 内核（已退役） 的
/// `_venue_exit_price or mark_price` 语义一致。
pub(crate) fn close_position(
    db: &crate::db::Db,
    position_id: i64,
    exit_price: f64,
    reason: &str,
    live: bool,
) -> Result<(), String> {
    if !exit_price.is_finite() || exit_price <= 0.0 {
        return Err("平仓价无效".into());
    }
    let mut conn = db.conn.lock().map_err(|_| "数据库忙".to_string())?;
    let tx = conn.transaction().map_err(|error| error.to_string())?;
    let position = tx
        .query_row(
            "SELECT mode, symbol, side, qty, entry_price, opened_at FROM positions \
             WHERE id=?1 AND status='open'",
            [position_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some((mode, symbol, side, qty, entry, opened_at)) = position else {
        return Err("仓位不存在或已关闭".into());
    };
    if (mode == "live") != live || !matches!(side.as_str(), "long" | "short") {
        return Err("仓位环境或方向无效".into());
    }
    if !qty.is_finite() || qty <= 0.0 || !entry.is_finite() || entry <= 0.0 {
        return Err("仓位数量或开仓价无效".into());
    }
    let direction = if side == "long" { 1.0 } else { -1.0 };
    let pnl = (exit_price - entry) * qty * direction;
    let fee = 0.0;
    let features = position_features(&tx, position_id);
    let _ = record_trade(
        &tx,
        &mode,
        &symbol,
        &side,
        entry,
        opened_at,
        qty,
        exit_price,
        pnl,
        crate::now(),
        reason,
        features.as_deref(),
        fee,
    )?;
    write_close(
        &tx,
        position_id,
        qty,
        qty,
        exit_price,
        crate::now(),
        reason,
        pnl,
        fee,
    )?;
    tx.commit().map_err(|error| error.to_string())
}

pub(crate) fn close_venue_position_tx(
    tx: &Transaction<'_>,
    mode: &str,
    now: i64,
    fee_rate: f64,
    symbol: &str,
    position_side: &str,
    exit_price: f64,
) -> Result<bool, String> {
    if !matches!(position_side, "long" | "short") {
        return Err("仓位方向无效".into());
    }
    if !exit_price.is_finite() || exit_price <= 0.0 {
        return Err("对账出场价无效".into());
    }
    if !fee_rate.is_finite() || !(0.0..=0.01).contains(&fee_rate) {
        return Err("成交手续费率无效".into());
    }
    let position = tx
        .query_row(
            "SELECT id, qty, entry_price, opened_at FROM positions \
             WHERE symbol=?1 AND side=?2 AND mode=?3 AND status='open' ORDER BY id LIMIT 1",
            (symbol, position_side, mode),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some((id, qty, entry, opened_at)) = position else {
        return Ok(false);
    };
    if !qty.is_finite() || qty <= 0.0 || !entry.is_finite() || entry <= 0.0 {
        return Err("对账仓位数量或开仓价无效".into());
    }
    let direction = if position_side == "long" { 1.0 } else { -1.0 };
    let pnl = (exit_price - entry) * qty * direction;
    let fee = exit_price * qty * fee_rate;
    if !pnl.is_finite() || !fee.is_finite() {
        return Err("对账盈亏无效".into());
    }
    let features = position_features(tx, id);
    let trade_id = record_trade(
        tx,
        mode,
        symbol,
        position_side,
        entry,
        opened_at,
        qty,
        exit_price,
        pnl,
        now,
        "venue_sync",
        features.as_deref(),
        fee,
    )?;
    write_close(tx, id, qty, qty, exit_price, now, "venue_sync", pnl, fee)?;
    if let Some(trade_id) = trade_id {
        let _ = tx.execute(
            "UPDATE positions SET trade_id=?1 WHERE id=?2",
            (trade_id, id),
        );
    }
    Ok(true)
}

pub(crate) fn demo_fill_for_manage(
    cycle_id: &str,
    symbol: &str,
    position_side: &str,
    position_qty: f64,
    mark_price: f64,
    action: &ManageAction,
) -> Option<DemoFill> {
    if cycle_id.is_empty()
        || symbol.is_empty()
        || !matches!(position_side, "long" | "short")
        || !position_qty.is_finite()
        || position_qty <= 0.0
        || !mark_price.is_finite()
        || mark_price <= 0.0
    {
        return None;
    }
    let (qty, purpose, reason) = match action {
        ManageAction::PartialTakeProfit { qty } => (
            *qty,
            "partial-tp",
            crate::exit_reason::ExitReason::PartialTakeProfit
                .as_str()
                .to_string(),
        ),
        ManageAction::Close { reason } => (position_qty, "close", reason.clone()),
        ManageAction::Hold | ManageAction::MoveToBreakeven { .. } => return None,
    };
    if !qty.is_finite() || qty <= 0.0 || qty > position_qty {
        return None;
    }
    Some(DemoFill {
        client_order_id: format!("demo-{cycle_id}-{symbol}-{purpose}"),
        exchange_order_id: String::new(),
        symbol: symbol.to_string(),
        side: if position_side == "long" {
            "SELL"
        } else {
            "BUY"
        }
        .to_string(),
        qty,
        price: mark_price,
        reduce_only: true,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE orders(id INTEGER PRIMARY KEY,ts INTEGER,symbol TEXT,side TEXT,order_type TEXT,qty REAL,price REAL,status TEXT,purpose TEXT,client_order_id TEXT,exchange_order_id TEXT,reduce_only INTEGER,mode TEXT,latency_ms INTEGER NOT NULL); CREATE TABLE positions(id INTEGER PRIMARY KEY,symbol TEXT,side TEXT,qty REAL,entry_price REAL,mark_price REAL,leverage REAL,margin REAL,sl_price REAL,tp_price REAL,peak_price REAL,initial_sl_price REAL,signal_score REAL,opened_at INTEGER,closed_at INTEGER,exit_price REAL,exit_reason TEXT,realized_pnl REAL,fees REAL,status TEXT,mode TEXT,venue_unrealized REAL,venue_ts INTEGER,partial_taken_at INTEGER,ai_confidence REAL DEFAULT 0,ai_reason TEXT DEFAULT ''); INSERT INTO positions(id,symbol,side,qty,entry_price,mark_price,status,mode,opened_at) VALUES(1,'BTCUSDT','long',0.25,100000,98500,'open','demo',1);") .unwrap();
        conn
    }

    #[test]
    fn closing_fill_satisfies_production_trades_schema() {
        let conn = db();
        conn.execute_batch(
            "UPDATE positions SET leverage=5, signal_score=0.4, ai_confidence=0.6, ai_reason='test' WHERE id=1;
             CREATE TABLE trades(
               id INTEGER PRIMARY KEY, symbol TEXT NOT NULL, side TEXT NOT NULL, qty REAL NOT NULL,
               entry_price REAL NOT NULL, exit_price REAL NOT NULL, leverage REAL NOT NULL,
               notional REAL NOT NULL, pnl REAL NOT NULL, pnl_pct REAL NOT NULL, roi_pct REAL NOT NULL,
               fees REAL NOT NULL, opened_at INTEGER NOT NULL, closed_at INTEGER NOT NULL,
               hold_seconds INTEGER NOT NULL, exit_reason TEXT NOT NULL, ai_confidence REAL NOT NULL,
               ai_reason TEXT NOT NULL, signal_score REAL NOT NULL, slippage_bps REAL NOT NULL,
               mode TEXT NOT NULL, reviewed BOOLEAN NOT NULL, review_note TEXT NOT NULL,
               features JSON, mantra_ids JSON, ai_tuning_set_id INTEGER
             );",
        )
        .unwrap();
        let fill = DemoFill {
            client_order_id: "demo-cycle-42-BTCUSDT-close".into(),
            exchange_order_id: String::new(),
            symbol: "BTCUSDT".into(),
            side: "SELL".into(),
            qty: 0.25,
            price: 98_500.0,
            reduce_only: true,
            reason: "stop_loss".into(),
        };
        let trade_id = apply_demo_fill(&conn, "demo", 42, 0.0005, &fill)
            .unwrap()
            .1
            .expect("生产 schema 下平仓必须能写入成交");
        let (leverage, fees, pnl_pct, roi_pct, ai_confidence, ai_reason): (
            f64,
            f64,
            f64,
            f64,
            f64,
            String,
        ) = conn
            .query_row(
                "SELECT leverage,fees,pnl_pct,roi_pct,ai_confidence,ai_reason FROM trades WHERE id=?1",
                [trade_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(leverage, 5.0);
        assert!((fees - 98_500.0 * 0.25 * 0.0005).abs() < 1e-9);
        assert!(pnl_pct < 0.0);
        assert!((roi_pct - pnl_pct * 5.0).abs() < 1e-9);
        assert_eq!(ai_confidence, 0.6);
        assert_eq!(ai_reason, "test");
    }

    #[test]
    fn closing_fill_provides_latency_ms_for_strict_schemas() {
        // 夹具的 orders.latency_ms 是 NOT NULL 且没有默认值：漏列会让整轮平仓失败
        let conn = db();
        let fill = DemoFill {
            client_order_id: "demo-cycle-42-BTCUSDT-close".into(),
            exchange_order_id: String::new(),
            symbol: "BTCUSDT".into(),
            side: "SELL".into(),
            qty: 0.25,
            price: 98_500.0,
            reduce_only: true,
            reason: "stop_loss".into(),
        };
        assert!(apply_demo_fill(&conn, "demo", 42, 0.0005, &fill).unwrap().0);
        let latency: i64 = conn
            .query_row("SELECT latency_ms FROM orders", [], |row| row.get(0))
            .unwrap();
        assert_eq!(latency, 0);
    }

    #[test]
    fn close_fill_is_reduce_only_at_mark_price() {
        let action = ManageAction::Close {
            reason: "stop_loss".into(),
        };
        let fill =
            demo_fill_for_manage("cycle-42", "BTCUSDT", "long", 0.25, 98_500.0, &action).unwrap();
        assert_eq!(fill.client_order_id, "demo-cycle-42-BTCUSDT-close");
        assert_eq!(fill.side, "SELL");
        assert_eq!(fill.qty, 0.25);
        assert_eq!(fill.price, 98_500.0);
        assert!(fill.reduce_only);
    }

    #[test]
    fn partial_profit_uses_reverse_side_and_requested_qty() {
        let action = ManageAction::PartialTakeProfit { qty: 0.1 };
        let fill =
            demo_fill_for_manage("cycle-42", "ETHUSDT", "short", 0.5, 2_500.0, &action).unwrap();
        assert_eq!(fill.side, "BUY");
        assert_eq!(fill.qty, 0.1);
        assert_eq!(fill.reason, "partial_take_profit");
    }

    #[test]
    fn protection_only_action_does_not_create_fill() {
        let action = ManageAction::MoveToBreakeven { new_sl: 100.04 };
        assert!(demo_fill_for_manage("cycle-42", "BTCUSDT", "long", 1.0, 101.0, &action).is_none());
    }

    #[test]
    fn fill_is_idempotent_and_reduces_position_atomically() {
        let conn = db();
        let fill = DemoFill {
            client_order_id: "demo-cycle-42-BTCUSDT-close".into(),
            exchange_order_id: String::new(),
            symbol: "BTCUSDT".into(),
            side: "SELL".into(),
            qty: 0.25,
            price: 98_500.0,
            reduce_only: true,
            reason: "stop_loss".into(),
        };
        assert!(apply_demo_fill(&conn, "demo", 42, 0.0005, &fill).unwrap().0);
        assert!(!apply_demo_fill(&conn, "demo", 43, 0.0005, &fill).unwrap().0);
        let status: String = conn
            .query_row("SELECT status FROM positions WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        let orders: i64 = conn
            .query_row("SELECT COUNT(*) FROM orders", [], |row| row.get(0))
            .unwrap();
        assert_eq!(status, "closed");
        assert_eq!(orders, 1);
        let (closed_at, exit_reason, realized, fees): (i64, String, f64, f64) = conn
            .query_row(
                "SELECT closed_at, exit_reason, realized_pnl, fees FROM positions WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(closed_at, 42);
        assert_eq!(exit_reason, "stop_loss");
        assert!((fees - 98500.0 * 0.25 * 0.0005).abs() < 1e-9);
        assert!(
            (realized - (-375.0 - fees)).abs() < 1e-9,
            "平仓盈亏要扣手续费"
        );
    }

    #[test]
    fn closing_fill_copies_injected_ids_into_the_trade_record() {
        let conn = db();
        conn.execute_batch(
            "ALTER TABLE positions ADD COLUMN features TEXT; \
             ALTER TABLE positions ADD COLUMN trade_id INTEGER DEFAULT 0; \
             CREATE TABLE trades(id INTEGER PRIMARY KEY, symbol TEXT, side TEXT, qty REAL, entry_price REAL, \
             exit_price REAL, pnl REAL, opened_at INTEGER, closed_at INTEGER, hold_seconds INTEGER, \
             exit_reason TEXT, mode TEXT, features TEXT); \
             UPDATE positions SET features='{\"injected_mantra_ids\":[1,2],\"injected_experience_ids\":[7],\"ai_review\":{\"reviewed\":true,\"action\":\"open_long\",\"reason\":\"test\"}}' WHERE id=1;",
        )
        .unwrap();
        let fill = DemoFill {
            client_order_id: "demo-cycle-42-BTCUSDT-close".into(),
            exchange_order_id: String::new(),
            symbol: "BTCUSDT".into(),
            side: "SELL".into(),
            qty: 0.25,
            price: 98_500.0,
            reduce_only: true,
            reason: "stop_loss".into(),
        };
        let trade_id = apply_demo_fill(&conn, "demo", 42, 0.0005, &fill)
            .unwrap()
            .1
            .expect("平仓成功要返回成交 id，供平仓后复盘使用");
        let (features, position_trade_id): (String, i64) = conn
            .query_row(
                "SELECT features, trade_id FROM positions WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(position_trade_id, trade_id, "仓位要指向本次成交");
        let trade_features: String = conn
            .query_row(
                "SELECT features FROM trades WHERE id=?1",
                [trade_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            features, trade_features,
            "决策时的注入 ID 必须随成交记录落库，复盘时才能回写命中率"
        );
        assert!(trade_features.contains("injected_experience_ids"));
        assert!(
            trade_features.contains("\"ai_review\"")
                && trade_features.contains("\"reviewed\":true"),
            "开仓 AI 复核结论必须随成交记录落库，复盘时才能追溯: {trade_features}"
        );
    }

    #[test]
    fn live_settlement_advances_the_reserved_order_and_clamps_overfill() {
        let conn = db();
        conn.execute("UPDATE positions SET mode='live' WHERE id=1", [])
            .unwrap();
        conn.execute(
            "INSERT INTO orders(ts,symbol,side,order_type,qty,price,status,purpose,client_order_id,exchange_order_id,reduce_only,mode,latency_ms) \
             VALUES(1,'BTCUSDT','SELL','MARKET',0.25,0,'SUBMITTING','stop_loss','nq-1','',1,'live',0)",
            [],
        )
        .unwrap();
        // 交易所回报 0.4，本地只有 0.25：账本减到 0，不能写成负仓位。
        let fill = DemoFill {
            client_order_id: "nq-1".into(),
            exchange_order_id: "9001".into(),
            symbol: "BTCUSDT".into(),
            side: "SELL".into(),
            qty: 0.4,
            price: 98_500.0,
            reduce_only: true,
            reason: "stop_loss".into(),
        };
        let tx = conn.unchecked_transaction().unwrap();
        assert!(
            apply_live_fill_tx(&tx, "live", 42, 0.0005, &fill)
                .unwrap()
                .0
        );
        tx.commit().unwrap();
        let (status, qty): (String, f64) = conn
            .query_row("SELECT status, qty FROM positions WHERE id=1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "closed");
        assert_eq!(qty, 0.0);
        let (count, order_qty, exchange_id, order_status): (i64, f64, String, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(qty), MAX(exchange_order_id), MAX(status) FROM orders",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "预留行必须被推进，而不是再插一条");
        assert_eq!(order_qty, 0.4, "订单记录交易所的真实成交量");
        assert_eq!(exchange_id, "9001");
        assert_eq!(order_status, "FILLED");

        // 同一笔再结算一次：状态已是 FILLED，不能再减仓。
        let tx = conn.unchecked_transaction().unwrap();
        assert!(
            !apply_live_fill_tx(&tx, "live", 43, 0.0005, &fill)
                .unwrap()
                .0
        );
        tx.commit().unwrap();
        let (realized, fees): (f64, f64) = conn
            .query_row(
                "SELECT realized_pnl, fees FROM positions WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(
            (fees - 98_500.0 * 0.25 * 0.0005).abs() < 1e-9,
            "费用只计一次"
        );
        assert!(realized < 0.0);
    }
}
