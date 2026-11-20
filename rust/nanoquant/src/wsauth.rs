//! WebSocket 首帧鉴权。对齐 `api/routes/ws.py` 的 `_auth_first_frame`。
//!
//! token 不放进 URL：查询串会进 access log、反向代理日志和浏览器历史。
//! 连接先接受，第一帧必须是 `{"type":"auth","token":"..."}` 且 token 校验通过，
//! 其它帧、坏 JSON、无效 token 一律视为未认证，由调用方决定关闭还是降级匿名。

use crate::auth::verify_token;

/// 首帧是否通过鉴权。不是 auth 帧、JSON 解析失败、token 校验不过都返回 false。
pub fn first_frame_ok(raw: &str, version: i64, master_key: &[u8], salt: &str, now: i64) -> bool {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    if msg.get("type").and_then(|v| v.as_str()) != Some("auth") {
        return false;
    }
    let token = msg.get("token").and_then(|v| v.as_str()).unwrap_or("");
    verify_token(token, Some(version), master_key, salt, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::issue_token;

    const KEY: &[u8] = b"master-key";
    const SALT: &str = "salt";

    #[test]
    fn only_a_valid_auth_frame_authenticates() {
        let (token, _) = issue_token("admin", 12, 1, KEY, SALT, 1_000);
        let frame = format!(r#"{{"type":"auth","token":"{token}"}}"#);
        assert!(first_frame_ok(&frame, 1, KEY, SALT, 1_000), "合法首帧放行");

        assert!(
            !first_frame_ok(r#"{"type":"ping"}"#, 1, KEY, SALT, 1_000),
            "不是 auth 帧不算"
        );
        assert!(
            !first_frame_ok("not json", 1, KEY, SALT, 1_000),
            "坏 JSON 不算"
        );
        assert!(
            !first_frame_ok(r#"{"type":"auth","token":"bad"}"#, 1, KEY, SALT, 1_000),
            "无效 token 不算"
        );
    }
}
