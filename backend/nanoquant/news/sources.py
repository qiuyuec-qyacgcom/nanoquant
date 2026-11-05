"""资讯源适配器.

两类源:
1. 结构化接口 (币安 bapi / RSS) —— 递归抽取文本字段, 与页面改版解耦
2. RSS —— 标准解析, 失败时退化为正则

网络约束:
- 请求统一走「资讯代理」(`news.proxy`), 留空复用 `data.proxy`
- 币安 bapi 与 coindesk 在部分网络下完全不可达, 因此默认源里同时挂了
  多个可直达的英文 RSS 源, 保证任一网络环境下都有内容
"""
from __future__ import annotations

import re
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from typing import Any, Callable

import httpx

from ..logging_setup import get_logger
from ..services.settings import get_data, get_news

log = get_logger("news.sources")

BROWSER_HEADERS = {
    "User-Agent": (
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
        "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36"
    ),
    "Accept": "application/rss+xml, application/xml;q=0.9, application/json;q=0.8, text/html;q=0.7, */*;q=0.5",
    "Accept-Language": "en-US,en;q=0.9,zh-CN;q=0.8",
    "Cache-Control": "no-cache",
    "Sec-Fetch-Dest": "document",
    "Sec-Fetch-Mode": "navigate",
    "Sec-Fetch-Site": "none",
    "Upgrade-Insecure-Requests": "1",
}


class SourceError(RuntimeError):
    """抓取失败 (保留 url 与原因, 供面板展示"为什么抓不到")."""

    def __init__(self, source: str, url: str, message: str, status: int = 0) -> None:
        super().__init__(f"{source}: {message}")
        self.source = source
        self.url = url
        self.message = message
        self.status = status


@dataclass
class RawItem:
    title: str
    url: str = ""
    summary: str = ""
    ts: int = 0
    source: str = ""


def http_client(timeout: float | None = None) -> httpx.AsyncClient:
    """统一出口: 走配置的代理, 带完整浏览器头 (部分站点会按 UA/头拦截)."""
    news_cfg = get_news()
    data_cfg = get_data()
    proxy = (news_cfg.proxy or data_cfg.proxy or "").strip()
    kwargs: dict[str, Any] = {
        "timeout": httpx.Timeout(timeout or news_cfg.request_timeout_sec, connect=5.0),
        "follow_redirects": True,
        "headers": BROWSER_HEADERS,
    }
    if proxy:
        kwargs["proxy"] = proxy
    return httpx.AsyncClient(**kwargs)


def mask_proxy(proxy: str) -> str:
    return re.sub(r"//[^@/]+@", "//***@", proxy or "")


_TEXT_KEYS = ("title", "content", "body", "text", "summary", "description", "articleTitle")
_URL_KEYS = ("url", "link", "shareUrl", "webLink", "sourceUrl")
_TIME_KEYS = ("publishTime", "releaseDate", "createTime", "date", "published_at", "publishDate")


def _walk(node: Any, out: list[dict[str, Any]], depth: int = 0) -> None:
    if depth > 8:
        return
    if isinstance(node, dict):
        if any(isinstance(node.get(k), str) and len(str(node.get(k))) > 12 for k in _TEXT_KEYS):
            out.append(node)
            return
        for v in node.values():
            _walk(v, out, depth + 1)
    elif isinstance(node, list):
        for v in node[:80]:
            _walk(v, out, depth + 1)


def _pick(node: dict[str, Any], keys: tuple[str, ...]) -> str:
    for k in keys:
        v = node.get(k)
        if isinstance(v, str) and v.strip():
            return v.strip()
    return ""


def _parse_ts(value: Any) -> int:
    try:
        ts = int(value)
    except (TypeError, ValueError):
        return 0
    return ts // 1000 if ts > 10**11 else ts


