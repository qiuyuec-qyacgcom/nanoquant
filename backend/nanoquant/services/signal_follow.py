"""外部频道信号：解析 / 事件分类 / 幂等键 / 增量读取 / 路径发现.

对接 RUNBOOK 描述的 `ao-monitor`:
- `poller.py -c <频道>` 把识别到的消息写进 `data/<slug>/signals.jsonl`;
- 本模块只消费**新增完整行**, 不抓 Telegram、不翻历史 (RUNBOOK M2 / 非目标: 历史回填);
- 频道不写死: 路径由配置 / 环境变量给出, 留空时自动发现最新的 signals.jsonl (RUNBOOK S0 / M7);
- 只把「开仓」类事件交给引擎跟单; 平仓/止损/更新一律忽略, 未知事件只报告不猜规则
  (RUNBOOK S6-06)。

字段做容错映射（不同频道模板差异大）:
- symbol: symbol / pair / coin / ticker / 标的
- side:   side / direction / action（long|buy|多|开多 / short|sell|空|开空）
- 杠杆:   leverage / lev / 杠杆，支持 "20x" / "20倍"
- 价格:   entry / price / entry_price；止损 stop / sl / stop_loss；止盈 tp / take_profit
- 时间:   ts / time / created_at / timestamp（秒、毫秒或 ISO8601）
- 事件:   event / event_type / type / kind / action / 事件 / 动作 / 类型 / status
- 唯一键: id / signal_id / msg_id，缺省时用 symbol+side+entry+ts 哈希
"""
from __future__ import annotations

import hashlib
import json
import os
import re
import time
from datetime import datetime
from typing import Any

from .symbols import normalize_symbol

_SIDE_MAP = {
    "long": "long", "buy": "long", "多": "long", "做多": "long", "开多": "long", "看多": "long",
    "short": "short", "sell": "short", "空": "short", "做空": "short", "开空": "short", "看空": "short",
}

#: 事件字段里自带的方向（"平多"/"平空"/"close_long"），当 side 字段缺失时用
_SIDE_FROM_EVENT = (
    ("平多", "long"), ("平空", "short"),
    ("多单", "long"), ("空单", "short"),
    ("closelong", "long"), ("closeshort", "short"),
    ("close_long", "long"), ("close_short", "short"),
    ("reduce_long", "long"), ("reduce_short", "short"),
)

#: 只按「明确的事件字段」分类, 不扫 headline/正文, 避免"开仓...止盈 3%"被误判成平仓
_EVENT_KEYS = (
    "event", "event_type", "type", "kind", "action", "stage", "op",
    "事件", "动作", "类型", "status",
)
_EXIT_WORDS = (
    "平仓", "平多", "平空", "清仓", "减仓", "离场", "出场", "止损", "止盈", "推保本",
    "移动止损", "撤单", "取消", "更新", "修改", "调整", "减半", "分批",
    "close", "exit", "stoploss", "stop_loss", "take_profit", "takeprofit", "cancel",
    "update", "modify", "breakeven", "trailing", "partial",
)
_OPEN_WORDS = (
    "开仓", "建仓", "入场", "进仓", "开多", "开空", "做多", "做空", "买入", "卖出", "开单",
    "open", "entry", "long", "short", "buy", "sell",
)

#: 生命周期事件：与「exit（旧口径，一律忽略）」分开，避免"止盈 50%"被当成全平。
#: 顺序即优先级：reduce 必须排在 close 之前，因为"平仓50%"同时含"平仓"，
#: 先判 close 会把"平一半"执行成"全平"。
_EVENT_MAP: tuple[tuple[str, tuple[str, ...]], ...] = (
    (
        "reduce",
        (
            "reduce", "partial", "trim", "减仓", "减持", "减半", "部分止盈",
            "部分平仓", "分批止盈", "平仓一半", "平一半", "止盈出局",
        ),
    ),
    (
        "close",
        (
            "closeall", "close_all", "close-all", "close", "全平", "全部平仓",
            "清仓", "全部出局", "全部止盈", "全部离场", "平掉", "已平仓",
            "平多", "平空", "平多头", "平空头", "平仓",
        ),
    ),
    (
        "add",
        (
            "add", "加仓", "补仓", "加码", "追加仓位", "二次开仓", "二次进场",
            "average", "scale_in", "scalein",
        ),
    ),
    (
        "move_stop",
        (
            "move_stop", "movestop", "move_stoploss", "breakeven", "trailing",
            "trail", "移止损", "移动止损", "保本", "推保本", "止损上移",
            "止损移至", "止损调整",
        ),
    ),
)

