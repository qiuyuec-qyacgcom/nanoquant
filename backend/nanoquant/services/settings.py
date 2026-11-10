"""分组配置中心: 默认值 + Pydantic 校验 + DB 持久化 + 热更新.

每个分组一个模型; 后台只提交"要改的字段", 服务端做深合并后整体校验,
校验失败抛 SettingsValidationError, 由路由映射成 400.
"""
from __future__ import annotations

import json
import threading
import time
from typing import Any, Literal

from pydantic import BaseModel, Field, ValidationError, field_validator, model_validator

from ..db import session_scope
from ..logging_setup import get_logger
from ..models import Setting

log = get_logger("settings")


class SettingsValidationError(ValueError):
    def __init__(self, group: str, detail: str) -> None:
        super().__init__(f"{group}: {detail}")
        self.group = group
        self.detail = detail


# 交易环境: demo=币安模拟盘(测试网真实下单) / live=币安实盘
MODE_DEMO = "demo"
MODE_LIVE = "live"
#: 引擎轮询下限: 策略是 5m K 线, 低于 15s 只是徒增 REST 权重与信号行, 没有信息增量
MIN_CYCLE_INTERVAL_SEC = 15
_LEGACY_MODES = {"paper": MODE_DEMO, "sim": MODE_DEMO, "demo": MODE_DEMO, "live": MODE_LIVE}


def normalize_mode(mode: str | None, default: str = MODE_DEMO) -> str:
    """把历史遗留的 paper 值统一成 demo, 未知值回退默认."""
    return _LEGACY_MODES.get(str(mode or "").strip().lower(), default)


class EngineSettings(BaseModel):
    enabled: bool = False
    mode: Literal["demo", "live"] = MODE_DEMO
    cycle_interval_sec: int = Field(default=MIN_CYCLE_INTERVAL_SEC, ge=MIN_CYCLE_INTERVAL_SEC, le=120)
    max_positions: int = Field(default=2, ge=1, le=20)
    max_symbols_per_cycle: int = Field(default=12, ge=1, le=60)
    trading_windows: list[str] = Field(default_factory=lambda: ["00:00-24:00"])
    timezone_offset: int = Field(default=8, ge=-12, le=14, description="交易时段所用时区偏移")
    close_on_halt: bool = False
    order_type: Literal["market", "limit_post_only"] = "market"
    recv_window_ms: int = Field(default=5000, ge=1000, le=60000)

    @model_validator(mode="before")
    @classmethod
    def _accept_legacy(cls, data: Any) -> Any:
        """兼容旧库: mode=paper / paper_start_balance 自动迁移到模拟盘命名."""
        if not isinstance(data, dict):
            return data
        out = dict(data)
        out["mode"] = normalize_mode(out.get("mode"), MODE_DEMO)
        if "paper_start_balance" in out:
            out.setdefault("demo_start_balance", out.pop("paper_start_balance"))
        return out


