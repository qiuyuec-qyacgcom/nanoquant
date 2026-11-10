"""ORM 模型.

约定: 时间统一存 UTC 秒级时间戳 (INTEGER), 前端负责本地化;
需要灵活结构的字段统一用 JSON 列, 便于演进而不改表.
"""
from __future__ import annotations

from sqlalchemy import JSON, Boolean, Float, Index, Integer, String, Text, UniqueConstraint
from sqlalchemy.orm import Mapped, mapped_column

from .db import Base


def now_ts() -> int:
    import time

    return int(time.time())


class Setting(Base):
    """分组配置 KV: value 为 JSON 字符串."""

    __tablename__ = "app_settings"
    key: Mapped[str] = mapped_column(String(64), primary_key=True)
    value: Mapped[str] = mapped_column(Text, default="{}")
    updated_at: Mapped[int] = mapped_column(Integer, default=now_ts, onupdate=now_ts)


class EngineState(Base):
    """引擎运行态键值 (enabled / mode / halt 原因 / 模拟盘余额等)."""

    __tablename__ = "engine_state"
    key: Mapped[str] = mapped_column(String(64), primary_key=True)
    value: Mapped[str] = mapped_column(Text, default="")
    updated_at: Mapped[int] = mapped_column(Integer, default=now_ts, onupdate=now_ts)


class EngineEvent(Base):
    """运行事件 / 审计日志."""

    __tablename__ = "engine_events"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ts: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    level: Mapped[str] = mapped_column(String(16), default="info", index=True)
    category: Mapped[str] = mapped_column(String(32), default="engine", index=True)
    message: Mapped[str] = mapped_column(Text, default="")
    payload: Mapped[dict | None] = mapped_column(JSON, nullable=True)


class TradingSymbol(Base):
    """标的池条目 (规则与规格迁移自 astra-quant-agent 的 instrument_pool).

    池内所有合约规格都来自**添加当时的币安在线合约表**, 之后不再依赖行情接口:
    - tier / sl_atr_mult: 分层风控参数
    - precision / tick_sz / min_sz / min_notional: 下单精度与最小约束
    - ct_val: 币安恒为 1 (按币数下单), 保留字段以对齐 astra 的池结构
    """

    __tablename__ = "trading_symbols"
    symbol: Mapped[str] = mapped_column(String(32), primary_key=True)
    name: Mapped[str] = mapped_column(String(32), default="")
    tier: Mapped[str] = mapped_column(String(24), default="tier_2_momentum")
    sl_atr_mult: Mapped[float] = mapped_column(Float, default=2.2)
    enabled: Mapped[bool] = mapped_column(Boolean, default=True)
    max_leverage: Mapped[int] = mapped_column(Integer, default=20)
    precision: Mapped[int] = mapped_column(Integer, default=3)
    tick_sz: Mapped[str] = mapped_column(String(32), default="0.01")
    min_sz: Mapped[str] = mapped_column(String(32), default="0.001")
    min_notional: Mapped[float] = mapped_column(Float, default=5.0)
    ct_val: Mapped[float] = mapped_column(Float, default=1.0)
    min_score: Mapped[float] = mapped_column(Float, default=0.0)
    note: Mapped[str] = mapped_column(String(255), default="")
    #: 添加顺序 (1,2,3...). 前台 K 线、引擎扫描、池列表都按它排序,
    #: 避免同一秒内批量添加时 created_at 相同导致顺序不稳定.
    sort_order: Mapped[int] = mapped_column(Integer, default=0, index=True)
    created_at: Mapped[int] = mapped_column(Integer, default=now_ts)
    updated_at: Mapped[int] = mapped_column(Integer, default=now_ts, onupdate=now_ts)