#: 显式事件字段的精确取值（网关会直接送这些值）
_EVENT_EXACT = {
    "open": "open",
    "entry": "open",
    "add": "add",
    "reduce": "reduce",
    "close": "close",
    "move_stop": "move_stop",
    "move": "move_stop",
    "hold": "exit",  # 持仓提醒：明确"不动作"，落到退出分支即跳过
}

#: 明确表示"整仓离场"的说法，优先级高于比例判断
_CLOSE_ALL_WORDS = ("全部平仓", "全平", "清仓", "全部出局", "全部止盈", "全部离场", "平掉", "closeall")
#: 带这些量词说明是在说比例，配合平仓动词即可判为减仓
_PCT_HINT = re.compile(r"\d{1,3}\s*%|一半|半仓|三分之一|三分之二|四分之一|三成|两成|七成|八成")

_NUM_RE = re.compile(r"-?\d+(?:\.\d+)?")
_ENV_PATH = "SIGNAL_FOLLOW_JSONL"
_ENV_ROOT = "AO_MONITOR_DATA"


def _first(row: dict[str, Any], *keys: str) -> Any:
    for key in keys:
        value = row.get(key)
        if value not in (None, "", 0, "0"):
            return value
    return None


def _float(value: Any) -> float:
    """容忍 "20x" / "1,234.5" / 带单位文本; 完全没数字返回 0."""
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return float(value)
    text = str(value or "").replace(",", "").strip()
    match = _NUM_RE.search(text)
    if not match:
        return 0.0
    try:
        return float(match.group())
    except ValueError:
        return 0.0


def normalize_side(value: Any) -> str:
    text = str(value or "").strip().lower().replace(" ", "")
    return _SIDE_MAP.get(text, "")


def _timestamp(value: Any) -> int:
    if isinstance(value, str) and ("T" in value or ("-" in value and ":" in value)):
        try:
            return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp())
        except ValueError:
            pass
    ts = int(_float(value))
    if ts > 10_000_000_000:  # 毫秒
        ts //= 1000
    return ts


def classify_event(row: dict[str, Any]) -> str:
    """返回 open / add / reduce / close / move_stop / exit / unknown / ""。

    - ""=没有事件字段, 按模板变体放行（老行为）
    - exit=旧口径的"其它离场类"，一律跳过不执行（保本兼容）
    - unknown 一律 fail-closed: 只报告「需扩充规则」, 不擅自当成开仓
      (RUNBOOK S6-06 / M6)

    之所以把加仓/减仓/平仓/移止损单列出来：它们各自对应一个真实动作，
    混在 exit 里会导致"频道说全部平仓，系统一直拿着"。
    """
    if not isinstance(row, dict):
        return ""
    raw = _first(row, *_EVENT_KEYS)
    if raw is None:
        return ""
    text = str(raw).strip().lower().replace(" ", "").replace("-", "")
    if not text:
        return ""

    exact = _EVENT_EXACT.get(text)
    if exact:
        return exact

    # "平仓 50%" 与 "全部平仓" 只差一个比例词，但一个是减半一个是清仓。
    # 先认全平的明确说法，再按"带比例=减仓"兜住前者。
    if any(word in text for word in _CLOSE_ALL_WORDS):
        return "close"
    if _PCT_HINT.search(text) and any(
        word in text for word in ("平仓", "减", "清", "close", "reduce", "partial", "trim")
    ):
        return "reduce"

    for event, words in _EVENT_MAP:
        if any(word.replace("-", "") in text for word in words):
            return event

    # 旧口径兜底：止损/止盈/更新/取消等仍然只报告、不执行
    if any(word in text for word in _EXIT_WORDS):
        return "exit"
    if any(word in text for word in _OPEN_WORDS):
        return "open"
    return "unknown"


