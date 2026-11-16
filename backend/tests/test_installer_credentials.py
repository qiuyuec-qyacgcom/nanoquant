"""安装脚本选 Rust 内核时填写的 API 凭证：首次启动加密入库并删除明文.

安装阶段数据库还没建，凭证只能先留 `data/.exchange_credentials`（JSON，0600），
由 `bootstrap()` 消费。钉住：正确入库、明文必删、坏文件不阻断启动、缺字段不写空值。
"""
import json

import pytest

from nanoquant import main as main_mod
from nanoquant.config import paths


@pytest.fixture()
def cred_file(tmp_path, monkeypatch):
    """把 credentials 文件指到临时目录，避免污染真实 data/。"""
    fake_data = tmp_path
    monkeypatch.setattr(paths, "DATA", fake_data, raising=False)
    return fake_data / ".exchange_credentials"


def _write(path, payload):
    path.write_text(json.dumps(payload), encoding="utf-8")


def test_imports_credentials_and_deletes_plaintext(cred_file, monkeypatch):
    from nanoquant.services import settings as settings_mod

    captured: dict = {}
    monkeypatch.setattr(
        main_mod, "update_group", lambda group, patch: captured.update({group: patch})
    )
    _write(cred_file, {"env": "demo", "api_key": "KEY123", "api_secret": "SECRET456"})

    main_mod._import_installer_credentials()

    assert "data" in captured
    patch = captured["data"]
    assert "demo_api_key_enc" in patch and "demo_api_secret_enc" in patch
    # 加密后不应是明文
    assert patch["demo_api_key_enc"] != "KEY123"
    assert patch["demo_api_secret_enc"] != "SECRET456"
    assert not cred_file.exists(), "明文文件必须删除"


def test_live_env_maps_to_live_prefix(cred_file, monkeypatch):
    captured: dict = {}
    monkeypatch.setattr(
        main_mod, "update_group", lambda group, patch: captured.update({group: patch})
    )
    _write(cred_file, {"env": "live", "api_key": "K", "api_secret": "S"})

    main_mod._import_installer_credentials()

    patch = captured.get("data") or {}
    assert "live_api_key_enc" in patch and "live_api_secret_enc" in patch


def test_missing_file_is_noop(cred_file, monkeypatch):
    called = []
    monkeypatch.setattr(main_mod, "update_group", lambda *a, **k: called.append(a))
    main_mod._import_installer_credentials()  # 文件不存在
    assert called == []


def test_corrupt_file_is_deleted_without_raising(cred_file, monkeypatch):
    monkeypatch.setattr(main_mod, "update_group", lambda *a, **k: None)
    cred_file.write_text("{not json", encoding="utf-8")
    main_mod._import_installer_credentials()  # 不得抛异常
    assert not cred_file.exists(), "坏文件也应删除，避免下次启动反复告警"


def test_partial_credentials_skip_write(cred_file, monkeypatch):
    captured: dict = {}
    monkeypatch.setattr(
        main_mod, "update_group", lambda group, patch: captured.update({group: patch})
    )
    _write(cred_file, {"env": "demo", "api_key": "", "api_secret": ""})

    main_mod._import_installer_credentials()
    assert not captured, "缺字段时不得写入空凭证"
    assert not cred_file.exists()


def test_import_failure_keeps_file_for_retry(cred_file, monkeypatch):
    """入库失败时必须保留文件供下次启动重试——曾经"无论成败都删"，
    一次性的安装期凭证会被永久丢掉，用户只能在面板再填一遍。"""
    def _boom(group, patch):
        raise RuntimeError("db not ready")

    monkeypatch.setattr(main_mod, "update_group", _boom)
    _write(cred_file, {"env": "demo", "api_key": "KEY123", "api_secret": "SECRET456"})

    main_mod._import_installer_credentials()  # 不得抛异常

    assert cred_file.exists(), "入库失败时明文文件应保留（0600），下次启动重试"