class RiskSettings(BaseModel):
    risk_per_trade_pct: float = Field(default=10.0, gt=0, le=100, description="单笔最大亏损占权益%")
    max_leverage: int = Field(default=100, ge=1, le=100, description="杠杆上限: 全局硬上限, 最高 100x; AI 开仓杠杆与单标的配置都只能收紧不能突破")
    default_leverage: int = Field(
        default=20, ge=1, le=100,
        description="杠杆下限: 全局硬下限, 最低 1x; AI/外部信号给出的杠杆低于该值时自动抬到下限",
    )
    margin_pct: float = Field(default=50.0, gt=0, le=100, description="单仓保证金占权益%")
    max_exposure_pct: float = Field(default=2000.0, gt=0, le=20000, description="总名义额/权益 上限%")
    max_same_direction: int = Field(
        default=0, ge=0, le=20, description="同方向最多持仓数, 0=不限制（币种高度相关, 建议小资金设 1）"
    )
    vol_target_atr_pct: float = Field(
        default=0.0, ge=0.0, le=20.0, description="波动率目标(ATR%), 0=关闭；ATR 高则自动缩仓"
    )
    daily_loss_limit_pct: float = Field(default=25.0, gt=0, le=100)
    max_drawdown_pct: float = Field(default=50.0, gt=0, le=100)
    consecutive_loss_limit: int = Field(default=4, ge=1, le=20)
    cooldown_minutes: int = Field(default=10, ge=0, le=1440)
    symbol_cooldown_minutes: int = Field(default=5, ge=0, le=1440)
    #: 止损熔断 (StoplossGuard): 窗口内 N 次止损 → 全局暂停开仓 M 分钟; 0=关闭
    stoploss_guard_count: int = Field(default=3, ge=0, le=20, description="窗口内止损次数触发全局暂停, 0=关闭")
    stoploss_guard_window_min: int = Field(default=60, ge=1, le=720)
    stoploss_guard_pause_min: int = Field(default=30, ge=1, le=1440)
    max_slippage_pct: float = Field(default=0.25, ge=0, le=5)
    min_notional_buffer: float = Field(default=1.05, ge=1.0, le=3.0)
    min_equity_usdt: float = Field(default=0.5, ge=0.1, le=100)
    fee_rate_taker: float = Field(default=0.0005, ge=0, le=0.01)
    fee_rate_maker: float = Field(default=0.0002, ge=0, le=0.01)


class StrategySettings(BaseModel):
    #: 5–30 分钟短线系统: 主周期限定为 5m/15m/30m/1h
    timeframe: Literal["5m", "15m", "30m", "1h"] = "5m"
    ema_fast: int = Field(default=9, ge=2, le=200)
    ema_slow: int = Field(default=21, ge=3, le=400)
    rsi_period: int = Field(default=14, ge=2, le=100)
    atr_period: int = Field(default=14, ge=2, le=100)
    entry_threshold: float = Field(default=0.30, ge=0.05, le=0.95)
    exit_threshold: float = Field(default=0.10, ge=0.0, le=0.9)
    volume_ratio_min: float = Field(default=0.80, ge=0.1, le=10)
    atr_pct_min: float = Field(default=0.05, ge=0.0, le=10)
    atr_pct_max: float = Field(default=4.0, ge=0.1, le=50)
    min_24h_quote_volume: float = Field(default=3_000_000, ge=0)
    sl_atr_mult: float = Field(default=1.2, ge=0.2, le=10)
    tp_atr_mult: float = Field(default=2.0, ge=0.2, le=20)
    trailing_pct: float = Field(default=0.8, ge=0.0, le=20, description="0 表示关闭移动止损")
    breakeven_at_r: float = Field(default=1.0, ge=0.1, le=10)
    max_hold_minutes: int = Field(default=180, ge=1, le=2880)
    min_signal_gap: float = Field(default=0.05, ge=0.0, le=1.0)
    #: 部分止盈: 浮盈达到 N 倍初始风险 (R) 时先平掉一部分, 剩余交给移动止损; 0=关闭
    partial_tp_at_r: float = Field(default=1.0, ge=0, le=10)
    #: 部分止盈比例 (0,1]; 0.5 = 平一半
    partial_tp_ratio: float = Field(default=0.5, gt=0, le=1)
    #: 盘口点差闸门 (bps): 买一卖一价差超过该值就放弃开仓, 0=关闭.
    #: 短线的利润空间有限, 点差与滑点必须先扣掉.
    max_spread_bps: float = Field(default=8.0, ge=0, le=200, description="盘口点差闸门(bps), 0=关闭")
    #: 时间阶梯止盈 (freqtrade minimal_roi 思路): [[持仓分钟, 目标收益率%], ...].
    #: 5–30 分钟级别持仓更久, 阈值随持仓时长递减; 空列表=关闭.
    roi_table: list[list[float]] = Field(
        default_factory=lambda: [[0, 0.8], [15, 0.6], [30, 0.45], [60, 0.3], [120, 0.2]],
        description="时间阶梯止盈 [[分钟, 收益率%]]",
    )
    weight_trend: float = Field(default=0.32, ge=0, le=2)
    weight_momentum: float = Field(default=0.26, ge=0, le=2)
    weight_volume: float = Field(default=0.18, ge=0, le=2)
    weight_funding: float = Field(default=0.12, ge=0, le=2)
    weight_oi: float = Field(default=0.12, ge=0, le=2)

    @field_validator("roi_table")
    @classmethod
    def _check_roi_table(cls, value: list[list[float]]) -> list[list[float]]:
        """时间阶梯止盈: [[分钟, 收益率%]]; 归一化为按分钟升序, 最多 12 档."""
        rows: list[list[float]] = []
        for row in value or []:
            if len(row) != 2:
                raise ValueError("每档必须写成 [分钟, 收益率%] 两个数字")
            minutes, pct = float(row[0]), float(row[1])
            if minutes < 0 or minutes > 2880:
                raise ValueError("分钟必须在 0~2880 之间")
            if pct < 0 or pct > 50:
                raise ValueError("收益率必须在 0~50% 之间")
            rows.append([round(minutes, 2), round(pct, 4)])
        if len(rows) > 12:
            raise ValueError("时间阶梯止盈最多 12 档")
        rows.sort(key=lambda item: item[0])
        return rows


