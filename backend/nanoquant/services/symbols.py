"""标的池: 规则完整迁移自本地项目 astra-quant-agent 的 instrument_pool.

与 astra 的对应关系:
- 添加前必须用**交易所在线合约表**校验 (instId 精确匹配 / USDT 结算 / 状态 live),
  不接受任何本地演示清单或白名单;
- 去重与容量上限在写入前判定 (重复 409 / 超限 409);
- 规格随合约表落库: precision(stepSize 小数位)、tick_sz、min_sz、min_notional、ct_val;
- 分层: BTC/ETH 为 tier_1_bluechip (sl_atr_mult 1.8), 其余 tier_2_momentum (2.2),
  单标的杠杆上限跟随全局风控上限;
- 删除必须提供确认短语 ``REMOVE <SYMBOL>``; 有持仓或无法确认持仓时拒绝删除;
- 允许空池 (取消 astra 的 BTC 哨兵与最小 1 个限制).

与 astra 的差异仅在存储: 这里用 SQLite 表而不是 JSON 文件, 因此 RMW 由事务保证.
"""
from __future__ import annotations

import re
from typing import Any

from ..db import session_scope
from ..exchange.client import BinanceError, NoCredentials
from ..exchange.client import client as binance_client
from ..exchange.precision import SymbolFilters
from ..logging_setup import get_logger
from ..models import PositionRecord, TradingSymbol
from .settings import get_data, get_engine, get_risk, normalize_mode

log = get_logger("symbols")

#: 币安 U 本位永续合约名: 大写字母数字 + USDT 计价 (如 BTCUSDT / 1000SHIBUSDT)
SYMBOL_RE = re.compile(r"^[A-Z0-9]{2,18}(?:USDT)$")

TIER_PROFILES: dict[str, dict[str, Any]] = {
    "tier_1_bluechip": {"label": "蓝筹主流", "sl_atr_mult": 1.8, "min_vol_24h_usd": 100_000_000},
    "tier_2_momentum": {"label": "高弹性动量", "sl_atr_mult": 2.2, "min_vol_24h_usd": 20_000_000},
}
TIER1_ASSETS = ("BTC", "ETH")


class PoolError(Exception):
    """标的池业务错误: status 直接映射成 HTTP 状态码."""

    def __init__(self, status: int, detail: str) -> None:
        super().__init__(detail)
        self.status = status
        self.detail = detail


def normalize_symbol(raw: str) -> str:
    return "".join(ch for ch in (raw or "").upper() if ch.isascii() and ch.isalnum())


def is_plausible_symbol(symbol: str) -> bool:
    if not symbol or not SYMBOL_RE.match(symbol):
        return False
    base = symbol[:-4]
    return len(base) >= 2 and any(ch.isalpha() for ch in base)


def evaluate_tier(base_asset: str) -> str:
    return "tier_1_bluechip" if (base_asset or "").upper() in TIER1_ASSETS else "tier_2_momentum"


def derive_leverage_cap(tier: str) -> int:
    """单标的杠杆上限: 跟随全局风控上限 (astra 2026-10 起的统一规则)."""
    risk = get_risk()
    return max(1, min(int(risk.max_leverage), 100))


def _precision(step: str) -> int:
    normalized = str(step).rstrip("0")
    return len(normalized.split(".", 1)[1]) if "." in normalized else 0


def _filter_value(raw: dict[str, Any], filter_type: str, key: str, default: str = "") -> str:
    for item in raw.get("filters") or []:
        if isinstance(item, dict) and item.get("filterType") == filter_type:
            return str(item.get(key) or default)
    return default


def _validate_contract(raw: dict[str, Any], symbol: str) -> None:
    """与 astra 一致的三段校验: 精确匹配 / USDT 结算 / 在线可交易."""
    if str(raw.get("symbol") or "") != symbol:
        raise PoolError(400, "仅允许添加币安 USDⓈ-M 在线可交易的 USDT 永续合约")
    if str(raw.get("contractType") or "") != "PERPETUAL":
        raise PoolError(400, "仅允许添加币安 U 本位永续合约（不支持交割/杠杆代币）")
    if str(raw.get("status") or "") != "TRADING":
        raise PoolError(400, "该合约当前不可交易（状态不是 TRADING）")
    if str(raw.get("quoteAsset") or "") != "USDT":
        raise PoolError(400, "仅允许添加 USDT 计价的永续合约")