def _extract_items(data: Any, source: str) -> list[RawItem]:
    nodes: list[dict[str, Any]] = []
    _walk(data, nodes)
    items: list[RawItem] = []
    seen: set[str] = set()
    for node in nodes:
        title = _pick(node, _TEXT_KEYS)
        if len(title) < 8:
            continue
        title = re.sub(r"\s+", " ", title)[:380]
        if title in seen:
            continue
        seen.add(title)
        ts = 0
        for k in _TIME_KEYS:
            ts = _parse_ts(node.get(k))
            if ts:
                break
        items.append(
            RawItem(
                title=title,
                url=_pick(node, _URL_KEYS)[:700],
                summary=_pick(node, ("summary", "description", "content", "body"))[:400],
                ts=ts,
                source=source,
            )
        )
        if len(items) >= 40:
            break
    return items


# ------------------------------------------------------------------ binance
async def binance_square(limit: int = 20) -> list[RawItem]:
    """币安广场公开内容流 (需要能访问 www.binance.com)."""
    urls = [
        ("https://www.binance.com/bapi/composite/v1/public/content/square/feed", {"page": 1, "pageSize": limit}),
        ("https://www.binance.com/bapi/composite/v3/friendly/pgc/content/square/feed", {"page": 1, "pageSize": limit}),
    ]
    errors: list[str] = []
    last_url = urls[0][0]
    async with http_client() as client:
        for url, params in urls:
            last_url = url
            try:
                r = await client.get(url, params=params, headers={**BROWSER_HEADERS, "Accept": "application/json"})
                if r.status_code != 200:
                    raise RuntimeError(f"HTTP {r.status_code}")
                items = _extract_items(r.json(), "binance_square")
                if items:
                    return items
                errors.append("接口返回成功但未解析出条目")
            except Exception as exc:  # noqa: BLE001
                log.warning("币安广场 %s 不可用: %s", url, exc)
                errors.append(f"{type(exc).__name__}: {exc}"[:120])
    raise SourceError("binance_square", last_url, "; ".join(errors) or "不可达")


async def binance_announcement(limit: int = 20) -> list[RawItem]:
    """币安公告 (需要能访问 www.binance.com)."""
    urls = [
        (
            "https://www.binance.com/bapi/apex/v1/public/apex/cms/article/list/query",
            {"type": 1, "pageNo": 1, "pageSize": limit},
        ),
        (
            "https://www.binance.com/bapi/composite/v1/public/cms/article/catalog/list/query",
            {"catalogId": 48, "pageNo": 1, "pageSize": limit},
        ),
    ]
    errors: list[str] = []
    last_url = urls[0][0]
    async with http_client() as client:
        for url, params in urls:
            last_url = url
            try:
                r = await client.get(url, params=params, headers={**BROWSER_HEADERS, "Accept": "application/json"})
                if r.status_code != 200:
                    raise RuntimeError(f"HTTP {r.status_code}")
                items = _extract_items(r.json(), "binance_announcement")
                if items:
                    return items
                errors.append("接口返回成功但未解析出条目")
            except Exception as exc:  # noqa: BLE001
                log.warning("币安公告 %s 不可用: %s", url, exc)
                errors.append(f"{type(exc).__name__}: {exc}"[:120])
    raise SourceError("binance_announcement", last_url, "; ".join(errors) or "不可达")


async def binance_announcement_rsshub(limit: int = 20) -> list[RawItem]:
    """通过自建 RSSHub 抓币安公告 (墙内可选方案, 需在配置里填 rsshub_base)."""
    base = (get_news().rsshub_base or "").strip().rstrip("/")
    if not base:
        return []
    for path in ("/binance/announcement", "/binance/announcement/0"):
        items = await _fetch_rss_url(f"{base}{path}", "binance_announcement", limit)
        if items:
            # 对外仍标记为币安公告, 便于面板聚合
            for it in items:
                it.source = "binance_announcement"
            return items
    return []


# ---------------------------------------------------------------------- rss
RSS_FEEDS: dict[str, str] = {
    "cointelegraph": "https://cointelegraph.com/rss",
    "theblock": "https://www.theblock.co/rss.xml",
    "decrypt": "https://decrypt.co/feed",
    "bitcoinmagazine": "https://bitcoinmagazine.com/feed",
    "newsbtc": "https://www.newsbtc.com/feed/",
    "beincrypto": "https://beincrypto.com/feed/",
    "utoday": "https://u.today/rss",
    "coindesk": "https://www.coindesk.com/arc/outboundfeeds/rss/",
}