class AiPersona(BaseModel):
    name: str
    weight: float = Field(default=1.0, ge=0, le=5)
    focus: str = ""
    enabled: bool = True


class AiSettings(BaseModel):
    enabled: bool = True
    mode: Literal["single", "committee"] = "single"
    provider_id: int = 0
    fallback_provider_ids: list[int] = Field(default_factory=list)
    min_confidence: float = Field(default=0.45, ge=0, le=1)
    require_agreement: bool = True
    allow_ai_override: bool = Field(default=False, description="允许 AI 反向覆盖量化信号")
    max_calls_per_hour: int = Field(default=30, ge=0, le=600)
    monthly_budget_usd: float = Field(default=3.0, ge=0, le=10000)
    timeout_sec: int = Field(default=45, ge=5, le=300)
    temperature: float = Field(default=0.2, ge=0, le=2)
    #: 已停用（保留字段兼容老配置）：AI 开仓杠杆统一由「风控 → 杠杆下限/上限」钳制，
    #: 不再由模型侧单独设上限，也不跟随心法里的杠杆建议。
    max_position_leverage: int = Field(default=20, ge=1, le=100, description="已停用: 开仓杠杆由风控全局上下限决定")
    inject_news: bool = True
    inject_mantras: bool = True
    personas: list[AiPersona] = Field(
        default_factory=lambda: [
            AiPersona(name="趋势研判官", weight=0.35, focus="多周期趋势与关键价位"),
            AiPersona(name="动能量化官", weight=0.35, focus="量价背离、资金费率与持仓量变化"),
            AiPersona(name="风险审计官", weight=0.30, focus="回撤、爆仓距离与流动性风险"),
        ]
    )
    #: 决策协议: 1=中文详细 JSON（兼容旧模型）, 2=英文紧凑 JSON + 后台转中文（省 ~50% 输入 / ~70% 输出）
    decision_schema_version: Literal[1, 2] = 2


