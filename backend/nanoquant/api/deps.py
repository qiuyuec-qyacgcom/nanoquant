"""FastAPI 依赖: 鉴权."""
from __future__ import annotations

from fastapi import Header, HTTPException, status

from ..security import verify_token
from ..services.settings import get_security


def current_user(authorization: str = Header(default="")) -> str:
    cfg = get_security()
    if not cfg.require_login:
        return cfg.username
    token = authorization.removeprefix("Bearer ").strip()
    payload = verify_token(token)
    if not payload:
        raise HTTPException(status_code=status.HTTP_401_UNAUTHORIZED, detail="登录已失效, 请重新登录")
    return str(payload.get("u", "admin"))