class AiProvider(Base):
    """AI 大模型 API 接入配置 (key 加密存储)."""

    __tablename__ = "ai_providers"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    name: Mapped[str] = mapped_column(String(64), unique=True)
    protocol: Mapped[str] = mapped_column(String(32), default="openai_chat")
    base_url: Mapped[str] = mapped_column(String(255), default="https://api.openai.com/v1")
    api_key_enc: Mapped[str] = mapped_column(Text, default="")
    model: Mapped[str] = mapped_column(String(128), default="gpt-4o-mini")
    temperature: Mapped[float] = mapped_column(Float, default=0.2)
    #: 推理型模型会先把预算花在 thinking 上，900/1200 常被截断导致 JSON 不完整
    max_tokens: Mapped[int] = mapped_column(Integer, default=3000)
    #: 思考强度: auto/none/minimal/low/medium/high/xhigh/max（映射到 thinking 或 reasoning_effort）
    reasoning_effort: Mapped[str] = mapped_column(String(16), default="auto")
    timeout_sec: Mapped[int] = mapped_column(Integer, default=45)
    json_mode: Mapped[bool] = mapped_column(Boolean, default=True)
    enabled: Mapped[bool] = mapped_column(Boolean, default=True)
    priority: Mapped[int] = mapped_column(Integer, default=100)
    role: Mapped[str] = mapped_column(String(32), default="primary")
    extra: Mapped[dict | None] = mapped_column(JSON, nullable=True)
    created_at: Mapped[int] = mapped_column(Integer, default=now_ts)
    updated_at: Mapped[int] = mapped_column(Integer, default=now_ts, onupdate=now_ts)


class AiCallLog(Base):
    """每次模型调用留痕, 用于成本与诊断."""

    __tablename__ = "ai_call_logs"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ts: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    provider: Mapped[str] = mapped_column(String(64), default="")
    model: Mapped[str] = mapped_column(String(128), default="")
    purpose: Mapped[str] = mapped_column(String(32), default="decision", index=True)
    ok: Mapped[bool] = mapped_column(Boolean, default=True)
    latency_ms: Mapped[int] = mapped_column(Integer, default=0)
    prompt_tokens: Mapped[int] = mapped_column(Integer, default=0)
    completion_tokens: Mapped[int] = mapped_column(Integer, default=0)
    cost_usd: Mapped[float] = mapped_column(Float, default=0.0)
    error: Mapped[str] = mapped_column(String(500), default="")
    #: 模型原始输出（v2 为紧凑英文 JSON），审计可追溯转译前的原文
    raw_text: Mapped[str] = mapped_column(Text, default="")


class Signal(Base):
    """每轮扫描产生的量化信号与最终裁决."""

    __tablename__ = "signals"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ts: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    symbol: Mapped[str] = mapped_column(String(32), index=True)
    direction: Mapped[str] = mapped_column(String(8), default="flat")
    score: Mapped[float] = mapped_column(Float, default=0.0)
    features: Mapped[dict | None] = mapped_column(JSON, nullable=True)
    ai_action: Mapped[str] = mapped_column(String(16), default="")
    ai_confidence: Mapped[float] = mapped_column(Float, default=0.0)
    ai_reason: Mapped[str] = mapped_column(Text, default="")
    decision: Mapped[str] = mapped_column(String(16), default="skip", index=True)
    reject_reason: Mapped[str] = mapped_column(String(255), default="")
    #: 产生该信号时的交易环境: 模拟盘/实盘的信号不能混在同一条审计流里
    mode: Mapped[str] = mapped_column(String(8), default="demo", index=True)
    #: 同标的同方向的 flat 信号在 60s 窗口内合并计数，避免每轮扫描都写一行
    repeats: Mapped[int] = mapped_column(Integer, default=1)


class OrderRecord(Base):
    """委托流水 (实盘与模拟盘共用)."""

    __tablename__ = "orders"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ts: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    symbol: Mapped[str] = mapped_column(String(32), index=True)
    side: Mapped[str] = mapped_column(String(8))
    order_type: Mapped[str] = mapped_column(String(24))
    qty: Mapped[float] = mapped_column(Float, default=0.0)
    price: Mapped[float] = mapped_column(Float, default=0.0)
    status: Mapped[str] = mapped_column(String(24), default="new")
    purpose: Mapped[str] = mapped_column(String(24), default="entry")
    client_order_id: Mapped[str] = mapped_column(String(64), default="", index=True)
    exchange_order_id: Mapped[str] = mapped_column(String(64), default="")
    reduce_only: Mapped[bool] = mapped_column(Boolean, default=False)
    mode: Mapped[str] = mapped_column(String(8), default="demo")
    #: 下单到交易所回执的耗时 (毫秒), 执行质量观测
    latency_ms: Mapped[int] = mapped_column(Integer, default=0)
    raw: Mapped[dict | None] = mapped_column(JSON, nullable=True)


