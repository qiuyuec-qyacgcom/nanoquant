"""轻量情绪打分与币种抽取 (不依赖模型, 保证离线可用)."""
from __future__ import annotations

import re

POSITIVE = {
    "上涨": 1.0, "突破": 1.2, "新高": 1.4, "利好": 1.3, "增持": 1.1, "回购": 1.0,
    "批准": 1.2, "通过": 0.8, "上线": 1.0, "合作": 0.9, "融资": 0.8, "流入": 0.9,
    "rally": 1.0, "surge": 1.0, "soar": 1.2, "bullish": 1.1, "record high": 1.3,
    "approval": 1.0, "partnership": 0.8, "inflow": 0.9, "upgrade": 0.9, "gain": 0.8,
}
NEGATIVE = {
    "下跌": -1.0, "暴跌": -1.5, "跌破": -1.2, "利空": -1.3, "减持": -1.0, "抛售": -1.2,
    "黑客": -1.4, "被盗": -1.4, "调查": -1.0, "起诉": -1.2, "罚款": -1.1, "清算": -1.2,
    "流出": -0.9, "暂停": -1.0, "下架": -1.4, "延期": -0.7, "风险": -0.6,
    "crash": -1.4, "plunge": -1.2, "dump": -1.1, "bearish": -1.0, "hack": -1.4,
    "lawsuit": -1.2, "outflow": -0.9, "delist": -1.4, "ban": -1.2, "liquidation": -1.2,
}

SYMBOL_ALIASES = {
    "BTC": "BTCUSDT", "BITCOIN": "BTCUSDT", "比特币": "BTCUSDT",
    "ETH": "ETHUSDT", "ETHEREUM": "ETHUSDT", "以太坊": "ETHUSDT",
    "SOL": "SOLUSDT", "SOLANA": "SOLUSDT",
    "BNB": "BNBUSDT", "币安币": "BNBUSDT",
    "DOGE": "DOGEUSDT", "狗狗币": "DOGEUSDT",
    "XRP": "XRPUSDT", "瑞波": "XRPUSDT",
    "ADA": "ADAUSDT", "LINK": "LINKUSDT", "AVAX": "AVAXUSDT",
    "ARB": "ARBUSDT", "OP": "OPUSDT", "TON": "TONUSDT",
}

_TAG_RE = re.compile(r"<[^>]+>")


def clean_text(text: str) -> str:
    return re.sub(r"\s+", " ", _TAG_RE.sub(" ", text or "")).strip()


def score_text(text: str) -> float:
    text = (text or "").lower()
    if not text:
        return 0.0
    score = 0.0
    hits = 0
    for word, weight in POSITIVE.items():
        if word.lower() in text:
            score += weight
            hits += 1
    for word, weight in NEGATIVE.items():
        if word.lower() in text:
            score += weight
            hits += 1
    if hits == 0:
        return 0.0
    return max(-1.0, min(1.0, score / max(2.0, hits * 0.9)))


def extract_symbols(text: str) -> list[str]:
    upper = (text or "").upper()
    found: set[str] = set()
    for alias, symbol in SYMBOL_ALIASES.items():
        pattern = alias if len(alias) > 3 else rf"\b{alias}\b"
        if re.search(pattern, upper, re.I):
            found.add(symbol)
    for raw in re.findall(r"\$([A-Z]{2,10})\b", upper):
        if raw + "USDT" in SYMBOL_ALIASES.values():
            found.add(raw + "USDT")
    return sorted(found)[:5]