class DataSettings(BaseModel):
    """交易所接入.

    模拟盘与实盘各自一套 API 凭证与域名:
    - demo → 币安模拟盘 (testnet.binancefuture.com)
    - live → 币安实盘 (fapi.binance.com 及备用域名)

    行情数据只从所选环境的币安公共接口获取, 没有任何本地演示行情.
    """

    live_base_url: str = "https://fapi.binance.com"
    demo_base_url: str = "https://testnet.binancefuture.com"
    #: 模拟盘备用线路: 可以填反向代理/多出口网关, 主线路被封时自动切换
    demo_backup_urls: list[str] = Field(default_factory=list)
    backup_base_urls: list[str] = Field(
        default_factory=lambda: [
            "https://fapi1.binance.com",
            "https://fapi2.binance.com",
            "https://fapi3.binance.com",
        ]
    )
    proxy: str = ""
    kline_limit: int = Field(default=200, ge=50, le=1500)
    ticker_ttl_sec: int = Field(default=5, ge=1, le=120)
    kline_ttl_sec: int = Field(default=20, ge=2, le=600)
    request_timeout_sec: int = Field(default=12, ge=3, le=60)
    max_pool_size: int = Field(default=20, ge=1, le=100, description="标的池容量上限（astra 规则）")
    demo_api_key_enc: str = ""
    demo_api_secret_enc: str = ""
    live_api_key_enc: str = ""
    live_api_secret_enc: str = ""


class NewsSettings(BaseModel):
    enabled: bool = True
    refresh_minutes: int = Field(default=10, ge=1, le=1440)
    sources: list[str] = Field(
        default_factory=lambda: [
            # 可直达的英文 RSS (无代理也能出内容)
            "cointelegraph",
            "theblock",
            "decrypt",
            "bitcoinmagazine",
            "newsbtc",
            "beincrypto",
            "utoday",
            # 需要能访问对应域名的源 (墙内需在下方填代理)
            "binance_square",
            "binance_announcement",
            "coindesk",
        ]
    )
    keep_days: int = Field(default=7, ge=1, le=365)
    ai_summary: bool = True
    sentiment_threshold: float = Field(default=0.35, ge=0, le=1)
    max_items_per_source: int = Field(default=30, ge=5, le=200)
    proxy: str = Field(default="", description="资讯专用代理, 留空复用数据源代理")
    request_timeout_sec: int = Field(default=8, ge=3, le=60)
    rsshub_base: str = Field(default="", description="自建 RSSHub 地址(可选), 用于代理抓取币安公告")


class EvolutionSettings(BaseModel):
    enabled: bool = True
    review_on_close: bool = True
    max_injected: int = Field(default=8, ge=0, le=30)
    ttl_days: int = Field(default=7, ge=1, le=90)
    min_confidence: float = Field(default=0.5, ge=0, le=1)
    global_review_every_trades: int = Field(default=10, ge=2, le=500)
    guard_strict: bool = True
    auto_disable_negative: bool = True
    max_rule_chars: int = Field(default=420, ge=80, le=2000)
    #: 入库硬门槛：支持该结论的独立平仓样本数，低于此值一律拒收（单笔偶发事件不写长期心法）
    min_lesson_sample: int = Field(default=2, ge=1, le=50)
    #: 批量复盘：攒够 N 笔未复盘成交才调模型，避免逐笔编故事
    batch_review_size: int = Field(default=10, ge=2, le=100)
    #: 健康分重算所需样本；不足返回「未知」，不改分、不归档
    min_health_sample: int = Field(default=10, ge=2, le=200)
    #: 健康分低于此值且样本达标才归档（0-1）
    archive_health_threshold: float = Field(default=0.45, ge=0.05, le=0.9)
    #: 生效心法上限，防止只增不减
    max_active_mantras: int = Field(default=40, ge=5, le=300)


class BackupSettings(BaseModel):
    retention: int = Field(default=10, ge=1, le=200)
    encrypt: bool = True
    auto_daily: bool = True
    include_logs: bool = False
    last_run_ts: int = 0


class SecuritySettings(BaseModel):
    username: str = "admin"
    password_hash: str = ""
    session_ttl_hours: int = Field(default=12, ge=1, le=720)
    require_login: bool = True
    login_fail_limit: int = Field(default=8, ge=3, le=100)


