"""资讯聚合服务: 抓取 / 去重 / 情绪 / 摘要 / 源诊断."""
from __future__ import annotations

import asyncio
import hashlib
import time
from typing import Any

from ..ai.client import ai_client
from ..ai.decision import extract_json
from ..ai.prompts import news_summary_messages
from ..db import session_scope
from ..logging_setup import get_logger
from ..models import NewsItem
from ..services import events
from ..services.settings import get_ai, get_news
from .sentiment import clean_text, extract_symbols, score_text
from .sources import (
    FETCHERS,
    PROXY_HINT_SOURCES,
    SOURCE_LABELS,
    SourceError,
    mask_proxy,
)

log = get_logger("news")

_LAST: dict[str, Any] = {"ts": 0, "status": {}, "count": 0, "inserted": 0}
_LOCK = asyncio.Lock()


def _dedupe_key(title: str, url: str, source: str) -> str:
    raw = (url or f"{source}:{title}").lower().strip()
    return hashlib.sha1(raw.encode("utf-8")).hexdigest()


def _active_sources(cfg) -> list[str]:
    """配置里启用的源; 若填了 RSSHub 地址则自动挂上币安公告镜像."""
    sources = [s for s in cfg.sources if s in FETCHERS]
    if (cfg.rsshub_base or "").strip() and "binance_announcement_rsshub" not in sources:
        sources.append("binance_announcement_rsshub")
    return sources


async def _run_source(name: str, cfg) -> tuple[str, list, str, int, str]:
    """返回 (源名, 条目, 错误, 耗时ms, 出错URL)."""
    fn = FETCHERS.get(name)
    if fn is None:
        return name, [], "未知资讯源", 0, ""
    started = time.perf_counter()
    budget = cfg.request_timeout_sec + 5
    try:
        items = await asyncio.wait_for(fn(cfg.max_items_per_source), timeout=budget)
        cost = int((time.perf_counter() - started) * 1000)
        if not items:
            return name, [], "接口可达但未解析出条目（结构可能已变化）", cost, ""
        return name, items, "", cost, ""
    except asyncio.TimeoutError:
        return name, [], f"超时 (> {budget}s)", int((time.perf_counter() - started) * 1000), ""
    except SourceError as exc:
        return name, [], exc.message[:220], int((time.perf_counter() - started) * 1000), exc.url
    except Exception as exc:  # noqa: BLE001
        return name, [], f"{type(exc).__name__}: {exc}"[:200], int((time.perf_counter() - started) * 1000), ""


async def refresh(force: bool = False) -> dict[str, Any]:
    cfg = get_news()
    if not cfg.enabled:
        return {"ok": False, "error": "资讯模块已关闭"}
    now = int(time.time())
    if not force and now - int(_LAST.get("ts") or 0) < cfg.refresh_minutes * 60:
        return {"ok": True, "skipped": True, "next_in": cfg.refresh_minutes * 60 - (now - int(_LAST["ts"]))}

    async with _LOCK:
        sources = _active_sources(cfg)
        results = await asyncio.gather(*[_run_source(s, cfg) for s in sources])
        inserted = 0
        status: dict[str, Any] = {}
        for name, items, error, latency, url in results:
            status[name] = {
                "ok": not error,
                "count": len(items),
                "error": error,
                "latency_ms": latency,
                "url": url,
            }
            for item in items:
                title = clean_text(item.title)[:380]
                if len(title) < 8:
                    continue
                key = _dedupe_key(title, item.url, item.source)
                text = f"{title} {item.summary}"
                with session_scope() as s:
                    if s.query(NewsItem).filter(NewsItem.dedupe == key).first():
                        continue
                    s.add(
                        NewsItem(
                            ts=item.ts or now,
                            source=item.source,
                            title=title,
                            url=item.url,
                            summary=clean_text(item.summary)[:600],
                            sentiment=score_text(text),
                            symbols=extract_symbols(text),
                            dedupe=key,
                        )
                    )
                inserted += 1

        with session_scope() as s:
            s.query(NewsItem).filter(NewsItem.ts < now - cfg.keep_days * 86400).delete()

        if inserted == 0 and _count() == 0:
            inserted += _seed_demo()

        failed = [name for name, v in status.items() if not v["ok"]]
        _LAST.update({"ts": now, "status": status, "count": inserted, "inserted": inserted, "failed": failed})
        level = "warn" if failed and inserted == 0 else "info"
        events.record(
            level,
            "news",
            f"资讯刷新: 新增 {inserted} 条, 成功 {len(status) - len(failed)}/{len(status)} 个源"
            + (f", 失败: {', '.join(SOURCE_LABELS.get(f, f) for f in failed[:4])}" if failed else ""),
            {"sources": status},
        )
        return {"ok": True, "inserted": inserted, "sources": status, "failed": failed}


