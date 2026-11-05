"""认证与密钥加密.

- 管理端密码: PBKDF2-HMAC-SHA256 + 随机盐, 存于 security 配置组
- 会话令牌: 无状态签名令牌 (HMAC-SHA256), 不落库
- API Key: Fernet 对称加密, 主密钥首次启动生成于 data/.master_key
"""
from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import secrets
import time

from cryptography.fernet import Fernet, InvalidToken

from .config import SECRET_SALT, paths

_PBKDF2_ITER = 120_000


# ---------------------------------------------------------------- master key
def _master_key() -> bytes:
    paths.ensure()
    if paths.MASTER_KEY.exists():
        raw = paths.MASTER_KEY.read_bytes().strip()
    else:
        raw = Fernet.generate_key()
        paths.MASTER_KEY.write_bytes(raw)
        try:
            os.chmod(paths.MASTER_KEY, 0o600)
        except OSError:  # pragma: no cover - Windows 部分文件系统不支持
            pass
    return raw


def _fernet() -> Fernet:
    return Fernet(_master_key())


def encrypt_secret(plain: str) -> str:
    if not plain:
        return ""
    return _fernet().encrypt(plain.encode("utf-8")).decode("ascii")


def decrypt_secret(cipher: str) -> str:
    if not cipher:
        return ""
    try:
        return _fernet().decrypt(cipher.encode("ascii")).decode("utf-8")
    except (InvalidToken, ValueError):
        return ""


def mask_secret(plain: str) -> str:
    if not plain:
        return ""
    if len(plain) <= 8:
        return "*" * len(plain)
    return f"{plain[:4]}{'*' * 6}{plain[-4:]}"


# ------------------------------------------------------------------ password
def hash_password(password: str, *, salt: str | None = None) -> str:
    salt = salt or secrets.token_hex(16)
    digest = hashlib.pbkdf2_hmac("sha256", password.encode("utf-8"), salt.encode("utf-8"), _PBKDF2_ITER)
    return f"pbkdf2_sha256${_PBKDF2_ITER}${salt}${digest.hex()}"


def verify_password(password: str, stored: str) -> bool:
    if not stored:
        return False
    try:
        algo, iters, salt, digest = stored.split("$", 3)
    except ValueError:
        return False
    if algo != "pbkdf2_sha256":
        return False
    calc = hashlib.pbkdf2_hmac("sha256", password.encode("utf-8"), salt.encode("utf-8"), int(iters))
    return hmac.compare_digest(calc.hex(), digest)


# -------------------------------------------------------------------- token
def _sign(payload: bytes) -> str:
    key = hashlib.sha256(_master_key() + SECRET_SALT.encode()).digest()
    return hmac.new(key, payload, hashlib.sha256).hexdigest()


def issue_token(username: str, ttl_hours: int) -> tuple[str, int]:
    exp = int(time.time()) + max(1, ttl_hours) * 3600
    body = {"u": username, "exp": exp, "n": secrets.token_hex(6)}
    raw = json.dumps(body, separators=(",", ":")).encode("utf-8")
    encoded = base64.urlsafe_b64encode(raw).decode("ascii").rstrip("=")
    return f"{encoded}.{_sign(encoded.encode())}", exp


def verify_token(token: str) -> dict | None:
    if not token or "." not in token:
        return None
    encoded, sig = token.rsplit(".", 1)
    if not hmac.compare_digest(_sign(encoded.encode()), sig):
        return None
    try:
        padded = encoded + "=" * (-len(encoded) % 4)
        body = json.loads(base64.urlsafe_b64decode(padded))
    except (ValueError, json.JSONDecodeError):
        return None
    if int(body.get("exp", 0)) < time.time():
        return None
    return body


def new_client_order_id(prefix: str = "nq") -> str:
    """最长 36 字符, 币安要求唯一."""
    return f"{prefix}{int(time.time() * 1000) % 10**10}{secrets.token_hex(4)}"