class PositionRecord(Base):
    """仓位快照/模拟盘仓位 (status=open 时即当前持仓)."""

    __tablename__ = "positions"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    symbol: Mapped[str] = mapped_column(String(32), index=True)
    side: Mapped[str] = mapped_column(String(8))  # long / short
    qty: Mapped[float] = mapped_column(Float)
    entry_price: Mapped[float] = mapped_column(Float)
    mark_price: Mapped[float] = mapped_column(Float, default=0.0)
    #: 交易所 positionRisk 的权威未实现盈亏与同步时刻 (优先于本地按最新成交价估算)
    venue_unrealized: Mapped[float] = mapped_column(Float, default=0.0)
    venue_ts: Mapped[int] = mapped_column(Integer, default=0)
    leverage: Mapped[float] = mapped_column(Float, default=1.0)
    margin: Mapped[float] = mapped_column(Float, default=0.0)
    sl_price: Mapped[float] = mapped_column(Float, default=0.0)
    tp_price: Mapped[float] = mapped_column(Float, default=0.0)
    peak_price: Mapped[float] = mapped_column(Float, default=0.0)
    opened_at: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    closed_at: Mapped[int] = mapped_column(Integer, default=0)
    exit_price: Mapped[float] = mapped_column(Float, default=0.0)
    exit_reason: Mapped[str] = mapped_column(String(32), default="")
    realized_pnl: Mapped[float] = mapped_column(Float, default=0.0)
    fees: Mapped[float] = mapped_column(Float, default=0.0)
    ai_confidence: Mapped[float] = mapped_column(Float, default=0.0)
    ai_reason: Mapped[str] = mapped_column(Text, default="")
    signal_score: Mapped[float] = mapped_column(Float, default=0.0)
    features: Mapped[dict | None] = mapped_column(JSON, nullable=True)
    #: 入场滑点 (bps): (成交均价 - 决策价)/决策价 * 10000, 方向已校正
    entry_slippage_bps: Mapped[float] = mapped_column(Float, default=0.0)
    #: 开仓时的初始止损价, 用于计算 R 倍数 (移动止损后 sl_price 会变, 不能拿来算 R)
    initial_sl_price: Mapped[float] = mapped_column(Float, default=0.0)
    #: 部分止盈执行时间 (0=未执行); 保证同一仓位只触发一次
    partial_taken_at: Mapped[int] = mapped_column(Integer, default=0)
    mode: Mapped[str] = mapped_column(String(8), default="demo", index=True)
    status: Mapped[str] = mapped_column(String(16), default="open", index=True)
    trade_id: Mapped[int] = mapped_column(Integer, default=0, index=True)


class SignalFollow(Base):
    """外部频道信号跟单记录（幂等 + 审计）.

    `signal_id` 唯一：同一条信号无论文件重读、进程重启还是 HTTP 重推，都只处理一次。
    """

    __tablename__ = "signal_follows"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    signal_id: Mapped[str] = mapped_column(String(64), unique=True, index=True)
    symbol: Mapped[str] = mapped_column(String(32), index=True)
    side: Mapped[str] = mapped_column(String(8), default="")
    status: Mapped[str] = mapped_column(String(16), default="opened", index=True)
    reason: Mapped[str] = mapped_column(String(240), default="")
    leverage: Mapped[int] = mapped_column(Integer, default=0)
    qty: Mapped[float] = mapped_column(Float, default=0.0)
    price: Mapped[float] = mapped_column(Float, default=0.0)
    margin: Mapped[float] = mapped_column(Float, default=0.0)
    mode: Mapped[str] = mapped_column(String(8), default="demo", index=True)
    payload: Mapped[dict | None] = mapped_column(JSON, nullable=True)
    created_at: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)