class SystemSettings(BaseModel):
    log_level: Literal["DEBUG", "INFO", "WARNING", "ERROR"] = "INFO"
    heartbeat_sec: int = Field(default=5, ge=1, le=60)
    event_retention_days: int = Field(default=14, ge=1, le=365)
    #: 信号审计保留天数: 过期先归档到 data/archive/ 再删除
    signal_retention_days: int = Field(default=14, ge=1, le=365)
    #: 权益曲线保留天数
    equity_retention_days: int = Field(default=30, ge=1, le=730)
    timezone_offset: int = Field(default=8, ge=-12, le=14)
    ui_refresh_sec: int = Field(default=5, ge=1, le=60)
    risk_disclaimer_accepted: bool = False
    #: 告警推送: 事件总线里达到级别的消息推送到 Webhook / Telegram
    alert_enabled: bool = False
    alert_min_level: Literal["info", "warn", "error"] = "warn"
    alert_webhook_url: str = Field(default="", description="钉钉/飞书/企业微信 文本机器人地址")
    #: error 级专用通道 (值班群); 留空则回落到 alert_webhook_url
    alert_error_webhook_url: str = ""
    alert_telegram_token: str = ""
    alert_telegram_chat_id: str = ""
    alert_cooldown_sec: int = Field(default=120, ge=10, le=3600, description="同一类别告警的最小间隔")
    #: 静默时段 (如 "23:00-07:00"): 期间只放行 error; 留空=不静默
    alert_quiet_hours: str = ""
    #: 消息模板, 支持 {level} {category} {message} {time}; 留空=默认格式
    alert_template: str = ""
    #: 摘要模式: 开启后 warn 不进即时通道, 攒到间隔后合并成一条 (error 仍然即时)
    alert_digest_enabled: bool = False
    alert_digest_minutes: int = Field(default=60, ge=5, le=1440)
    #: 执行内核 sidecar (Rust): off=只用 Python 签名; shadow=旁路比对不影响下单;
    #: enforce=下单前同步比对, 不一致拒绝下单. sidecar 不可用时仍降级回 Python 签名.
    exec_mode: Literal["off", "shadow", "enforce"] = "off"
    exec_socket: str = Field(default="", description="留空读 EXEC_SOCKET 环境变量; unix:/path 或 tcp:host:port")
    exec_sample_pct: int = Field(default=100, ge=1, le=100, description="影子比对抽样比例%")
    exec_timeout_ms: int = Field(default=500, ge=50, le=5000, description="单次比对超时")
    api_rate_limit_per_min: int = Field(
        default=0, ge=0, le=10000, description="每 IP 每分钟 API 请求上限, 0=关闭（防爆破/刷接口）"
    )
    metrics_enabled: bool = Field(default=True, description="是否开放只读 /metrics（Prometheus 文本）")


class SignalFollowSettings(BaseModel):
    """外部频道信号跟单（对接 ao-monitor 的 signals.jsonl / HTTP 推送）.

    即时性：HTTP 直推是零延迟；文件通道只做**本地增量读**，不是 Telegram 轮询，
    因此不受 RUNBOOK M1 的 10s 下限约束。
    """

    enabled: bool = Field(default=False, description="开启后才会读取或接收外部信号")
    jsonl_path: str = Field(default="", description="signals.jsonl 绝对路径（或频道目录）; 留空自动找 ao-monitor/data/*/signals.jsonl")
    poll_sec: int = Field(default=1, ge=1, le=60, description="本地 jsonl 增量轮询间隔秒（非 Telegram 轮询）")
    ingest_token: str = Field(default="", description="HTTP 直推共享密钥; 留空则直推接口只接受登录态")
    leverage_bonus: int = Field(default=10, ge=0, le=100, description="在原信号杠杆基础上增加的倍数（默认 +10x）")
    max_age_sec: int = Field(default=180, ge=5, le=3600, description="信号超过该秒数不再跟单（防重启补开旧信号）")
    max_per_hour: int = Field(default=6, ge=1, le=100, description="每小时最多跟单笔数")
    auto_add_symbol: bool = Field(default=True, description="标的池没有该标的时自动添加")
    strict_events: bool = Field(default=False, description="开启后未知事件一律拒绝; 关闭时只告警并按方向跟单")
    default_sl_pct: float = Field(default=1.2, gt=0, le=20, description="信号未给止损时使用的止损百分比")
    dry_run: bool = Field(default=True, description="只记录意图不下单; 人工明确授权后才可关闭（RUNBOOK M4）")


