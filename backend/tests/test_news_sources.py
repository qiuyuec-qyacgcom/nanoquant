"""资讯源的纯解析逻辑：时间戳、字段抽取、RSS 解析与代理掩码.

这些离线即可验证；抓取失败 / 解析退化是"面板静默没内容"的常见根因。
"""
from __future__ import annotations

import pytest

from nanoquant.news.sources import (
    RawItem,
    SourceError,
    _extract_items,
    _parse_rss,
    _parse_rss_time,
    _parse_ts,
    _pick,
    mask_proxy,
)


def test_mask_proxy_hides_credentials():
    assert mask_proxy("http://user:pass@host:8080") == "http://***@host:8080"
    assert mask_proxy("socks5://u:p@1.2.3.4") == "socks5://***@1.2.3.4"
    assert mask_proxy("http://noauth") == "http://noauth"
    assert mask_proxy("") == ""


def test_parse_ts_seconds_vs_millis():
    assert _parse_ts(1_700_000_000) == 1_700_000_000  # 秒级，不动
    assert _parse_ts(1_700_000_000_000) == 1_700_000_000  # 毫秒级 → 秒
    assert _parse_ts("1700000000") == 1_700_000_000
    assert _parse_ts("not-a-number") == 0
    assert _parse_ts(None) == 0


def test_pick_prefers_first_present_key():
    assert _pick({"url": "a", "link": "b"}, ("url", "link")) == "a"
    assert _pick({"link": "b"}, ("url", "link")) == "b"
    assert _pick({}, ("url", "link")) == ""
    assert _pick({"url": "  "}, ("url",)) == "", "空白串视同缺失"


def test_extract_items_walks_nested_structure():
    data = {
        "data": {
            "list": [
                {"title": "Bitcoin hits new all-time high", "url": "https://x/1", "publishTime": 1_700_000_000_000},
                {"title": "Ethereum upgrade scheduled", "shareUrl": "https://x/2", "releaseDate": 1_700_000_100},
            ]
        }
    }
    items = _extract_items(data, "demo")
    assert len(items) == 2
    assert items[0].title.startswith("Bitcoin hits")
    assert items[0].url == "https://x/1"
    assert items[0].ts == 1_700_000_000
    assert items[0].source == "demo"


def test_extract_items_dedupes_and_skips_short():
    data = [{"title": "dup title long enough"}, {"title": "dup title long enough"}, {"title": "short"}]
    items = _extract_items(data, "demo")
    assert len(items) == 1, "重复标题去重 + 短标题过滤"


def test_parse_rss_standard_xml():
    xml = b"""<?xml version="1.0"?>
    <rss><channel>
      <item>
        <title>First headline</title>
        <link>https://example.com/1</link>
        <description>&lt;p&gt;Body text&lt;/p&gt;</description>
        <pubDate>Wed, 01 Jan 2025 00:00:00 GMT</pubDate>
      </item>
    </channel></rss>"""
    items = _parse_rss(xml, "cointelegraph", 10)
    assert len(items) == 1
    assert items[0].title == "First headline"
    assert items[0].url == "https://example.com/1"
    assert "Body text" in items[0].summary
    assert items[0].source == "cointelegraph"
    assert items[0].ts == 1_735_689_600  # 2025-01-01 00:00:00 UTC


def test_parse_rss_atom_feed():
    xml = b"""<?xml version="1.0"?>
    <feed xmlns="http://www.w3.org/2005/Atom">
      <entry>
        <title>Atom entry</title>
        <link href="https://example.com/a"/>
        <summary>Atom summary</summary>
        <updated>2025-01-01T00:00:00Z</updated>
      </entry>
    </feed>"""
    items = _parse_rss(xml, "theblock", 10)
    assert len(items) == 1
    assert items[0].title == "Atom entry"
    assert items[0].url == "https://example.com/a"


def test_parse_rss_cdata_fallback():
    """XML 解析失败时退化为 CDATA 正则，绝不返回空导致资讯页空白."""
    broken = b'<rss><channel><title><![CDATA[channel]]></title><item><title><![CDATA[Headline one]]></title></item><item><title><![CDATA[Headline two]]></title></item></channel></rss>'
    items = _parse_rss(broken, "newsbtc", 10)
    # channel 的 title 被跳过（从 [1:] 开始），正文两条都进来
    assert [it.title for it in items] == ["Headline one", "Headline two"]


def test_parse_rss_time_fallbacks():
    assert _parse_rss_time("Wed, 01 Jan 2025 00:00:00 GMT") == 1_735_689_600
    assert _parse_rss_time("") == 0
    assert _parse_rss_time("garbage-date") == 0


def test_source_error_carries_context():
    err = SourceError("binance_square", "https://x", "HTTP 429", 429)
    assert err.source == "binance_square"
    assert err.url == "https://x"
    assert err.status == 429
    assert "binance_square" in str(err) and "429" in str(err)


def test_raw_item_defaults():
    it = RawItem(title="t")
    assert it.url == "" and it.summary == "" and it.ts == 0 and it.source == ""
