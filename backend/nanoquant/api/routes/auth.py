"""登录与账号."""
from __future__ import annotations

from fastapi import APIRouter, Depends, HTTPException

from ...security import hash_password, issue_token, verify_password
from ...services import state
from ...services.settings import get_security, update_group
from ..deps import current_user
from ..schemas import LoginIn, PasswordIn

router = APIRouter(prefix="/auth", tags=["auth"])


@router.post("/login")
def login(payload: LoginIn) -> dict:
    cfg = get_security()
    fails = state.get_int("login_fails", 0)
    if fails >= cfg.login_fail_limit:
        raise HTTPException(status_code=429, detail="失败次数过多, 请稍后再试或手动重置密码")
    if payload.username != cfg.username or not verify_password(payload.password, cfg.password_hash):
        state.set("login_fails", str(fails + 1))
        raise HTTPException(status_code=401, detail="用户名或密码错误")
    state.set("login_fails", "0")
    token, exp = issue_token(payload.username, cfg.session_ttl_hours)
    return {
        "token": token,
        "expires_at": exp,
        "username": payload.username,
        "must_change_password": payload.password == "admin123",
    }


@router.get("/me")
def me(user: str = Depends(current_user)) -> dict:
    cfg = get_security()
    return {
        "username": user,
        "require_login": cfg.require_login,
        "session_ttl_hours": cfg.session_ttl_hours,
    }


@router.post("/password")
def change_password(payload: PasswordIn, user: str = Depends(current_user)) -> dict:
    cfg = get_security()
    if not verify_password(payload.old_password, cfg.password_hash):
        raise HTTPException(status_code=400, detail="原密码不正确")
    update_group("security", {"password_hash": hash_password(payload.new_password)})
    token, exp = issue_token(user, cfg.session_ttl_hours)
    return {"ok": True, "token": token, "expires_at": exp}
