"""请求体模型."""
from __future__ import annotations

from typing import Any, Literal

from pydantic import BaseModel, Field


class LoginIn(BaseModel):
    username: str
    password: str


class PasswordIn(BaseModel):
    old_password: str
    new_password: str = Field(min_length=6, max_length=64)


class SettingsIn(BaseModel):
    patch: dict[str, Any]


class BacktestIn(BaseModel):
    """回测入参: 只覆盖纯量化部分, 费率与滑点显式传入."""

    symbol: str
    interval: str = "5m"
    limit: int = Field(default=1000, ge=200, le=1500)
    fee_bps: float = Field(default=5.0, ge=0, le=100)
    slippage_bps: float = Field(default=2.0, ge=0, le=100)
    initial_equity: float = Field(default=100.0, gt=0)


class BacktestGridIn(BacktestIn):
    """参数网格: 每个列表最多 4 个值, 组合数上限 48."""

    entry_threshold: list[float] = Field(default_factory=list, max_length=4)
    sl_atr_mult: list[float] = Field(default_factory=list, max_length=4)
    tp_atr_mult: list[float] = Field(default_factory=list, max_length=4)
    trailing_pct: list[float] = Field(default_factory=list, max_length=4)


class CredentialsIn(BaseModel):
    """交易所凭证: 模拟盘与实盘各存一套."""

    env: Literal["demo", "live"] = "live"
    api_key: str = ""
    api_secret: str = ""
    proxy: str | None = None


class ProviderIn(BaseModel):
    name: str = Field(min_length=1, max_length=64)
    protocol: str = "openai_chat"
    base_url: str
    api_key: str = ""
    model: str = "gpt-4o-mini"
    temperature: float = 0.2
    max_tokens: int = 900
    timeout_sec: int = 45
    json_mode: bool = True
    enabled: bool = True
    priority: int = 100
    role: str = "primary"
    extra: dict[str, Any] | None = None


class ProviderPatch(BaseModel):
    name: str | None = None
    protocol: str | None = None
    base_url: str | None = None
    api_key: str | None = None
    model: str | None = None
    temperature: float | None = None
    max_tokens: int | None = None
    timeout_sec: int | None = None
    json_mode: bool | None = None
    enabled: bool | None = None
    priority: int | None = None
    role: str | None = None
    extra: dict[str, Any] | None = None


class SymbolIn(BaseModel):
    """加入标的池: 只接受币安合约名, 其余规格由交易所在线合约表派生."""

    symbol: str


class SymbolBulkIn(BaseModel):
    symbols: list[str] = Field(min_length=1, max_length=100)


class SymbolPatch(BaseModel):
    enabled: bool | None = None
    max_leverage: int | None = None
    min_score: float | None = None
    note: str | None = None


class SymbolDeleteIn(BaseModel):
    """删除标的必须带确认短语 REMOVE <SYMBOL> (astra 规则)."""

    confirmation: str = ""


class EngineControl(BaseModel):
    enabled: bool | None = None
    # demo=模拟盘 / live=实盘; 兼容旧值 paper(会被归一化为 demo)
    mode: str | None = None
    # 切到实盘必须输入确认短语 LIVE
    confirm: str = ""


class AnalyzeIn(BaseModel):
    symbol: str
    interval: str = "1m"
    include_news: bool = True


class MantraIn(BaseModel):
    title: str
    rule: str
    category: str = "entry"
    confidence: float = 0.55
    tags: list[str] | None = None


class MantraPatch(BaseModel):
    title: str | None = None
    rule: str | None = None
    category: str | None = None
    confidence: float | None = None
    enabled: bool | None = None
    tags: list[str] | None = None


class BackupIn(BaseModel):
    name: str = ""
    settings_groups: list[str] | None = None
    tables: list[str] | None = None
    files: list[str] | None = None
    encrypt: bool | None = None


class RestoreIn(BaseModel):
    restore_settings: bool = True
    restore_tables: bool = True


class ClosePositionIn(BaseModel):
    reason: str = "manual"


class TestProviderIn(BaseModel):
    prompt: str = "只回答两个字: 正常"