def build_pool_row(raw: dict[str, Any]) -> dict[str, Any]:
    """合约表行 → 池条目 (对应 astra 的 from_binance_instrument)."""
    symbol = normalize_symbol(str(raw.get("symbol") or ""))
    base = str(raw.get("baseAsset") or symbol[:-4]).upper()
    tier = evaluate_tier(base)
    profile = TIER_PROFILES[tier]
    step = _filter_value(raw, "LOT_SIZE", "stepSize", "0.001")
    min_qty = _filter_value(raw, "LOT_SIZE", "minQty", step)
    tick = _filter_value(raw, "PRICE_FILTER", "tickSize", "0.0001")
    min_notional = _filter_value(raw, "MIN_NOTIONAL", "notional", "5") or "5"
    return {
        "symbol": symbol,
        "name": base,
        "tier": tier,
        "sl_atr_mult": float(profile["sl_atr_mult"]),
        "max_leverage": derive_leverage_cap(tier),
        "precision": _precision(step),
        "tick_sz": tick,
        "min_sz": min_qty,
        "min_notional": float(min_notional),
        "ct_val": 1.0,
    }


async def _contract(symbol: str) -> dict[str, Any]:
    try:
        rows = await binance_client.exchange_symbols()
    except (BinanceError, NoCredentials) as exc:
        raise PoolError(502, f"无法读取币安合约表，暂时不能添加标的：{exc}") from exc
    raw = rows.get(symbol) or {}
    _validate_contract(raw, symbol)
    return raw


def filters_of(row: TradingSymbol, last_price: float = 0.0) -> SymbolFilters:
    """池条目的交易规格 (引擎下单/风控使用, 不依赖行情模块)."""
    step = 10.0 ** -max(0, int(row.precision or 0))
    return SymbolFilters(
        symbol=row.symbol,
        price_tick=float(row.tick_sz or "0.01"),
        qty_step=step,
        min_qty=float(row.min_sz or step),
        min_notional=float(row.min_notional or 5.0),
        max_leverage=int(row.max_leverage or 20),
        last_price=last_price,
        source="pool",
    )


def filters_map() -> dict[str, SymbolFilters]:
    with session_scope() as s:
        rows = s.query(TradingSymbol).all()
        return {r.symbol: filters_of(r) for r in rows}


def filters_for(symbol: str) -> SymbolFilters | None:
    with session_scope() as s:
        row = s.get(TradingSymbol, symbol)
        return filters_of(row) if row else None


def known_symbols() -> set[str]:
    with session_scope() as s:
        return {r.symbol for r in s.query(TradingSymbol).all()}


#: 添加顺序的单一事实源: sort_order 优先, 兼容老库的 created_at 与 symbol
POOL_ORDER = (TradingSymbol.sort_order.asc(), TradingSymbol.created_at.asc(), TradingSymbol.symbol.asc())


def pool_rows(enabled_only: bool = False) -> list[dict[str, Any]]:
    """按添加顺序返回池条目 (脱离会话的普通 dict, 可直接给接口/前台用)."""
    with session_scope() as s:
        query = s.query(TradingSymbol)
        if enabled_only:
            query = query.filter(TradingSymbol.enabled.is_(True))
        rows = query.order_by(*POOL_ORDER).all()
        return [
            {
                "symbol": r.symbol,
                "name": r.name or r.symbol[:-4],
                "tier": r.tier,
                "tier_label": TIER_PROFILES.get(r.tier, TIER_PROFILES["tier_2_momentum"])["label"],
                "sl_atr_mult": r.sl_atr_mult,
                "max_leverage": r.max_leverage,
                "enabled": r.enabled,
                "sort_order": int(r.sort_order or 0),
                "created_at": int(r.created_at or 0),
            }
            for r in rows
        ]


def ordered_symbols(enabled_only: bool = True) -> list[str]:
    return [row["symbol"] for row in pool_rows(enabled_only=enabled_only)]


def enabled_symbols() -> list[str]:
    return ordered_symbols(enabled_only=True)


def pool_state() -> dict[str, Any]:
    rows = pool_rows()
    return {
        "status": "ok" if rows else "empty",
        "count": len(rows),
        "detail": "" if rows else "标的池为空, 请在「标的池」页添加币安 USDT 永续合约",
        "symbols": [row["symbol"] for row in rows],
        "limits": {
            "minimum": 0,
            "maximum": get_data().max_pool_size,
            "update_interval_hint": "每次保存即时生效",
        },
    }


