"""公开前台接口 (无需登录).

只暴露可公开的聚合数据:
- 标的池 (严格按添加顺序) / 资讯 / 情绪
- 前台 K 线: **只覆盖标的池内的合约**, 数据实时取自该交易环境的币安公共接口,
  没有任何本地演示行情; 池为空时接口返回空列表, 前台展示"未添加标的"
- 引擎运行状态 (仅开关与模式, 不含资金)
- 脱敏心法 (仅标题与健康度, 不含规则正文)
- 绩效 (需在配置中显式开启, 且只给比例不给金额)

刻意不暴露: 账户余额、持仓、订单、API 密钥、心法规则全文、AI 配置.
"""
from __future__ import annotations

import time
from typing import Any

from fastapi import APIRouter, HTTPException, Query

from ...db import session_scope
from ...evolution import mantra as mantra_store
from ...exchange.client import BinanceError, NoCredentials
from ...exchange.client import client as binance_client
from ...models import Signal, TradeRecord
from ...news import service as news_service
from ...services import state
from ...services.settings import get_engine, get_public, normalize_mode
from ...services.symbols import ordered_symbols, pool_rows

router = APIRouter(prefix="/public", tags=["public"])


def _require_site() -> None:
    if not get_public().site_enabled:
        raise HTTPException(status_code=503, detail="公开站点已关闭")


def _pool_symbols() -> list[str]:
    """标的池 (启用) —— 前台展示严格跟随它, 且顺序即添加顺序."""
    return ordered_symbols(enabled_only=True)


def _candle(candle: Any) -> dict[str, float]:
    # 币安 klines 的 openTime 本身就是毫秒时间戳, 不能再乘 1000
    return {
        "t": int(candle.open_time),
        "o": float(candle.open),
        "h": float(candle.high),
        "l": float(candle.low),
        "c": float(candle.close),
        "v": float(candle.volume),
    }


@router.get("/pool")
def public_pool() -> dict[str, Any]:
    """前台标的池: 严格按后台添加顺序返回 (空池返回空列表)."""
    _require_site()
    rows = pool_rows(enabled_only=True)
    return {
        "symbols": [
            {
                "symbol": row["symbol"],
                "name": row["name"],
                "tier": row["tier"],
                "tier_label": row["tier_label"],
                "order": index + 1,
                "added_at": row["created_at"],
            }
            for index, row in enumerate(rows)
        ],
        "count": len(rows),
        "empty": not rows,
        "updated_at": int(time.time()),
    }


INTERVAL_MS = {
    "1m": 60_000,
    "3m": 180_000,
    "5m": 300_000,
    "15m": 900_000,
    "30m": 1_800_000,
    "1h": 3_600_000,
    "4h": 14_400_000,
    "1d": 86_400_000,
}


def _kline_payload(symbol: str, interval: str, candles: list[Any]) -> dict[str, Any]:
    step = INTERVAL_MS.get(interval, 60_000)
    last = candles[-1] if candles else None
    return {
        "symbol": symbol,
        "interval": interval,
        "items": [_candle(c) for c in candles],
        "count": len(candles),
        "last_open_time": int(last.open_time) if last else 0,
        "next_close_at": int(last.open_time) + step if last else 0,
        "server_time": int(time.time() * 1000),
        "updated_at": int(time.time()),
    }


@router.get("/klines")
async def public_klines(
    symbol: str = Query(..., description="合约, 必须在标的池中"),
    interval: str = Query(default="5m"),
    limit: int = Query(default=600, ge=20, le=1000),
    fresh: bool = Query(default=True, description="绕过服务端缓存, 取最新一根未收盘蜡烛"),
) -> dict[str, Any]:
    """单标的 K 线: 只允许池内合约, 数据来自币安实时接口 (无演示数据)."""
    _require_site()
    target = symbol.upper()
    pool = _pool_symbols()
    if not pool:
        return {"symbol": target, "interval": interval, "items": [], "empty_pool": True}
    if target not in pool:
        raise HTTPException(status_code=404, detail=f"{target} 不在标的池中, 前台不展示池外行情")
    try:
        candles = await binance_client.klines(target, interval, limit, fresh=fresh)
    except (BinanceError, NoCredentials) as exc:
        raise HTTPException(status_code=503, detail=f"币安行情暂不可用: {exc}") from exc
    return _kline_payload(target, interval, candles)


@router.get("/quotes")
async def public_quotes() -> dict[str, Any]:
    """池内标的的 24h 快照 (价格/涨跌/成交额), 给前台 K 线列表用."""
    _require_site()
    pool = _pool_symbols()
    if not pool:
        return {"symbols": [], "tickers": {}, "updated_at": int(time.time())}
    try:
        rows = await binance_client.tickers(pool)
    except (BinanceError, NoCredentials) as exc:
        raise HTTPException(status_code=503, detail=f"币安行情暂不可用: {exc}") from exc
    return {
        "symbols": pool,
        "tickers": {
            symbol: {
                "last": float(t.last),
                "change_pct": float(t.change_pct),
                "high": float(t.high),
                "low": float(t.low),
                "quote_volume": float(t.quote_volume),
                "ts": int(t.ts),
            }
            for symbol, t in rows.items()
        },
        "updated_at": int(time.time()),
    }


