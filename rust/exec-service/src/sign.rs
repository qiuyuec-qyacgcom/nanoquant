//! 币安签名：HMAC-SHA256（hex）与 Ed25519（base64）。
//!
//! 与 Python 侧 `exchange/client.py::sign_query` 保持**逐字节一致**：
//! - HMAC  → 小写 hex
//! - Ed25519 → 标准 base64（含 `=` 填充），拼 URL 时必须再 urlencode

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use ed25519_dalek::{Signer as _, SigningKey};
use hmac::{Hmac, Mac};
use sha2::Sha256;

const HEX: &[u8; 16] = b"0123456789abcdef";

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 去掉 PEM 头尾与空白；单行 PEM（`-----BEGIN...----- M... -----END...-----`）也要能处理。
///
/// 与 Python 侧 `normalize_pem` 同口径：**按 `-----` 标记片段删除**，而不是按行过滤。
/// 旧实现用 `skipping` 状态机，进入 BEGIN 后永远退出不了（单行/多行 PEM 都被清空），
/// 结果 `detect()` 静默回落到 HMAC —— 签名自然是错的。
pub fn normalize_pem(value: &str) -> String {
    let mut text = value.trim().to_string();
    if !text.contains("-----") {
        return text.split_whitespace().collect();
    }
    while let Some(start) = text.find("-----") {
        let Some(offset) = text[start + 5..].find("-----") else {
            break;
        };
        let end = start + 5 + offset + 5;
        text.replace_range(start..end, "");
    }
    text.split_whitespace().collect()
}

pub enum Signer {
    Hmac(Vec<u8>),
    Ed25519(Box<SigningKey>),
}

impl Signer {
    /// 能按 base64(PKCS#8 DER) 解出 Ed25519 私钥就按 Ed25519，否则当 HMAC 密钥。
    pub fn detect(secret: &str) -> Self {
        let body = normalize_pem(secret);
        if let Ok(der) = B64.decode(body.as_bytes()) {
            if let Ok(key) = SigningKey::from_pkcs8_der(&der) {
                return Signer::Ed25519(Box::new(key));
            }
        }
        Signer::Hmac(body.into_bytes())
    }

    pub fn algo(&self) -> &'static str {
        match self {
            Signer::Hmac(_) => "hmac-sha256",
            Signer::Ed25519(_) => "ed25519",
        }
    }

    pub fn sign(&self, query: &str) -> String {
        match self {
            Signer::Hmac(secret) => {
                let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac accepts any key");
                mac.update(query.as_bytes());
                to_hex(&mac.finalize().into_bytes())
            }
            Signer::Ed25519(key) => B64.encode(key.sign(query.as_bytes()).to_bytes()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUERY: &str = "symbol=BTCUSDT&side=BUY&type=MARKET&timestamp=1700000000000";

    /// 固定种子 → 确定性签名，Python 侧已生成期望值
    const ED_SECRET: &str = "MC4CAQAwBQYDK2VwBCIEIAABAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhscHR4f";
    const ED_SIG: &str = "9Z7/7rbpsRx/V335rbIjgYZc73mgxDfAwdrSux8fheFxw4zb9ncQdI0rnNtyYTNFNhpkMNoOYSba0xJe+hb2Aw==";
    const HMAC_HEX: &str = "c5bee7ef23d0f08ff08fe04022f745e9ff389ec6ea2ad8203e0a096b238dcf36";

    #[test]
    fn ed25519_matches_python() {
        let signer = Signer::detect(ED_SECRET);
        assert_eq!(signer.algo(), "ed25519");
        assert_eq!(signer.sign(QUERY), ED_SIG);
    }

    #[test]
    fn hmac_matches_python() {
        let signer = Signer::detect("secret");
        assert_eq!(signer.algo(), "hmac-sha256");
        assert_eq!(signer.sign(QUERY), HMAC_HEX);
    }

    #[test]
    fn single_line_pem_is_normalized() {
        let one_line = format!("-----BEGIN PRIVATE KEY----- {ED_SECRET} -----END PRIVATE KEY-----");
        assert_eq!(normalize_pem(&one_line), ED_SECRET);
        assert_eq!(Signer::detect(&one_line).algo(), "ed25519");
    }
}