class PublicSettings(BaseModel):
    """公开前台 (无需登录) 的展示配置."""

    site_enabled: bool = True
    site_title: str = "NanoQuant"
    site_tagline: str = "小资金 · 币安 U 本位永续 · 超短线量化终端"
    hero_note: str = ""
    show_engine_status: bool = Field(default=True, description="前台展示引擎运行状态(不含资金)")
    show_mantras: bool = Field(default=True, description="前台展示脱敏后的分析心法")
    show_performance: bool = Field(default=False, description="前台展示胜率/收益等绩效数据")


GROUPS: dict[str, type[BaseModel]] = {
    "engine": EngineSettings,
    "risk": RiskSettings,
    "strategy": StrategySettings,
    "ai": AiSettings,
    "data": DataSettings,
    "news": NewsSettings,
    "evolution": EvolutionSettings,
    "backup": BackupSettings,
    "security": SecuritySettings,
    "system": SystemSettings,
    "public": PublicSettings,
    "signal_follow": SignalFollowSettings,
}

_CACHE: dict[str, tuple[float, dict[str, Any]]] = {}
_LOCK = threading.RLock()
_TTL = 1.5


def _deep_merge(base: dict[str, Any], patch: dict[str, Any]) -> dict[str, Any]:
    out = dict(base)
    for k, v in patch.items():
        if isinstance(v, dict) and isinstance(out.get(k), dict):
            out[k] = _deep_merge(out[k], v)
        else:
            out[k] = v
    return out


def default_group(name: str) -> dict[str, Any]:
    model = GROUPS[name]
    return model().model_dump()


def _migrate_legacy_group(name: str, data: dict[str, Any]) -> dict[str, Any]:
    """旧配置就地迁移, 在合并默认值之前完成.

    - engine: paper → demo; 移除本地模拟盘余额与静态标的数组
    - data  : 单一密钥 → 实盘密钥; 移除 auto/synthetic/testnet 等演示行情开关
    """
    out = dict(data)
    if name == "engine":
        if out.get("mode") == "paper":
            out["mode"] = MODE_DEMO
        # 老库可能存了 1~14s 的轮询间隔: 先钳到下限, 否则整组配置会校验失败并回退全部默认值
        try:
            interval = int(out.get("cycle_interval_sec", MIN_CYCLE_INTERVAL_SEC))
        except (TypeError, ValueError):
            interval = MIN_CYCLE_INTERVAL_SEC
        out["cycle_interval_sec"] = max(MIN_CYCLE_INTERVAL_SEC, min(120, interval))
        if "paper_start_balance" in out:
            out.setdefault("demo_start_balance", out.pop("paper_start_balance"))
        out.pop("demo_start_balance", None)
        out.pop("symbols", None)
    elif name == "data":
        if "api_key_enc" in out:
            out.setdefault("live_api_key_enc", out.pop("api_key_enc"))
        if "api_secret_enc" in out:
            out.setdefault("live_api_secret_enc", out.pop("api_secret_enc"))
        if "base_url" in out:
            out.setdefault("live_base_url", out.pop("base_url"))
        for legacy in ("mode", "allow_synthetic_fallback", "synthetic_seed", "testnet"):
            out.pop(legacy, None)
    elif name == "strategy":
        # 由 1 分钟级升级为 5–30 分钟短线: 旧主周期 1m/3m 统一迁到 5m
        if out.get("timeframe") in {"1m", "3m"}:
            out["timeframe"] = "5m"
    return out