class TradeRecord(Base):
    """闭环交易 (用于复盘/统计/心法)."""

    __tablename__ = "trades"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    symbol: Mapped[str] = mapped_column(String(32), index=True)
    side: Mapped[str] = mapped_column(String(8))
    qty: Mapped[float] = mapped_column(Float)
    entry_price: Mapped[float] = mapped_column(Float)
    exit_price: Mapped[float] = mapped_column(Float)
    leverage: Mapped[float] = mapped_column(Float, default=1.0)
    notional: Mapped[float] = mapped_column(Float, default=0.0)
    pnl: Mapped[float] = mapped_column(Float, default=0.0)
    pnl_pct: Mapped[float] = mapped_column(Float, default=0.0)
    roi_pct: Mapped[float] = mapped_column(Float, default=0.0)
    fees: Mapped[float] = mapped_column(Float, default=0.0)
    opened_at: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    closed_at: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    hold_seconds: Mapped[int] = mapped_column(Integer, default=0)
    exit_reason: Mapped[str] = mapped_column(String(32), default="")
    ai_confidence: Mapped[float] = mapped_column(Float, default=0.0)
    ai_reason: Mapped[str] = mapped_column(Text, default="")
    signal_score: Mapped[float] = mapped_column(Float, default=0.0)
    #: 入场滑点 (bps), 从持仓复制, 执行质量观测
    slippage_bps: Mapped[float] = mapped_column(Float, default=0.0)
    mode: Mapped[str] = mapped_column(String(8), default="demo", index=True)
    reviewed: Mapped[bool] = mapped_column(Boolean, default=False, index=True)
    review_note: Mapped[str] = mapped_column(Text, default="")
    features: Mapped[dict | None] = mapped_column(JSON, nullable=True)
    mantra_ids: Mapped[list | None] = mapped_column(JSON, nullable=True)


class AlertLog(Base):
    """告警发送记录: 用来确认通道是否真的通, 而不是只看配置。"""

    __tablename__ = "alert_logs"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ts: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    level: Mapped[str] = mapped_column(String(8), default="warn", index=True)
    category: Mapped[str] = mapped_column(String(48), default="")
    message: Mapped[str] = mapped_column(Text, default="")
    #: 命中的通道, 如 "webhook,telegram"
    channels: Mapped[str] = mapped_column(String(64), default="")
    ok: Mapped[bool] = mapped_column(Boolean, default=False, index=True)
    error: Mapped[str] = mapped_column(Text, default="")


class WriteFailure(Base):
    """异步写库失败的死信记录 (可重放).

    订单/持仓的落库是异步的: 若最终失败而不留痕, 本地账本就会与交易所不一致,
    所以失败必须落到这里, 让面板可见、可手动重放。
    """

    __tablename__ = "write_failures"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ts: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    #: 写入函数名 (_persist_entry / _persist_close ...)
    kind: Mapped[str] = mapped_column(String(48), default="")
    #: 调用参数 (JSON: {"args": [...], "kwargs": {...}}), 供重放使用
    payload: Mapped[dict | None] = mapped_column(JSON, nullable=True)
    error: Mapped[str] = mapped_column(Text, default="")
    attempts: Mapped[int] = mapped_column(Integer, default=0)
    #: 自动重放次数; 超过上限就不再自动尝试, 避免无限刷
    auto_attempts: Mapped[int] = mapped_column(Integer, default=0)
    resolved: Mapped[bool] = mapped_column(Boolean, default=False, index=True)


class Mantra(Base):
    """分析心法: 可被新心法优化/替代的经验条目."""

    __tablename__ = "mantras"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    title: Mapped[str] = mapped_column(String(120))
    rule: Mapped[str] = mapped_column(Text)
    tags: Mapped[list | None] = mapped_column(JSON, nullable=True)
    category: Mapped[str] = mapped_column(String(32), default="entry", index=True)
    source: Mapped[str] = mapped_column(String(32), default="trade_review")
    confidence: Mapped[float] = mapped_column(Float, default=0.5)
    health: Mapped[float] = mapped_column(Float, default=0.5)
    hits: Mapped[int] = mapped_column(Integer, default=0)
    wins: Mapped[int] = mapped_column(Integer, default=0)
    losses: Mapped[int] = mapped_column(Integer, default=0)
    enabled: Mapped[bool] = mapped_column(Boolean, default=True, index=True)
    visible: Mapped[bool] = mapped_column(Boolean, default=True)
    version: Mapped[int] = mapped_column(Integer, default=1)
    superseded_by: Mapped[int] = mapped_column(Integer, default=0)
    evidence: Mapped[dict | None] = mapped_column(JSON, nullable=True)
    created_at: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    updated_at: Mapped[int] = mapped_column(Integer, default=now_ts, onupdate=now_ts)
    expires_at: Mapped[int] = mapped_column(Integer, default=0)