@router.get("/klines/batch")
async def public_klines_batch(
    interval: str = Query(default="5m"),
    limit: int = Query(default=120, ge=20, le=300),
    max_symbols: int = Query(default=12, ge=1, le=20),
    fresh: bool = Query(default=False, description="绕过缓存"),
) -> dict[str, Any]:
    """池内多标的 K 线 (按添加顺序), 一次请求渲染整个前台 K 线区.

    单个标的取数失败不会影响其它标的: 错误记录在 `errors`, 前台显示该标的
    "行情暂不可用", 而不是整块空白.
    """
    _require_site()
    pool = _pool_symbols()[:max_symbols]
    if not pool:
        return {
            "symbols": [],
            "items": {},
            "tickers": {},
            "errors": {},
            "interval": interval,
            "empty": True,
            "updated_at": int(time.time()),
        }
    items: dict[str, list[dict[str, float]]] = {}
    errors: dict[str, str] = {}
    for symbol in pool:
        try:
            candles = await binance_client.klines(symbol, interval, limit, fresh=fresh)
            items[symbol] = [_candle(c) for c in candles]
        except (BinanceError, NoCredentials) as exc:
            errors[symbol] = str(exc)[:160]
    tickers: dict[str, dict[str, float]] = {}
    try:
        rows = await binance_client.tickers(pool)
        tickers = {
            symbol: {
                "last": float(t.last),
                "change_pct": float(t.change_pct),
                "high": float(t.high),
                "low": float(t.low),
                "quote_volume": float(t.quote_volume),
                "ts": int(t.ts),
            }
            for symbol, t in rows.items()
        }
    except (BinanceError, NoCredentials):
        tickers = {}
    return {
        "symbols": pool,
        "items": items,
        "tickers": tickers,
        "errors": errors,
        "interval": interval,
        "empty": False,
        "updated_at": int(time.time()),
    }


@router.get("/config")
def public_config() -> dict[str, Any]:
    """前台启动时读取的站点元信息 (站点关闭时也允许读取, 以便展示维护页)."""
    cfg = get_public()
    engine_cfg = get_engine()
    return {
        "site_enabled": cfg.site_enabled,
        "site_title": cfg.site_title,
        "site_tagline": cfg.site_tagline,
        "hero_note": cfg.hero_note,
        "show_engine_status": cfg.show_engine_status,
        "show_mantras": cfg.show_mantras,
        "show_performance": cfg.show_performance,
        "mode": engine_cfg.mode,
        "version": __import__("nanoquant").__version__,
        "server_time": int(time.time()),
    }


@router.get("/overview")
async def public_overview() -> dict[str, Any]:
    """公开首页概览: 标的池 + 资讯情绪 + 引擎状态 (不含任何行情数据)."""
    _require_site()
    cfg = get_public()
    pool = _pool_symbols()

    payload: dict[str, Any] = {
        "updated_at": int(time.time()),
        "symbol_count": len(pool),
        "pool": pool,
        "pool_empty": not pool,
        "news": news_service.overview(),
    }
    if cfg.show_engine_status:
        engine_cfg = get_engine()
        payload["engine"] = {
            "enabled": engine_cfg.enabled,
            "mode": normalize_mode(engine_cfg.mode),
            "last_cycle_ts": state.get_int("last_cycle_ts", 0),
        }
    return payload


@router.get("/news")
def public_news(
    limit: int = Query(default=30, ge=1, le=100),
    source: str = "",
) -> dict[str, Any]:
    _require_site()
    return {
        "items": news_service.list_items(limit=limit, source=source),
        "sentiment": news_service.overview(),
    }


@router.get("/mantras")
def public_mantras(limit: int = Query(default=8, ge=1, le=30)) -> dict[str, Any]:
    """脱敏心法: 只给标题/分类/健康度, 不给规则正文."""
    _require_site()
    cfg = get_public()
    if not cfg.show_mantras:
        return {"enabled": False, "items": [], "stats": None}
    lessons = mantra_store.rank_lessons(limit)
    return {
        "enabled": True,
        "items": [
            {
                "id": m["id"],
                "title": m["title"],
                "category": m["category"],
                "health": m["health"],
                "version": m["version"],
            }
            for m in lessons
        ],
        "stats": mantra_store.stats(),
    }


@router.get("/performance")
def public_performance(window: int = Query(default=200, ge=10, le=1000)) -> dict[str, Any]:
    """绩效: 默认关闭; 开启后也只返回比例指标, 不含金额."""
    _require_site()
    cfg = get_public()
    if not cfg.show_performance:
        return {"enabled": False}
    # 绩效只统计当前交易环境, 避免模拟盘与实盘混在一起误导
    mode = normalize_mode(get_engine().mode)
    with session_scope() as s:
        trades = (
            s.query(TradeRecord)
            .filter(TradeRecord.mode == mode)
            .order_by(TradeRecord.closed_at.desc())
            .limit(window)
            .all()
        )
        signals = s.query(Signal).filter(Signal.mode == mode).count()
    total = len(trades)
    wins = sum(1 for t in trades if t.pnl > 0)
    holds = [t.hold_seconds for t in trades]
    gross_win = sum(t.pnl for t in trades if t.pnl > 0)
    gross_loss = abs(sum(t.pnl for t in trades if t.pnl <= 0))
    return {
        "enabled": True,
        "trades": total,
        "signals": signals,
        "win_rate": round(wins / total * 100, 2) if total else 0.0,
        "profit_factor": round(gross_win / gross_loss, 3) if gross_loss > 0 else (999.0 if gross_win > 0 else 0.0),
        "avg_hold_seconds": int(sum(holds) / len(holds)) if holds else 0,
        "window": window,
    }