async def diagnostics() -> dict[str, Any]:
    """并发体检所有源 (含未启用的), 返回可读原因与建议."""
    cfg = get_news()
    active = _active_sources(cfg)
    results = await asyncio.gather(*[_run_source(s, cfg) for s in active])
    rows: list[dict[str, Any]] = []
    seen: set[str] = set()
    for name, items, error, latency, url in results:
        seen.add(name)
        rows.append(
            {
                "source": name,
                "label": SOURCE_LABELS.get(name, name),
                "enabled": True,
                "ok": not error,
                "count": len(items),
                "latency_ms": latency,
                "error": error,
                "url": url,
                "hint": _hint(name, error),
            }
        )
    for name in FETCHERS:
        if name in seen or name == "binance_announcement_rsshub":
            continue
        rows.append(
            {
                "source": name,
                "label": SOURCE_LABELS.get(name, name),
                "enabled": False,
                "ok": False,
                "count": 0,
                "latency_ms": 0,
                "error": "未启用",
                "url": "",
                "hint": "在「参数配置 → 资讯 → sources」中加入该源名即可启用",
            }
        )
    ok_count = sum(1 for r in rows if r["ok"])
    return {
        "ok": ok_count > 0,
        "checked_at": int(time.time()),
        "proxy": mask_proxy(cfg.proxy) or "(未配置, 直连)",
        "timeout_sec": cfg.request_timeout_sec,
        "ok_count": ok_count,
        "total": len(rows),
        "rows": rows,
        "suggestion": (
            ""
            if ok_count
            else "所有源均不可达: 请检查服务器出网/DNS, 或在「参数配置 → 资讯」填写 HTTP 代理"
        ),
    }


SOURCE_HOSTS = {
    "binance_square": "www.binance.com",
    "binance_announcement": "www.binance.com",
    "coindesk": "www.coindesk.com",
}


def _hint(name: str, error: str) -> str:
    if not error:
        return ""
    if "ConnectTimeout" in error or "ConnectError" in error or "NetworkError" in error:
        host = SOURCE_HOSTS.get(name)
        extra = f"该源需要访问 {host}" if host else "该域名在当前网络不可达"
        return f"{extra}; 可填写 HTTP 代理后重试, 或改用其它源"
    if "HTTP 403" in error:
        return "被站点风控(Cloudflare)拦截, 换网络/代理或换源"
    if "HTTP 404" in error or "HTTP 410" in error:
        return "接口已下线或地址变更, 建议更换源"
    if "超时" in error:
        return "响应超时: 检查网络质量, 或调大 request_timeout_sec"
    if "未解析出条目" in error:
        return "接口可达但结构变化, 需要更新解析规则"
    return ""


def _count() -> int:
    with session_scope() as s:
        return s.query(NewsItem).count()


def _seed_demo() -> int:
    """全源不可达时的离线占位, 明确标注来源为 demo."""
    samples = [
        ("BTC 现货 ETF 单日净流入 2.4 亿美元, 连续三日为正", 0.7, ["BTCUSDT"]),
        ("以太坊 Layer2 活跃地址创季度新高, 手续费降至低位", 0.45, ["ETHUSDT"]),
        ("某交易所宣布将上线新的永续合约品种", 0.3, []),
        ("市场杠杆率快速上升, 分析师提示短期回调风险", -0.4, ["BTCUSDT", "ETHUSDT"]),
        ("SOL 生态 DEX 交易量环比下滑 18%", -0.35, ["SOLUSDT"]),
        ("监管机构就稳定币储备披露征求意见", -0.2, []),
    ]
    now = int(time.time())
    count = 0
    with session_scope() as s:
        for i, (title, sentiment, symbols) in enumerate(samples):
            key = _dedupe_key(title, "", "demo")
            if s.query(NewsItem).filter(NewsItem.dedupe == key).first():
                continue
            s.add(
                NewsItem(
                    ts=now - i * 900,
                    source="demo",
                    title=title,
                    url="",
                    summary="离线演示数据: 当前网络无法访问外部资讯源, 此条目用于展示面板结构。",
                    sentiment=sentiment,
                    symbols=symbols,
                    dedupe=key,
                )
            )
            count += 1
    return count