def follow_leverage(signal_leverage: float, default_leverage: float, bonus: int = 10) -> int:
    """目标杠杆 = 原信号杠杆 + 加成; 信号没给杠杆时回落到风控「杠杆下限」.

    最终仍由 `risk.choose_leverage` 钳到风控全局杠杆下限/上限与交易所上限，
    这里只负责给出「在原信号基础上 +N」的原始意图, 保证可单测。
    """
    lev = float(signal_leverage or 0.0)
    if lev <= 0:
        return max(1, int(round(float(default_leverage or 1))))
    return max(1, int(round(lev + max(0, int(bonus)))))


def parse_signal(row: dict[str, Any], *, now: int | None = None) -> dict[str, Any] | None:
    """把 jsonl / HTTP 里的一条记录转成统一结构；缺标的返回 None。

    方向只在「开仓 / 加仓」时必需：平仓、减仓、移止损这类消息频道经常
    只写币种（"BTC 全部平仓"），方向由下游按现有持仓补齐。
    """
    if not isinstance(row, dict):
        return None
    symbol = normalize_symbol(str(_first(row, "symbol", "pair", "coin", "ticker", "标的") or ""))
    if not symbol:
        return None
    event = classify_event(row)
    side = normalize_side(_first(row, "side", "direction", "action", "方向"))
    if not side:
        # "action":"平多" 这类写法方向藏在事件词里
        blob = "".join(
            str(row.get(k, "")) for k in ("event", "event_type", "action", "类型", "动作")
        ).lower().replace(" ", "")
        for word, mapped in _SIDE_FROM_EVENT:
            if word in blob:
                side = mapped
                break
    if not side and event not in ("close", "reduce", "move_stop", "exit"):
        return None
    ts = _timestamp(_first(row, "ts", "time", "created_at", "timestamp", "时间")) or int(now or time.time())
    signal_id = str(_first(row, "id", "signal_id", "msg_id", "message_id") or "").strip()
    return {
        "symbol": symbol,
        "side": side,
        "leverage": max(0.0, min(_float(_first(row, "leverage", "lev", "杠杆")), 125.0)),
        "entry": _float(_first(row, "entry", "price", "entry_price", "入场价")),
        "stop": _float(_first(row, "sl", "stop", "stop_loss", "止损")),
        "tp": _float(_first(row, "tp", "take_profit", "止盈")),
        "reduce_pct": _float(_first(row, "reduce_pct", "reduce", "close_pct", "减仓比例")),
        "stop_to_breakeven": bool(
            _first(row, "stop_to_breakeven", "breakeven", "move_to_breakeven")
        ),
        "ts": ts,
        "signal_id": signal_id,
        "event": event,
        "event_missing": event == "",
        "raw": row,
    }


def signal_identity(parsed: dict[str, Any]) -> str:
    """幂等键：频道给 id 就用 id；否则按 标的/方向/入场价/时间 哈希."""
    sid = str(parsed.get("signal_id") or "").strip()
    if sid:
        return f"{sid}:{parsed['symbol']}:{parsed['side']}"[:64]
    seed = f"{parsed['symbol']}|{parsed['side']}|{parsed.get('entry') or 0}|{parsed.get('ts') or 0}"
    return "h:" + hashlib.sha1(seed.encode("utf-8")).hexdigest()[:24]


def _with_signals_jsonl(path: str) -> str:
    """配置既可以是文件, 也可以是 ao-monitor/data 目录或某个频道目录."""
    if os.path.isdir(path):
        return os.path.join(path, "signals.jsonl")
    if os.path.splitext(path)[1] or os.path.isfile(path):
        return path
    return os.path.join(path, "signals.jsonl")


def _scan_root(root: str) -> tuple[str, float]:
    best, best_mtime = "", 0.0
    if not os.path.isdir(root):
        return best, best_mtime
    direct = os.path.join(root, "signals.jsonl")
    if os.path.isfile(direct):
        best, best_mtime = direct, os.path.getmtime(direct)
    try:
        names = sorted(os.listdir(root))[:500]
    except OSError:
        names = []
    for name in names:
        candidate = os.path.join(root, name, "signals.jsonl")
        try:
            mtime = os.path.getmtime(candidate)
        except OSError:
            continue
        if mtime > best_mtime:
            best, best_mtime = candidate, mtime
    return best, best_mtime


