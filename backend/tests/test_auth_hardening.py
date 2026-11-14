"""认证加固测试：登录限流按 IP + 时间窗口、改密吊销旧 token.

背景：旧实现用全局 state 计数登录失败，任何匿名者刷 login_fail_limit 个错密码
就能把管理员锁在外面（可远程触发的 DoS）；改密后旧 token 仍可用到过期，踢不掉人。
"""
from __future__ import annotations

import time

import pytest
from fastapi import HTTPException
from fastapi.testclient import TestClient

from nanoquant.api.routes import auth as auth_module

API = "/api/v1"


# ------------------------------------------------------------ 登录限流
class _FakeClient:
    def __init__(self, host: str) -> None:
        self.host = host


class _FakeRequest:
    def __init__(self, ip: str) -> None:
        self.client = _FakeClient(ip)


def _bad_login() -> "object":
    from nanoquant.api.schemas import LoginIn

    return LoginIn(username="admin", password="wrong-password")


def test_login_failures_are_counted_per_ip(monkeypatch):
    from nanoquant.services.settings import get_security

    monkeypatch.setattr(auth_module, "_login_fails", {})
    limit = get_security().login_fail_limit
    bad = _bad_login()

    # 同一 IP 连续失败到上限 → 401，之后再打 → 429
    for _ in range(limit):
        with pytest.raises(HTTPException) as exc:
            auth_module.login(bad, _FakeRequest("10.0.0.1"))
        assert exc.value.status_code == 401
    with pytest.raises(HTTPException) as exc:
        auth_module.login(bad, _FakeRequest("10.0.0.1"))
    assert exc.value.status_code == 429

    # 另一个 IP 不受第一个 IP 的失败影响
    with pytest.raises(HTTPException) as exc:
        auth_module.login(bad, _FakeRequest("10.0.0.2"))
    assert exc.value.status_code == 401


def test_login_fail_window_expires(monkeypatch):
    """窗口外的旧失败不计入，否则一次攻击后这个 IP 永远解不开."""
    from nanoquant.services.settings import get_security

    window = get_security().login_fail_window_sec
    # 塞满窗口外的"失败"（1 小时前），再失败一次仍应是 401 而非 429
    monkeypatch.setattr(
        auth_module,
        "_login_fails",
        {"10.0.0.9": [time.time() - window - 60] * 100},
    )
    with pytest.raises(HTTPException) as exc:
        auth_module.login(_bad_login(), _FakeRequest("10.0.0.9"))
    assert exc.value.status_code == 401


def test_successful_login_resets_counter(monkeypatch):
    from nanoquant.api.schemas import LoginIn

    monkeypatch.setattr(
        auth_module,
        "_login_fails",
        {"10.0.0.3": [time.time()] * 5},
    )
    auth_module.login(LoginIn(username="admin", password="admin123"), _FakeRequest("10.0.0.3"))
    assert "10.0.0.3" not in auth_module._login_fails


# ------------------------------------------------------------ 改密吊销旧 token
def test_verify_token_rejects_stale_version():
    from nanoquant.security import issue_token, verify_token

    token, _ = issue_token("admin", 12, version=3)
    assert verify_token(token, version=3) is not None
    assert verify_token(token, version=4) is None, "令牌代际落后必须失效"
    # 不传 version 不校验代际，保持向后兼容（调用方负责传）
    assert verify_token(token) is not None


@pytest.fixture()
def app_client():
    from nanoquant.main import bootstrap, create_app
    from nanoquant.security import hash_password
    from nanoquant.services.settings import get_security, update_group

    bootstrap()
    before = get_security()
    update_group(
        "security",
        {"password_hash": hash_password("admin123"), "password_is_default": True},
    )
    c = TestClient(create_app())
    try:
        yield c
    finally:
        c.close()
        update_group(
            "security",
            {"password_hash": before.password_hash, "password_is_default": False},
        )


def test_change_password_revokes_old_token(app_client):
    old = app_client.post(
        f"{API}/auth/login", json={"username": "admin", "password": "admin123"}
    ).json()["token"]
    old_headers = {"Authorization": f"Bearer {old}"}

    new_password = "Revoked-Old-Token-1"
    res = app_client.post(
        f"{API}/auth/password",
        json={"old_password": "admin123", "new_password": new_password},
        headers=old_headers,
    )
    assert res.status_code == 200, res.text
    new_token = res.json()["token"]

    # 旧 token 立即失效 —— 改密才真正把人踢下线
    assert app_client.get(f"{API}/auth/me", headers=old_headers).status_code == 401
    # 新 token 有效
    assert (
        app_client.get(
            f"{API}/auth/me", headers={"Authorization": f"Bearer {new_token}"}
        ).status_code
        == 200
    )
