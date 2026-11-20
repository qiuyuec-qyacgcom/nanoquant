use base64::Engine;

/// AuthState 里的主密钥可能是 32 字节原文，也可能是 base64 文本。
pub fn key_text_from_master_bytes(master_key: &[u8]) -> Result<String, String> {
    if let Ok(text) = std::str::from_utf8(master_key) {
        let trimmed = text.trim();
        if base64::engine::general_purpose::URL_SAFE
            .decode(trimmed)
            .map(|bytes| bytes.len() == 32)
            .unwrap_or(false)
        {
            return Ok(trimmed.to_string());
        }
    }
    if master_key.len() == 32 {
        return Ok(base64::engine::general_purpose::URL_SAFE.encode(master_key));
    }
    Err("主密钥既不是 32 字节原文也不是有效 Fernet 密钥".to_string())
}

pub fn encrypt_secret(master_key: &str, plaintext: &str) -> Result<String, String> {
    let key = fernet_key(master_key)?;
    let fernet = fernet::Fernet::new(&key).ok_or_else(|| "Fernet 主密钥格式无效".to_string())?;
    Ok(fernet.encrypt(plaintext.as_bytes()))
}

pub fn decrypt_secret(master_key: &str, ciphertext: &str) -> Result<String, String> {
    if ciphertext.is_empty() {
        return Ok(String::new());
    }
    let key = fernet_key(master_key)?;
    let fernet = fernet::Fernet::new(&key).ok_or_else(|| "Fernet 主密钥格式无效".to_string())?;
    let plaintext = fernet
        .decrypt(ciphertext)
        .map_err(|_| "凭证解密失败：主密钥与密文不匹配".to_string())?;
    String::from_utf8(plaintext).map_err(|_| "凭证不是有效的 UTF-8".to_string())
}

fn fernet_key(master_key: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(master_key.trim())
        .map_err(|e| format!("主密钥不是有效的 URL-safe Base64: {e}"))?;
    if bytes.len() != 32 {
        return Err("Fernet 主密钥必须解码为 32 字节".into());
    }
    Ok(master_key.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypts_legacy_compatible_fernet_ciphertext() {
        let key = "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=";
        let fernet = fernet::Fernet::new(key).unwrap();
        let token = fernet.encrypt(b"fixture-secret");
        assert_eq!(decrypt_secret(key, &token).unwrap(), "fixture-secret");
    }

    #[test]
    fn rejects_invalid_master_key_and_ciphertext() {
        assert!(decrypt_secret("invalid", "token").is_err());
        let key = "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=";
        assert!(decrypt_secret(key, "not-a-token").is_err());
    }

    #[test]
    fn empty_ciphertext_is_an_unconfigured_secret() {
        let key = "MDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDA=";
        assert_eq!(decrypt_secret(key, "").unwrap(), "");
    }
}
