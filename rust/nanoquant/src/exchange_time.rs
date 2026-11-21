//! 交易所时钟偏移。
//!
//! 币安按服务器时间校验签名里的 `timestamp`：本地时钟快 1 秒就会返回
//! `-1021 Timestamp ... ahead of the server's time`，`recvWindow` 不能弥补未来的偏差。
//! 这里用 `/fapi/v1/time` 定期测出「服务器时间 - 本地时间」的偏移，签名时间戳按偏移修正。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::Value;

#[derive(Clone, Copy, Default)]
struct ClockState {
    offset_ms: i64,
    synced_at_ms: i64,
}

static CLOCKS: LazyLock<Mutex<HashMap<String, ClockState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static CLIENT: LazyLock<Result<reqwest::Client, reqwest::Error>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
});

/// 偏移的有效期：本地时钟漂移很慢，一分钟同步一次足够，也不会打满限频。
const SYNC_TTL_MS: i64 = 60_000;

fn key(base: &str) -> String {
    base.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// 按已同步的偏移修正后的时间戳；未同步过时就是本地时间。
pub(crate) fn now_ms(base: &str) -> i64 {
    adjusted(crate::now_millis(), offset(base))
}

fn offset(base: &str) -> i64 {
    CLOCKS
        .lock()
        .ok()
        .and_then(|clocks| clocks.get(&key(base)).map(|state| state.offset_ms))
        .unwrap_or(0)
}

fn adjusted(local_ms: i64, offset_ms: i64) -> i64 {
    local_ms.saturating_add(offset_ms)
}

/// 服务器时间取往返中点，扣掉半个网络往返，避免把延迟算成时钟偏差。
fn offset_from_reply(local_before: i64, local_after: i64, server_ms: i64) -> i64 {
    let midpoint = local_before.saturating_add(local_after) / 2;
    server_ms.saturating_sub(midpoint)
}

fn store(base: &str, state: ClockState) {
    if let Ok(mut clocks) = CLOCKS.lock() {
        clocks.insert(key(base), state);
    }
}

fn is_fresh(base: &str, now_ms: i64) -> bool {
    CLOCKS
        .lock()
        .ok()
        .and_then(|clocks| clocks.get(&key(base)).copied())
        .is_some_and(|state| now_ms.saturating_sub(state.synced_at_ms) < SYNC_TTL_MS)
}

/// 与交易所对时。失败时保留旧偏移，只记录尝试时间，避免每次请求都打网络。
pub(crate) async fn sync(base: &str) -> Result<i64, String> {
    let client = CLIENT.as_ref().map_err(|error| error.to_string())?;
    let url = format!("{}/fapi/v1/time", base.trim_end_matches('/'));
    let before = crate::now_millis();
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let after = crate::now_millis();
    if !response.status().is_success() {
        store(
            base,
            ClockState {
                offset_ms: offset(base),
                synced_at_ms: after,
            },
        );
        return Err(format!("对时失败 HTTP {}", response.status()));
    }
    let body = response
        .json::<Value>()
        .await
        .map_err(|error| error.to_string())?;
    let server_ms = body["serverTime"]
        .as_i64()
        .ok_or_else(|| "对时响应缺少 serverTime".to_string())?;
    let offset_ms = offset_from_reply(before, after, server_ms);
    store(
        base,
        ClockState {
            offset_ms,
            synced_at_ms: after,
        },
    );
    Ok(offset_ms)
}

/// 供签名前调用：偏移过期时先对时，对不上就沿用旧值继续。
pub(crate) async fn ensure_fresh(base: &str) {
    if is_fresh(base, crate::now_millis()) {
        return;
    }
    let _ = sync(base).await;
}

/// 交易所返回 -1021（时间戳偏差）时立刻重新对时，下一次尝试即可恢复。
pub(crate) async fn note_response(base: &str, status: u16, body: &str) {
    if (status == 400 || status == 401) && body.contains("\"code\":-1021") {
        let _ = sync(base).await;
    }
}

/// 后台对时：启动即同步一次，之后每分钟刷新实盘与模拟盘两条线路。
pub(crate) fn spawn_sync_task() {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            for base in [
                "https://fapi.binance.com",
                "https://testnet.binancefuture.com",
            ] {
                let _ = sync(base).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_uses_the_round_trip_midpoint() {
        assert_eq!(offset_from_reply(1_000, 1_200, 50_000), 48_900);
        assert_eq!(offset_from_reply(1_000, 1_000, 1_000), 0);
    }

    #[test]
    fn adjusted_timestamp_applies_the_offset() {
        assert_eq!(adjusted(10_000, -1_000), 9_000);
        assert_eq!(adjusted(10_000, 2_500), 12_500);
    }

    #[test]
    fn base_keys_ignore_case_and_trailing_slash() {
        assert_eq!(key("https://Fapi.Binance.com/"), "https://fapi.binance.com");
    }

    #[tokio::test]
    async fn stale_state_is_reported_even_when_offset_was_zero() {
        let base = "https://clock-test.invalid";
        store(
            base,
            ClockState {
                offset_ms: 0,
                synced_at_ms: 0,
            },
        );
        assert!(!is_fresh(base, crate::now_millis()));
    }
}