def resolve_path(configured: str, home: str) -> str:
    """定位 signals.jsonl: 配置 > SIGNAL_FOLLOW_JSONL > AO_MONITOR_DATA > 自动发现.

    自动发现顺序覆盖 RUNBOOK 的 `ao-monitor/data/<slug>/signals.jsonl` 布局,
    取 mtime 最新的一份（不同频道独立目录, 互不覆盖; 频道名仍来自上游, 这里不猜 slug）。
    """
    raw = (configured or "").strip() or os.getenv(_ENV_PATH, "").strip()
    if raw:
        return _with_signals_jsonl(os.path.abspath(os.path.expanduser(raw)))
    home_abs = os.path.abspath(home)
    parent = os.path.dirname(home_abs)
    roots = (
        os.getenv(_ENV_ROOT, "").strip(),
        os.path.join(parent, "ao-monitor", "data"),
        os.path.join(home_abs, "ao-monitor", "data"),
        os.path.join(parent, "ao-monitor"),
        os.path.join(home_abs, "data", "ao-monitor"),
    )
    best, best_mtime = "", 0.0
    for root in roots:
        if not root:
            continue
        candidate, mtime = _scan_root(os.path.abspath(os.path.expanduser(root)))
        if mtime > best_mtime:
            best, best_mtime = candidate, mtime
    return best


def read_new_lines(path: str, offset: int, *, max_bytes: int = 1 << 20) -> tuple[list[dict[str, Any]], int]:
    """从 offset 读取新增的完整行；末尾未写完的半行不消费（下次再读）.

    文件截断时自动从 0 重读; 单次最多读 1MB, 避免上游异常暴涨时把整轮卡住。
    """
    if not path or not os.path.isfile(path):
        return [], offset
    size = os.path.getsize(path)
    if size < offset:
        offset = 0
    if size == offset:
        return [], offset
    start = offset
    if size - start > max_bytes:
        start = size - max_bytes
        with open(path, "rb") as fh:
            fh.seek(start)
            start += len(fh.readline())  # 丢掉被截断的半行
    with open(path, "rb") as fh:
        fh.seek(start)
        chunk = fh.read(size - start)
    consumed = start - offset + len(chunk)
    if not chunk.endswith(b"\n"):
        cut = chunk.rfind(b"\n") + 1
        if cut <= 0:
            return [], offset  # 一行都没写完
        chunk = chunk[:cut]
        consumed = start - offset + cut
    rows: list[dict[str, Any]] = []
    for line in chunk.decode("utf-8", "replace").splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            item = json.loads(line)
        except ValueError:
            continue
        if isinstance(item, dict):
            rows.append(item)
    return rows, offset + consumed


# -------------------------------------------------------------------- 频道链接(http/https)
_URL_RE = re.compile(r"^https?://", re.IGNORECASE)


def is_url(source: str) -> bool:
    """配置项既可以是本地文件路径，也可以是 http(s) 频道链接."""
    return bool(_URL_RE.match((source or "").strip()))


def _coerce_rows(payload: Any) -> list[dict[str, Any]]:
    """从 URL 响应体提取信号行：JSON 数组 / 含列表字段的对象 / 纯 JSONL 文本.

    返回 dict 列表；空或非预期结构返回空列表（调用方据此跳过本轮）。
    """
    if isinstance(payload, list):
        return [r for r in payload if isinstance(r, dict)]
    if isinstance(payload, dict):
        for key in ("signals", "items", "data", "messages", "rows", "result"):
            value = payload.get(key)
            if isinstance(value, list):
                return [r for r in value if isinstance(r, dict)]
        return []
    if isinstance(payload, str):
        out: list[dict[str, Any]] = []
        for line in payload.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                item = json.loads(line)
            except ValueError:
                continue
            if isinstance(item, dict):
                out.append(item)
        return out
    return []


async def fetch_url_signals(url: str, *, timeout: float = 10.0) -> list[dict[str, Any]]:
    """拉取频道链接并把响应体解析成统一信号行列表.

    兼容三种上游格式：JSON 数组、含 ``signals/items/data`` 等键的对象、纯 JSONL 文本。
    失败直接抛异常，由轮询方记录后跳过本轮（不阻塞）。
    """
    import httpx

    async with httpx.AsyncClient(timeout=timeout, follow_redirects=True) as client:
        resp = await client.get(url, headers={"Accept": "application/json, text/plain"})
        resp.raise_for_status()
        try:
            payload = resp.json()
        except ValueError:
            payload = resp.text
        return _coerce_rows(payload)