async def rss(source: str, limit: int = 20) -> list[RawItem]:
    url = RSS_FEEDS.get(source)
    if not url:
        return []
    return await _fetch_rss_url(url, source, limit)


async def _fetch_rss_url(url: str, source: str, limit: int) -> list[RawItem]:
    try:
        async with http_client() as client:
            r = await client.get(url)
            if r.status_code != 200:
                raise SourceError(source, url, f"HTTP {r.status_code}", r.status_code)
            content = r.content
    except SourceError:
        raise
    except Exception as exc:  # noqa: BLE001
        log.warning("RSS %s 拉取失败: %s", source, exc)
        raise SourceError(source, url, f"{type(exc).__name__}: {exc}"[:160]) from exc
    return _parse_rss(content, source, limit)


def _parse_rss(content: bytes, source: str, limit: int) -> list[RawItem]:
    items: list[RawItem] = []
    try:
        root = ET.fromstring(content)
    except ET.ParseError:
        text = content.decode("utf-8", "ignore")
        for t in re.findall(r"<title><!\[CDATA\[(.*?)\]\]></title>", text, re.S)[1 : limit + 1]:
            items.append(RawItem(title=re.sub(r"\s+", " ", t)[:380], source=source))
        return items

    ns = {"atom": "http://www.w3.org/2005/Atom", "dc": "http://purl.org/dc/elements/1.1/"}
    nodes = root.findall(".//item") or root.findall(".//atom:entry", ns)
    for node in nodes[:limit]:
        title = (
            node.findtext("title")
            or node.findtext("atom:title", default="", namespaces=ns)
            or ""
        ).strip()
        if not title:
            continue
        link = node.findtext("link") or ""
        if not link:
            link_el = node.find("atom:link", ns)
            link = link_el.get("href", "") if link_el is not None else ""
        desc = node.findtext("description") or node.findtext("atom:summary", default="", namespaces=ns) or ""
        pub = (
            node.findtext("pubDate")
            or node.findtext("atom:updated", default="", namespaces=ns)
            or node.findtext("dc:date", default="", namespaces=ns)
            or ""
        )
        items.append(
            RawItem(
                title=re.sub(r"\s+", " ", title)[:380],
                url=link[:700],
                summary=re.sub(r"<[^>]+>", " ", desc or "")[:400],
                ts=_parse_rss_time(pub),
                source=source,
            )
        )
    return items


def _parse_rss_time(raw: str) -> int:
    if not raw:
        return 0
    from email.utils import parsedate_to_datetime

    try:
        return int(parsedate_to_datetime(raw).timestamp())
    except Exception:  # noqa: BLE001
        return 0


def _bind(fn: Callable[..., Any], source: str) -> Callable[..., Any]:
    async def _fetcher(limit: int = 20) -> list[RawItem]:
        return await fn(source, limit)

    return _fetcher


FETCHERS: dict[str, Callable[..., Any]] = {
    "binance_square": binance_square,
    "binance_announcement": binance_announcement,
    "binance_announcement_rsshub": binance_announcement_rsshub,
    **{name: _bind(rss, name) for name in RSS_FEEDS},
}

SOURCE_LABELS = {
    "binance_square": "币安广场",
    "binance_announcement": "币安公告",
    "binance_announcement_rsshub": "币安公告(RSSHub)",
    "coindesk": "CoinDesk",
    "cointelegraph": "Cointelegraph",
    "theblock": "The Block",
    "decrypt": "Decrypt",
    "bitcoinmagazine": "Bitcoin Magazine",
    "newsbtc": "NewsBTC",
    "beincrypto": "BeInCrypto",
    "utoday": "U.Today",
    "demo": "离线示例",
}

# 需要访问 www.binance.com / coindesk.com 的源, 面板会提示"可能需要代理"
PROXY_HINT_SOURCES = {"binance_square", "binance_announcement", "coindesk"}
