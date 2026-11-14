"""出厂密码策略：未改密前，后台接口必须被拒.

背景：旧实现只在登录响应里回一个 ``must_change_password: true``，前端存下来
却没人用，后端也不拦——面板一旦映射到公网，admin123 就是常开后门。
现在由 main._force_password_change 中间件强制拦截。
"""
from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

API = "/api/v1"

#: 被拦的代表性路由（任意一个需要登录的读接口即可）
GUARDED = f"{API}/system/exec-sidecar"


@pytest.fixture()
def app_client():
    from nanoquant.main import bootstrap, create_app
    from nanoquant.security import hash_password
    from nanoquant.services.settings import get_security, update_group

    bootstrap()
    before = get_security()
    # 模拟"刚装好、还没改密码"的机器
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


def _login(client: TestClient, password: str = "admin123") -> dict:
    res = client.post(f"{API}/auth/login", json={"username": "admin", "password": password})
    assert res.status_code == 200, res.text
    return res.json()


def test_login_flags_default_password(app_client):
    body = _login(app_client)
    assert body["must_change_password"] is True


def test_guarded_route_blocked_with_clear_reason(app_client):
    token = _login(app_client)["token"]
    res = app_client.get(GUARDED, headers={"Authorization": f"Bearer {token}"})
    assert res.status_code == 403, res.text
    body = res.json()
    assert body["code"] == "password_change_required"
    assert "密码" in body["detail"]


def test_allowlisted_endpoints_still_work(app_client):
    """改密链路本身不能被自己挡住，否则永远出不来。"""
    token = _login(app_client)["token"]
    me = app_client.get(f"{API}/auth/me", headers={"Authorization": f"Bearer {token}"})
    assert me.status_code == 200
    assert me.json()["must_change_password"] is True
    assert app_client.get(f"{API}/health").status_code == 200


def test_public_site_not_blocked(app_client):
    """前台访客接口与后台权限无关，不该被出厂密码牵连。"""
    res = app_client.get(f"{API}/public/pool")
    assert res.status_code != 403


def test_reject_same_password(app_client):
    token = _login(app_client)["token"]
    res = app_client.post(
        f"{API}/auth/password",
        json={"old_password": "admin123", "new_password": "admin123"},
        headers={"Authorization": f"Bearer {token}"},
    )
    assert res.status_code == 400
    assert "相同" in res.json()["detail"]


def test_change_password_unlocks_everything(app_client):
    token = _login(app_client)["token"]
    new_password = "No-Longer-Default-9x"
    res = app_client.post(
        f"{API}/auth/password",
        json={"old_password": "admin123", "new_password": new_password},
        headers={"Authorization": f"Bearer {token}"},
    )
    assert res.status_code == 200, res.text
    fresh = res.json()["token"]

    # 旧密码失效、新密码可登录，且不再是"出厂状态"
    assert app_client.post(f"{API}/auth/login", json={"username": "admin", "password": "admin123"}).status_code == 401
    assert _login(app_client, new_password)["must_change_password"] is False

    headers = {"Authorization": f"Bearer {fresh}"}
    assert app_client.get(GUARDED, headers=headers).status_code != 403
    assert app_client.get(f"{API}/auth/me", headers=headers).json()["must_change_password"] is False


# ------------------------------------------------------------ 初始密码来源
# 首次启动的密码不再写死 admin123：
#   环境变量 > 安装脚本留下的 data/.admin_password > 随机生成（打印在启动日志）
def _reset_security():
    """把 security 恢复成"已配置好密码"的稳定状态，避免污染同进程的其它用例."""
    from nanoquant.security import hash_password
    from nanoquant.services.settings import update_group

    update_group(
        "security",
        {"password_hash": hash_password("admin123"), "password_is_default": False},
    )


def test_generate_password_shape():
    from nanoquant.security import generate_password

    pwd = generate_password()
    assert len(pwd) == 16
    # 去掉了 0/O、1/l/I 这类看不清的字符
    assert not set(pwd) & set("0O1lI")
    assert generate_password(8) and len(generate_password(8)) == 12, "长度下限是 12"
    assert len({generate_password() for _ in range(50)}) == 50, "不能退化成可预测的序列"


def test_bootstrap_generates_random_password(monkeypatch):
    from nanoquant.main import bootstrap
    from nanoquant.services.settings import get_security, update_group

    monkeypatch.delenv("NANOQUANT_ADMIN_PASSWORD", raising=False)
    monkeypatch.setattr("nanoquant.main.generate_password", lambda *a, **k: "Rand0mPassw0rd12")
    update_group("security", {"password_hash": "", "password_is_default": False})
    try:
        bootstrap()
        cfg = get_security()
        assert cfg.password_is_default is True, "随机初始密码仍要求首次登录后改掉"
        from nanoquant.security import verify_password

        assert verify_password("Rand0mPassw0rd12", cfg.password_hash) is True
        assert verify_password("admin123", cfg.password_hash) is False
    finally:
        _reset_security()


def test_bootstrap_uses_env_password(monkeypatch):
    from nanoquant.main import bootstrap
    from nanoquant.security import verify_password
    from nanoquant.services.settings import get_security, update_group

    monkeypatch.setenv("NANOQUANT_ADMIN_PASSWORD", "Env-Chosen-Pass-9")
    update_group("security", {"password_hash": "", "password_is_default": False})
    try:
        bootstrap()
        cfg = get_security()
        assert verify_password("Env-Chosen-Pass-9", cfg.password_hash) is True
        assert cfg.password_is_default is False, "运维自己选的密码不该被强制再改一次"
    finally:
        monkeypatch.delenv("NANOQUANT_ADMIN_PASSWORD", raising=False)
        _reset_security()


def test_bootstrap_reads_and_removes_password_file(monkeypatch):
    from nanoquant.config import paths
    from nanoquant.main import bootstrap
    from nanoquant.security import verify_password
    from nanoquant.services.settings import get_security, update_group

    monkeypatch.delenv("NANOQUANT_ADMIN_PASSWORD", raising=False)
    secret = paths.DATA / ".admin_password"
    secret.write_text("Installer-Set-Pass-7", encoding="utf-8")
    update_group("security", {"password_hash": "", "password_is_default": False})
    try:
        bootstrap()
        cfg = get_security()
        assert verify_password("Installer-Set-Pass-7", cfg.password_hash) is True
        assert cfg.password_is_default is False
        assert not secret.exists(), "明文密码文件用完必须删掉"
    finally:
        if secret.exists():
            secret.unlink()
        _reset_security()


def test_bootstrap_file_beats_random(monkeypatch):
    """文件与环境变量都没料时才随机，这里确认优先级：有文件就不走随机分支."""
    from nanoquant.config import paths
    from nanoquant.main import bootstrap
    from nanoquant.services.settings import get_security, update_group

    monkeypatch.delenv("NANOQUANT_ADMIN_PASSWORD", raising=False)
    calls = []
    monkeypatch.setattr("nanoquant.main.generate_password", lambda *a, **k: calls.append(1) or "x")
    secret = paths.DATA / ".admin_password"
    secret.write_text("From-File-Pass-123", encoding="utf-8")
    update_group("security", {"password_hash": "", "password_is_default": False})
    try:
        bootstrap()
        assert calls == []
        assert get_security().password_is_default is False
    finally:
        if secret.exists():
            secret.unlink()
        _reset_security()