def _load_raw(name: str) -> dict[str, Any]:
    with session_scope() as s:
        row = s.get(Setting, name)
        if not row:
            return {}
        try:
            data = json.loads(row.value or "{}")
        except json.JSONDecodeError:
            log.warning("配置 %s JSON 损坏, 回退默认值", name)
            return {}
        if not isinstance(data, dict):
            return {}
        migrated = _migrate_legacy_group(name, data)
        # 一次性写回被钳制的轮询间隔，避免每次读都做迁移、也避免面板显示旧值
        if name == "engine" and data.get("cycle_interval_sec") != migrated.get("cycle_interval_sec"):
            row.value = json.dumps(
                {**data, "cycle_interval_sec": migrated["cycle_interval_sec"]}, ensure_ascii=False
            )
            s.flush()
            log.info(
                "引擎轮询间隔 %ss → %ss (下限 %ss)",
                data.get("cycle_interval_sec"), migrated["cycle_interval_sec"], MIN_CYCLE_INTERVAL_SEC,
            )
        return migrated


def get_group(name: str, use_cache: bool = True) -> dict[str, Any]:
    if name not in GROUPS:
        raise KeyError(name)
    now = time.time()
    if use_cache:
        hit = _CACHE.get(name)
        if hit and now - hit[0] < _TTL:
            return hit[1]
    merged = _deep_merge(default_group(name), _load_raw(name))
    try:
        validated = GROUPS[name](**merged).model_dump()
    except ValidationError as exc:
        log.error("配置 %s 校验失败, 使用默认值: %s", name, exc)
        validated = default_group(name)
    _CACHE[name] = (now, validated)
    return validated


def get_all() -> dict[str, Any]:
    return {name: get_group(name) for name in GROUPS}


def update_group(name: str, patch: dict[str, Any]) -> dict[str, Any]:
    if name not in GROUPS:
        raise KeyError(name)
    if not isinstance(patch, dict):
        raise SettingsValidationError(name, "patch 必须是对象")
    with _LOCK:
        current = _deep_merge(default_group(name), _load_raw(name))
        candidate = _deep_merge(current, patch)
        try:
            validated = GROUPS[name](**candidate)
        except ValidationError as exc:
            first = exc.errors()[0]
            field = ".".join(str(p) for p in first.get("loc", ())) or "?"
            raise SettingsValidationError(name, f"{field}: {first.get('msg')}") from exc
        payload = validated.model_dump()
        with session_scope() as s:
            row = s.get(Setting, name)
            if row:
                row.value = json.dumps(payload, ensure_ascii=False)
            else:
                s.add(Setting(key=name, value=json.dumps(payload, ensure_ascii=False)))
        _CACHE[name] = (time.time(), payload)
        return payload


def reset_group(name: str) -> dict[str, Any]:
    if name not in GROUPS:
        raise KeyError(name)
    with _LOCK:
        with session_scope() as s:
            row = s.get(Setting, name)
            if row:
                s.delete(row)
        _CACHE.pop(name, None)
    return get_group(name, use_cache=False)


def invalidate(name: str | None = None) -> None:
    with _LOCK:
        if name:
            _CACHE.pop(name, None)
        else:
            _CACHE.clear()


def get_public() -> PublicSettings:
    return PublicSettings(**get_group("public"))


def get_engine() -> EngineSettings:
    return EngineSettings(**get_group("engine"))


def get_risk() -> RiskSettings:
    return RiskSettings(**get_group("risk"))


def get_strategy() -> StrategySettings:
    return StrategySettings(**get_group("strategy"))


def get_ai() -> AiSettings:
    return AiSettings(**get_group("ai"))


def get_data() -> DataSettings:
    return DataSettings(**get_group("data"))


def get_news() -> NewsSettings:
    return NewsSettings(**get_group("news"))


def get_evolution() -> EvolutionSettings:
    return EvolutionSettings(**get_group("evolution"))


def get_backup() -> BackupSettings:
    return BackupSettings(**get_group("backup"))


def get_security() -> SecuritySettings:
    return SecuritySettings(**get_group("security"))


def get_system() -> SystemSettings:
    return SystemSettings(**get_group("system"))


def get_signal_follow() -> SignalFollowSettings:
    return SignalFollowSettings(**get_group("signal_follow"))