class EvolutionRun(Base):
    """一次自进化执行记录."""

    __tablename__ = "evolution_runs"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ts: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    kind: Mapped[str] = mapped_column(String(24), default="trade_review")
    input_count: Mapped[int] = mapped_column(Integer, default=0)
    produced: Mapped[int] = mapped_column(Integer, default=0)
    replaced: Mapped[int] = mapped_column(Integer, default=0)
    ok: Mapped[bool] = mapped_column(Boolean, default=True)
    summary: Mapped[str] = mapped_column(Text, default="")
    raw: Mapped[dict | None] = mapped_column(JSON, nullable=True)


class NewsItem(Base):
    """资讯条目 (多源聚合)."""

    __tablename__ = "news_items"
    __table_args__ = (UniqueConstraint("dedupe", name="uq_news_dedupe"),)
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ts: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    source: Mapped[str] = mapped_column(String(48), default="", index=True)
    title: Mapped[str] = mapped_column(String(400))
    url: Mapped[str] = mapped_column(String(800), default="")
    summary: Mapped[str] = mapped_column(Text, default="")
    sentiment: Mapped[float] = mapped_column(Float, default=0.0)
    symbols: Mapped[list | None] = mapped_column(JSON, nullable=True)
    dedupe: Mapped[str] = mapped_column(String(64), index=True)
    raw: Mapped[dict | None] = mapped_column(JSON, nullable=True)


class BackupJob(Base):
    """备份任务产物."""

    __tablename__ = "backup_jobs"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ts: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    name: Mapped[str] = mapped_column(String(128), default="")
    scope: Mapped[dict | None] = mapped_column(JSON, nullable=True)
    file_path: Mapped[str] = mapped_column(String(400), default="")
    size: Mapped[int] = mapped_column(Integer, default=0)
    sha256: Mapped[str] = mapped_column(String(80), default="")
    encrypted: Mapped[bool] = mapped_column(Boolean, default=False)
    status: Mapped[str] = mapped_column(String(16), default="ok")
    message: Mapped[str] = mapped_column(Text, default="")
    created_by: Mapped[str] = mapped_column(String(32), default="system")


class EquityPoint(Base):
    """权益曲线采样."""

    __tablename__ = "equity_points"
    id: Mapped[int] = mapped_column(Integer, primary_key=True, autoincrement=True)
    ts: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    mode: Mapped[str] = mapped_column(String(8), default="demo", index=True)
    wallet: Mapped[float] = mapped_column(Float, default=0.0)
    available: Mapped[float] = mapped_column(Float, default=0.0)
    unrealized: Mapped[float] = mapped_column(Float, default=0.0)
    margin_used: Mapped[float] = mapped_column(Float, default=0.0)
    positions: Mapped[int] = mapped_column(Integer, default=0)


class Cooldown(Base):
    """标的冷却锁."""

    __tablename__ = "cooldowns"
    symbol: Mapped[str] = mapped_column(String(32), primary_key=True)
    until_ts: Mapped[int] = mapped_column(Integer, default=0)
    reason: Mapped[str] = mapped_column(String(120), default="")
    kind: Mapped[str] = mapped_column(String(24), default="symbol")


class PendingOrder(Base):
    """下单前落盘的幂等台账: 结果未知时靠查询收敛, 不盲目重发."""

    __tablename__ = "pending_orders"
    client_order_id: Mapped[str] = mapped_column(String(64), primary_key=True)
    symbol: Mapped[str] = mapped_column(String(32), index=True)
    payload: Mapped[dict | None] = mapped_column(JSON, nullable=True)
    state: Mapped[str] = mapped_column(String(24), default="submitting", index=True)
    created_at: Mapped[int] = mapped_column(Integer, default=now_ts, index=True)
    updated_at: Mapped[int] = mapped_column(Integer, default=now_ts, onupdate=now_ts)
    message: Mapped[str] = mapped_column(String(255), default="")


Index("ix_positions_open", PositionRecord.status, PositionRecord.symbol)
Index("ix_trades_mode_closed", TradeRecord.mode, TradeRecord.closed_at)
Index("ix_news_ts_source", NewsItem.ts, NewsItem.source)