def list_items(
    *,
    limit: int = 60,
    source: str = "",
    symbol: str = "",
    since: int = 0,
    min_abs_sentiment: float = 0.0,
) -> list[dict[str, Any]]:
    with session_scope() as s:
        q = s.query(NewsItem).order_by(NewsItem.ts.desc())
        if source:
            q = q.filter(NewsItem.source == source)
        if since:
            q = q.filter(NewsItem.ts >= since)
        rows = q.limit(min(limit * 4, 800)).all()
        out: list[dict[str, Any]] = []
        for r in rows:
            if symbol and symbol not in (r.symbols or []):
                continue
            if min_abs_sentiment and abs(r.sentiment) < min_abs_sentiment:
                continue
            out.append(
                {
                    "id": r.id,
                    "ts": r.ts,
                    "source": r.source,
                    "source_label": SOURCE_LABELS.get(r.source, r.source),
                    "title": r.title,
                    "url": r.url,
                    "summary": r.summary,
                    "sentiment": round(r.sentiment, 3),
                    "symbols": r.symbols or [],
                }
            )
            if len(out) >= limit:
                break
        return out


def source_catalog() -> list[dict[str, Any]]:
    """全部源 + 是否启用 + 上次抓取结果, 供面板渲染筛选条与健康标签."""
    cfg = get_news()
    active = set(_active_sources(cfg))
    status = _LAST.get("status") or {}
    out: list[dict[str, Any]] = []
    for name in FETCHERS:
        st = status.get(name) or {}
        out.append(
            {
                "id": name,
                "label": SOURCE_LABELS.get(name, name),
                "enabled": name in active,
                "ok": bool(st.get("ok")),
                "count": int(st.get("count") or 0),
                "error": str(st.get("error") or ""),
                "latency_ms": int(st.get("latency_ms") or 0),
                "needs_proxy": name in PROXY_HINT_SOURCES,
            }
        )
    return out


def digest(limit: int = 8) -> list[dict[str, Any]]:
    items = list_items(limit=limit)
    return [
        {"title": i["title"], "sentiment": i["sentiment"], "symbols": i["symbols"]}
        for i in items
    ]


def overview() -> dict[str, Any]:
    now = int(time.time())
    since = now - 86400
    with session_scope() as s:
        rows = s.query(NewsItem).filter(NewsItem.ts >= since).all()
        positive = sum(1 for r in rows if r.sentiment > 0.15)
        negative = sum(1 for r in rows if r.sentiment < -0.15)
        neutral = len(rows) - positive - negative
        avg = sum(r.sentiment for r in rows) / len(rows) if rows else 0.0
        by_source: dict[str, int] = {}
        for r in rows:
            by_source[r.source] = by_source.get(r.source, 0) + 1
        return {
            "total_24h": len(rows),
            "positive": positive,
            "negative": negative,
            "neutral": neutral,
            "sentiment_index": round(avg, 4),
            "by_source": by_source,
            "last_refresh": _LAST.get("ts", 0),
            "source_status": _LAST.get("status", {}),
            "failed": _LAST.get("failed", []),
        }


async def ai_summary() -> dict[str, Any]:
    cfg = get_ai()
    items = list_items(limit=20)
    if not items:
        return {"ok": False, "error": "暂无资讯"}
    if not cfg.enabled:
        return {"ok": False, "error": "AI 未启用"}
    result = await ai_client.chat_with_fallback(
        news_summary_messages(items),
        purpose="news_summary",
        provider_id=cfg.provider_id,
        fallback_ids=cfg.fallback_provider_ids,
        temperature=0.3,
    )
    if not result.ok:
        return {"ok": False, "error": result.error}
    try:
        data = extract_json(result.text)
    except ValueError as exc:
        return {"ok": False, "error": str(exc), "raw": result.text[:800]}
    return {"ok": True, "provider": result.provider, "model": result.model, "summary": data, "items": len(items)}
