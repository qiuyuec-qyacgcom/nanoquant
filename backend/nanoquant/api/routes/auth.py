"""登录与账号."""
from __future__ import annotations

import time

from fastapi import APIRouter, Depends, HTTPException, Request

from ...security import hash_password, issue_token, verify_password
from ...services.settings import get_security, update_group
from ..deps import current_user
from ..schemas import LoginIn, PasswordIn

router = APIRouter(prefix="/auth", tags=["auth"])

#: 按客户端 IP 记录的登录失败时间戳（进程内滑动窗口）。
#: 之前用一个全局 state 计数，任何匿名者刷 login_fail_limit 个错密码就能把
#: 管理员锁在外面；现在失败按 IP 隔离、且超窗自动清除，单点不再被拖垮。
_login_fails: dict[str, list[float]] = {}
#: 字典大小安全阀：超过就清一次过期项，防伪造大量 IP 撑内存
_MAX_IP_TRACKED = 5000


def _prune_login_fails() -> None:
    if len(_login_fails) < _MAX_IP_TRACKED:
        return
    now = time.time()
    for ip in list(_login_fails):
        stamps = _login_fails[ip]
        if not stamps or now - stamps[-1] > 3600:
            del _login_fails[ip]


@router.post("/login")
def login(payload: LoginIn, request: Request) -> dict:
    cfg = get_security()
    ip = request.client.host if request.client else "unknown"
    now = time.time()
    recent = [t for t in _login_fails.get(ip, []) if now - t < cfg.login_fail_window_sec]
    if len(recent) >= cfg.login_fail_limit:
        _login_fails[ip] = recent
        minutes = max(1, cfg.login_fail_window_sec // 60)
        raise HTTPException(status_code=429, detail=f"失败次数过多, 请 {minutes} 分钟后再试")
    if payload.username != cfg.username or not verify_password(payload.password, cfg.password_hash):
        recent.append(now)
        _login_fails[ip] = recent
        _prune_login_fails()
        raise HTTPException(status_code=401, detail="用户名或密码错误")
    _login_fails.pop(ip, None)
    token, exp = issue_token(payload.username, cfg.session_ttl_hours, cfg.token_version)
    return {
        "token": token,
        "expires_at": exp,
        "username": payload.username,
        "must_change_password": cfg.password_is_default,
    }


@router.get("/me")
def me(user: str = Depends(current_user)) -> dict:
    cfg = get_security()
    return {
        "username": user,
        "require_login": cfg.require_login,
        "session_ttl_hours": cfg.session_ttl_hours,
        "must_change_password": cfg.password_is_default,
    }


@router.post("/password")
def change_password(payload: PasswordIn, user: str = Depends(current_user)) -> dict:
    cfg = get_security()
    if not verify_password(payload.old_password, cfg.password_hash):
        raise HTTPException(status_code=400, detail="原密码不正确")
    if payload.new_password == payload.old_password:
        raise HTTPException(status_code=400, detail="新密码不能与原密码相同")
    # password_is_default 落 False: 改过一次就说明这台机器不再是出厂状态
    # token_version +1: 旧的会话令牌从此作废，改密才真正能把人踢下线
    new_version = cfg.token_version + 1
    update_group(
        "security",
        {
            "password_hash": hash_password(payload.new_password),
            "password_is_default": False,
            "token_version": new_version,
        },
    )
    token, exp = issue_token(user, cfg.session_ttl_hours, new_version)
    return {"ok": True, "token": token, "expires_at": exp}
