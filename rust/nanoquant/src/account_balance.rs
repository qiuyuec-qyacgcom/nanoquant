//! 账户余额刷新：每 3 秒从交易所账户接口刷新钱包/可用/权益，写入 `engine_state`。

use tokio::sync::watch;
use tokio::time::Duration;

use crate::runtime::EngineControl;

/// 账户面板专用：每 3 秒从交易所账户接口刷新钱包/可用/权益。
pub(crate) async fn run_account_balance_task(
    db: crate::db::Db,
    master_key: Vec<u8>,
    mut stop: watch::Receiver<bool>,
) {
    let control = EngineControl::new(db.clone(), master_key);
    loop {
        if *stop.borrow() {
            break;
        }
        if let Err(error) = refresh_account_balance(&db, &control).await {
            tracing::debug!(%error, "账户余额刷新失败");
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            _ = stop.changed() => break,
        }
    }
}

async fn refresh_account_balance(
    db: &crate::db::Db,
    control: &EngineControl,
) -> Result<(), String> {
    let mode: String = {
        let conn = db
            .conn
            .lock()
            .map_err(|_| "数据库锁不可用".to_string())?;
        conn.query_row("SELECT value FROM engine_state WHERE key='mode'", [], |row| {
            row.get(0)
        })
        .unwrap_or_else(|_| "demo".to_string())
    };
    if mode != "live" {
        return Ok(());
    }
    let account = control.account_snapshot("live").await?;
    let payload = serde_json::json!({
        "ts": crate::now(),
        "equity": account.equity,
        "wallet": account.wallet,
        "available": account.available_balance,
    });
    let conn = db
        .conn
        .lock()
        .map_err(|_| "数据库锁不可用".to_string())?;
    conn.execute(
        "INSERT OR REPLACE INTO engine_state(key,value) VALUES('account.live',?1)",
        [payload.to_string()],
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}