async def add_symbol(raw_symbol: str) -> dict[str, Any]:
    symbol = normalize_symbol(raw_symbol)
    if not is_plausible_symbol(symbol):
        raise PoolError(400, "请输入币安 U 本位合约名（如 BTCUSDT / 1000PEPEUSDT）")
    raw = await _contract(symbol)
    row = build_pool_row(raw)
    limit = get_data().max_pool_size
    with session_scope() as s:
        if s.get(TradingSymbol, symbol) is not None:
            raise PoolError(409, "该币种已在交易池中")
        count = s.query(TradingSymbol).count()
        if count >= limit:
            raise PoolError(409, f"交易池最多允许 {limit} 个币种；请先删除一个无持仓币种")
        # 追加到队尾: 前台 K 线、引擎扫描都严格按添加顺序
        last = s.query(TradingSymbol).order_by(TradingSymbol.sort_order.desc()).first()
        next_order = int(getattr(last, "sort_order", 0) or 0) + 1
        s.add(TradingSymbol(enabled=True, min_score=0.0, note="", sort_order=next_order, **row))
    log.info("标的池新增 %s (%s)", symbol, row["tier"])
    return row


async def add_many(raw_symbols: list[str]) -> dict[str, Any]:
    added: list[str] = []
    skipped: list[str] = []
    invalid: list[dict[str, str]] = []
    for raw_symbol in raw_symbols:
        symbol = normalize_symbol(raw_symbol)
        if not symbol:
            continue
        try:
            await add_symbol(symbol)
            added.append(symbol)
        except PoolError as exc:
            if exc.status == 409 and "已在交易池" in exc.detail:
                skipped.append(symbol)
            else:
                invalid.append({"symbol": symbol, "reason": exc.detail})
    return {"added": added, "skipped": skipped, "invalid": invalid}


async def delete_symbol(raw_symbol: str, confirmation: str) -> dict[str, Any]:
    symbol = normalize_symbol(raw_symbol)
    phrase = (confirmation or "").strip().upper()
    if phrase != f"REMOVE {symbol}":
        raise PoolError(400, f"必须提供确认短语（精确为：REMOVE {symbol}）")
    mode = normalize_mode(get_engine().mode)
    with session_scope() as s:
        row = s.get(TradingSymbol, symbol)
        if row is None:
            raise PoolError(404, "该币种不在交易池中")
        held = (
            s.query(PositionRecord)
            .filter(
                PositionRecord.status == "open",
                PositionRecord.symbol == symbol,
                PositionRecord.mode == mode,
            )
            .first()
        )
        if held is not None:
            raise PoolError(409, "该标的仍有持仓，删除后将失去止损/止盈接管，禁止删除")
    # astra 同款: 交易所仍有持仓/无法确认持仓时一律拒绝 (fail-closed).
    # 未配置当前环境凭证时不可能存在交易所持仓, 按本地记录判断即可.
    try:
        positions = await binance_client.positions(symbol)
    except NoCredentials:
        positions = []
    except BinanceError as exc:
        raise PoolError(503, f"无法确认实时持仓（{exc}），为防止删掉有持仓的标的，已拒绝删除") from exc
    for pos in positions:
        if str(pos.get("symbol") or "") != symbol:
            continue
        try:
            amount = abs(float(pos.get("positionAmt", 0) or 0))
        except (TypeError, ValueError):
            amount = 0.0
        if amount > 0:
            raise PoolError(409, "该标的在交易所仍有实时持仓，删除后将失去风控接管，禁止删除")
    with session_scope() as s:
        row = s.get(TradingSymbol, symbol)
        if row is None:
            raise PoolError(404, "该币种不在交易池中")
        s.delete(row)
    log.info("标的池移除 %s", symbol)
    return {"removed": symbol, "count": len(known_symbols())}


async def candidates(keyword: str, limit: int = 50) -> list[dict[str, Any]]:
    """候选合约 (来自币安在线合约表, 不是行情)."""
    key = normalize_symbol(keyword)
    try:
        rows = await binance_client.exchange_symbols()
    except (BinanceError, NoCredentials) as exc:
        raise PoolError(502, f"无法读取币安合约表：{exc}") from exc
    pool = known_symbols()
    out: list[dict[str, Any]] = []
    for symbol in sorted(rows):
        raw = rows[symbol]
        try:
            _validate_contract(raw, symbol)
        except PoolError:
            continue
        if key and not symbol.startswith(key) and key not in symbol:
            continue
        row = build_pool_row(raw)
        out.append(
            {
                "symbol": symbol,
                "name": row["name"],
                "tier": row["tier"],
                "tier_label": TIER_PROFILES[row["tier"]]["label"],
                "max_leverage": row["max_leverage"],
                "tick_sz": row["tick_sz"],
                "min_sz": row["min_sz"],
                "min_notional": row["min_notional"],
                "in_pool": symbol in pool,
            }
        )
        if len(out) >= limit:
            break
    return out
