"""币安 U 本位合约客户端 (公共行情 + 签名交易).

设计要点:
- 环境由交易环境决定: demo → testnet.binancefuture.com, live → fapi.binance.com;
  两个环境各有独立 API 凭证, 切换环境即切换域名 + 凭证 + 缓存.
- 公共接口 (合约表 / 24h ticker / K 线 / 资金费率 / 持仓量) 也在这里, 供引擎内部取价;
  面板不再单独提供行情接口, 也不存在任何本地演示行情. 取不到数据就向调用方报错,
  由引擎 fail-closed (不开新仓).
- 全 Decimal 之外仍保留 float 输入, 但下单价量由 precision 层先对齐
- 双闸限频 (权重 + 订单数), 并 honoured 交易所封禁
- 结果未知 (超时/-1000/-1006/-1007) 抛 UncertainSubmission, 由上层"查询收敛"
"""
from __future__ import annotations

import asyncio
import base64
import hashlib
import hmac
import re
import time
from contextvars import ContextVar
from dataclasses import dataclass
from typing import Any
from urllib.parse import quote, urlencode

from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import padding
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.asymmetric.rsa import RSAPrivateKey
from cryptography.hazmat.primitives.serialization import load_pem_private_key
from cryptography.hazmat.primitives.serialization import load_der_private_key

import httpx

from ..logging_setup import get_logger
from ..security import decrypt_secret
from ..services.settings import get_data, get_engine, normalize_mode
from .precision import SymbolFilters, parse_exchange_info
from .ratelimit import OrderRateLimiter, RateLimited, WeightLimiter

log = get_logger("binance")

#: Ed25519 私钥解析结果缓存: base64(secret) -> key 对象
_KEY_CACHE: dict[str, Any] = {}


def normalize_pem(value: str) -> str:
    """把粘贴进来的 PEM / 带换行的密钥压成单行.

    用户经常直接从 openssl 输出或币安页面复制, 带 `-----BEGIN ...-----` 头尾和换行;
    请求头里塞原始 PEM 会直接触发 -2014。这里统一去掉头尾与空白。
    """
    text = (value or "").strip()
    if "-----" not in text:
        return "".join(text.split())
    # 有的客户端把 PEM 压成一行: "-----BEGIN PRIVATE KEY----- MC4C... -----END PRIVATE KEY-----"
    # 所以按"标记片段"删除, 而不是按行过滤
    text = re.sub(r"-----BEGIN[^-]*-----", "", text)
    text = re.sub(r"-----END[^-]*-----", "", text)
    return "".join(text.split())


def looks_like_public_key(value: str) -> bool:
    """Ed25519 公钥(SPKI)的 base64 以 MCow 开头, 长度约 60."""
    key = normalize_pem(value)
    return key.startswith("MCow") and len(key) >= 56


def _private_key(secret: str) -> Any | None:
    """尝试把 secret 解析成 Ed25519 私钥; 不是就给 None.

    币安支持两类 API Key:
    - HMAC: Key/Secret 都是普通字符串, 签名是 hex;
    - Ed25519: Secret 是 base64(PKCS#8 DER) 私钥, 签名是 base64。
    用"能否按 DER 解析出 Ed25519 私钥"来判定, 避免把普通字符串误判。
    """
    cached = _KEY_CACHE.get(secret, "miss")
    if cached != "miss":
        return cached
    body = normalize_pem(secret)
    key = None
    for loader in (
        lambda b: load_der_private_key(b, password=None),
        lambda b: load_pem_private_key(b, password=None),
    ):
        try:
            key = loader(body.encode() if isinstance(body, str) and b"BEGIN" in body.encode() else base64.b64decode(body, validate=True))
            break
        except Exception:  # noqa: BLE001
            continue
    if key is None or not isinstance(key, (Ed25519PrivateKey, RSAPrivateKey)):
        _KEY_CACHE[secret] = None
        return None
    if len(_KEY_CACHE) > 8:
        _KEY_CACHE.clear()
    _KEY_CACHE[secret] = key
    return key


def sign_query(query: str, secret: str) -> str:
    """按密钥类型签名: Ed25519 / RSA → base64, HMAC-SHA256 → hex."""
    key = _private_key(secret)
    if isinstance(key, Ed25519PrivateKey):
        return base64.b64encode(key.sign(query.encode())).decode()
    if isinstance(key, RSAPrivateKey):
        return base64.b64encode(
            key.sign(query.encode(), padding.PKCS1v15(), hashes.SHA256())
        ).decode()
    return hmac.new(normalize_pem(secret).encode(), query.encode(), hashlib.sha256).hexdigest()


#: 分段耗时采集点: broker 在下单前 set 一个 dict, `_request` 负责填 wait/sign/http
STAGE_SINK: ContextVar[dict[str, float] | None] = ContextVar("nq_stage_sink", default=None)

UNCERTAIN_CODES = {-1000, -1006, -1007, -1021}


class BinanceError(RuntimeError):
    def __init__(self, code: int, message: str, status: int = 0) -> None:
        super().__init__(f"[{code}] {message}")
        self.code = code
        self.message = message
        self.status = status


class UncertainSubmission(BinanceError):
    """提交结果未知: 必须查询确认, 禁止直接重发."""


class NoCredentials(RuntimeError):
    pass


@dataclass
class Ticker:
    symbol: str
    last: float
    change_pct: float = 0.0
    high: float = 0.0
    low: float = 0.0
    quote_volume: float = 0.0
    bid: float = 0.0
    ask: float = 0.0
    ts: int = 0


@dataclass
class Candle:
    open_time: int
    open: float
    high: float
    low: float
    close: float
    volume: float
    quote_volume: float = 0.0
    trades: int = 0


class BinanceClient:
    def __init__(self) -> None:
        self._http: httpx.AsyncClient | None = None
        self._key = ""
        self._secret = ""
        self._time_offset = 0.0
        self.weight = WeightLimiter()
        self.orders = OrderRateLimiter()
        self._base = ""
        self._base_pos = 0
        self._env = ""
        self._filters: dict[str, SymbolFilters] = {}
        self._filters_ts = 0.0
        self._tickers: dict[str, Ticker] = {}
        self._tickers_ts = 0.0
        self._klines: dict[tuple[str, str, int], tuple[float, list[Candle]]] = {}
        self._books: dict[str, tuple[float, float]] = {}
        self._books_ts = 0.0
        self._lock = asyncio.Lock()

    # -------------------------------------------------------------- lifecycle
    def current_env(self) -> str:
        return normalize_mode(get_engine().mode)

    def _ensure_env(self) -> str:
        """环境变化时丢弃域名与缓存, 避免用测试网的价去跑实盘."""
        env = self.current_env()
        if env != self._env:
            self._env = env
            self._base = ""
            self._base_pos = 0
            self._time_offset = 0.0
            self._filters = {}
            self._filters_ts = 0.0
            self._tickers = {}
            self._tickers_ts = 0.0
            self._klines = {}
            self._books = {}
            self._books_ts = 0.0
        return env

    async def _client(self) -> httpx.AsyncClient:
        if self._http is not None:
            return self._http
        cfg = get_data()
        kwargs: dict[str, Any] = {
            "timeout": httpx.Timeout(max(10, cfg.request_timeout_sec), connect=8.0),
            "limits": httpx.Limits(max_connections=10, max_keepalive_connections=5),
            "headers": {"User-Agent": "NanoQuant/1.0"},
        }
        if cfg.proxy:
            kwargs["proxy"] = cfg.proxy
        self._http = httpx.AsyncClient(**kwargs)
        return self._http

    async def close(self) -> None:
        if self._http is not None:
            await self._http.aclose()
            self._http = None

    def _credentials(self, *, required: bool) -> tuple[str, str]:
        cfg = get_data()
        env = self._ensure_env()
        if env == "live":
            self._key = decrypt_secret(cfg.live_api_key_enc)
            self._secret = decrypt_secret(cfg.live_api_secret_enc)
        else:
            self._key = decrypt_secret(cfg.demo_api_key_enc)
            self._secret = decrypt_secret(cfg.demo_api_secret_enc)
        if required and (not self._key or not self._secret):
            label = "实盘" if env == "live" else "模拟盘"
            raise NoCredentials(f"未配置{label}环境的币安 API Key/Secret")
        return self._key, self._secret

    def _bases(self) -> list[str]:
        cfg = get_data()
        env = self._ensure_env()
        if env == "demo":
            urls = [cfg.demo_base_url.strip().rstrip("/")]
            urls += [u.strip().rstrip("/") for u in cfg.demo_backup_urls if u.strip()]
            return [u for u in dict.fromkeys(urls) if u]
        urls = [cfg.live_base_url.strip().rstrip("/")]
        urls += [u.strip().rstrip("/") for u in cfg.backup_base_urls if u.strip()]
        return [u for u in dict.fromkeys(urls) if u]

    def _rotate_base(self) -> bool:
        """切到下一条线路 (多出口/反代容灾); 只有一条时返回 False."""
        bases = self._bases()
        if len(bases) <= 1:
            return False
        current = self._base or bases[0]
        try:
            pos = bases.index(current)
        except ValueError:
            pos = -1
        self._base_pos = (pos + 1) % len(bases)
        self._base = bases[self._base_pos]
        log.warning("交易所线路切换: %s", self._base)
        return True

    async def sync_time(self) -> int:
        client = await self._client()
        self._ensure_env()
        for base in self._bases():
            try:
                r = await client.get(f"{base}/fapi/v1/time", timeout=6)
                if r.status_code == 200:
                    server = int(r.json()["serverTime"])
                    self._time_offset = server / 1000.0 - time.time()
                    self._base = base
                    return int(abs(self._time_offset) * 1000)
            except Exception:  # noqa: BLE001
                continue
        return -1

    # ---------------------------------------------------------------- request
    async def _request(
        self,
        method: str,
        path: str,
        params: dict[str, Any] | None = None,
        *,
        signed: bool = True,
        weight: int = 1,
        order: bool = False,
        _retry: bool = True,
    ) -> Any:
        client = await self._client()
        cfg = get_data()
        self._ensure_env()
        t_enter = time.perf_counter()
        try:
            await self.weight.acquire(weight)
        except RateLimited as exc:
            raise BinanceError(-1003, str(exc)) from exc
        if order:
            await self.orders.acquire()
        if not self._base:
            # 首次调用的校时属于连接准备, 不能算进签名耗时
            await self.sync_time()
        t_wait = time.perf_counter()
        base = self._base or self._bases()[0]
        payload = {k: v for k, v in (params or {}).items() if v is not None}
        headers: dict[str, str] = {}
        if signed:
            key, secret = self._credentials(required=True)
            key = normalize_pem(key)
            payload["timestamp"] = int((time.time() + self._time_offset) * 1000)
            payload["recvWindow"] = int(cfg.request_timeout_sec * 1000) if cfg.request_timeout_sec < 6 else 5000
            query = urlencode(payload, doseq=True)
            signature = sign_query(query, secret)
            # Ed25519/RSA 的签名是 base64, 含 '+' '/' '=' —— 必须再次编码,
            # 否则 '+' 会被服务端当成空格, 造成"时好时坏"的 -1022。
            query = f"{query}&signature={quote(signature, safe='')}"
            headers["X-MBX-APIKEY"] = key
            url = f"{base}{path}?{query}"
        else:
            # 公共 GET 同样要带 query: 旧实现直接丢了 params, 导致
            # /fapi/v1/klines 缺 symbol 被币安拒绝(-1102)
            query = urlencode(payload, doseq=True)
            url = f"{base}{path}" + (f"?{query}" if query else "")
        started = time.perf_counter()
        try:
            if method.upper() == "GET":
                r = await client.get(url, headers=headers)
            elif method.upper() == "DELETE":
                r = await client.delete(url, headers=headers)
            elif method.upper() == "POST":
                r = await client.post(url, headers=headers)
            else:
                r = await client.put(url, headers=headers)
        except (httpx.TimeoutException, httpx.NetworkError) as exc:
            if method.upper() in {"POST", "PUT", "DELETE"}:
                raise UncertainSubmission(-1, f"网络异常, 结果未知: {exc}") from exc
            # 只读请求可以安全换线路重试一次; 写请求保持"结果未知"语义, 不重发
            if _retry and self._rotate_base():
                return await self._request(
                    method, path, params, signed=signed, weight=weight, order=order, _retry=False
                )
            raise BinanceError(-1, f"网络异常: {exc}") from exc
        finally:
            now = time.perf_counter()
            elapsed = (now - started) * 1000
            if elapsed > 1500:
                log.warning("%s %s 耗时 %.0fms", method, path, elapsed)
            sink = STAGE_SINK.get()
            if sink is not None:
                sink["wait_ms"] = (t_wait - t_enter) * 1000
                sink["sign_ms"] = (started - t_wait) * 1000
                sink["http_ms"] = elapsed

        try:
            used = int(r.headers.get("x-mbx-used-weight-1m", "0") or 0)
            if used:
                self.weight.observe_weight(used)
        except ValueError:
            pass

        if r.status_code >= 400:
            try:
                body = r.json()
            except Exception:  # noqa: BLE001
                body = {"code": -1, "msg": r.text[:200]}
            code = int(body.get("code", -1))
            msg = str(body.get("msg", ""))[:300]
            if code == -2014 and signed:
                if looks_like_public_key(self._key):
                    msg = (
                        f"{msg}（当前 API Key 是一段公钥；币安要求填的是创建密钥后"
                        "返回的 API Key，公钥只用于在网页上注册）"
                    )
                elif _private_key(self._secret) is not None:
                    msg = f"{msg}（Ed25519 私钥格式未被接受，请确认粘贴的是完整的 Secret）"
            if code == -1003:
                # 先尝试换线路 (不同出口 IP 不会被同一条封禁连累), 换不了才本地熔断
                if _retry and method.upper() == "GET" and self._rotate_base():
                    return await self._request(
                        method, path, params, signed=signed, weight=weight, order=order, _retry=False
                    )
                self.weight.observe_ban(msg)
            if code in UNCERTAIN_CODES and method.upper() != "GET":
                raise UncertainSubmission(code, msg, r.status_code)
            raise BinanceError(code, msg, r.status_code)
        if not r.text:
            return {}
        return r.json()

    # ------------------------------------------------------------ public data
    async def symbol_filters(self, force: bool = False) -> dict[str, SymbolFilters]:
        """在线合约表 (含精度与最小下单约束), 15 分钟缓存."""
        self._ensure_env()
        if not force and self._filters and time.time() - self._filters_ts < 900:
            return self._filters
        info = await self._request("GET", "/fapi/v1/exchangeInfo", signed=False, weight=1)
        parsed = parse_exchange_info(info if isinstance(info, dict) else {})
        if parsed:
            self._filters = parsed
            self._filters_ts = time.time()
        return self._filters

    async def exchange_symbols(self, force: bool = False) -> dict[str, dict]:
        """原始 exchangeInfo 行 (标的池校验用)."""
        info = await self._request("GET", "/fapi/v1/exchangeInfo", signed=False, weight=1)
        rows = info.get("symbols", []) if isinstance(info, dict) else []
        return {str(r.get("symbol")): r for r in rows if isinstance(r, dict)}

    async def tickers(self, symbols: list[str] | None = None) -> dict[str, Ticker]:
        """24h ticker: 优先读 WebSocket 快照 (零 REST 权重), REST 兜底."""
        cfg = get_data()
        self._ensure_env()

        from .ticker_stream import ticker_hub

        wanted = [s.upper() for s in (symbols or [])]
        if wanted:
            # 让上游订阅这些标的并尝试用 WS 快照直接返回
            await ticker_hub.ensure_subscribed(wanted)
            snap = ticker_hub.snapshot()
            if all(s in snap for s in wanted):
                return {s: snap[s] for s in wanted}

        # REST 兜底 (WS 尚未就绪 / 快照陈旧)
        now = time.time()
        if not self._tickers or now - self._tickers_ts >= cfg.ticker_ttl_sec:
            async with self._lock:
                if not self._tickers or now - self._tickers_ts >= cfg.ticker_ttl_sec:
                    rows = await self._request("GET", "/fapi/v1/ticker/24hr", signed=False, weight=40)
                    out: dict[str, Ticker] = {}
                    for row in rows if isinstance(rows, list) else []:
                        if not isinstance(row, dict):
                            continue
                        try:
                            # 24hr ticker **不含** bidPrice/askPrice, 旧实现直接下标
                            # 取值导致每一行都 KeyError → 整张 ticker 表恒为空
                            t = Ticker(
                                symbol=str(row.get("symbol") or ""),
                                last=float(row.get("lastPrice") or 0),
                                change_pct=float(row.get("priceChangePercent") or 0),
                                high=float(row.get("highPrice") or 0),
                                low=float(row.get("lowPrice") or 0),
                                quote_volume=float(row.get("quoteVolume") or 0),
                                bid=float(row.get("bidPrice") or 0),
                                ask=float(row.get("askPrice") or 0),
                                ts=int(now),
                            )
                        except (TypeError, ValueError):
                            continue
                        if not t.symbol:
                            continue
                        out[t.symbol] = t
                    if out:
                        self._tickers = out
                        self._tickers_ts = now
        if wanted:
            return {s: self._tickers[s] for s in wanted if s in self._tickers}
        return dict(self._tickers)

    async def book_tickers(self, symbols: list[str] | None = None) -> dict[str, tuple[float, float]]:
        """买一/卖一价 (盘口点差闸门), 2 秒缓存.

        超短线最怕"点差 + 滑点"吃掉利润: 进场前用买一卖一判断流动性,
        点差过宽直接放弃, 而不是等成交后才发现滑点。
        """
        self._ensure_env()
        now = time.time()
        if not self._books or now - self._books_ts >= 2.0:
            rows = await self._request("GET", "/fapi/v1/ticker/bookTicker", signed=False, weight=2)
            out: dict[str, tuple[float, float]] = {}
            for row in rows if isinstance(rows, list) else []:
                if not isinstance(row, dict):
                    continue
                try:
                    bid = float(row.get("bidPrice") or 0)
                    ask = float(row.get("askPrice") or 0)
                except (TypeError, ValueError):
                    continue
                symbol = str(row.get("symbol") or "")
                if symbol and bid > 0 and ask > 0:
                    out[symbol] = (bid, ask)
            if out:
                self._books = out
                self._books_ts = now
        if symbols:
            return {s: self._books[s] for s in symbols if s in self._books}
        return dict(self._books)

    async def klines(
        self, symbol: str, interval: str = "1m", limit: int = 200, *, fresh: bool = False
    ) -> list[Candle]:
        """K 线.

        `fresh=True` 绕过缓存: 前台/后台 K 线工作站每 3 秒刷新时必须走这条路径,
        否则最后一根未收盘的蜡烛会被 `kline_ttl_sec` (默认 20s) 钉住, 看起来"不实时".
        """
        cfg = get_data()
        self._ensure_env()
        key = (symbol, interval, limit)
        hit = self._klines.get(key)
        # fresh 也保留 1.5s 微缓存: 多个页面/标签同时轮询时避免重复计入权重
        ttl = 1.5 if fresh else cfg.kline_ttl_sec
        if hit and time.time() - hit[0] < ttl:
            return hit[1]
        rows = await self._request(
            "GET",
            "/fapi/v1/klines",
            {"symbol": symbol, "interval": interval, "limit": min(limit, 1500)},
            signed=False,
            weight=2,
        )
        candles = [
            Candle(
                open_time=int(r[0]),
                open=float(r[1]),
                high=float(r[2]),
                low=float(r[3]),
                close=float(r[4]),
                volume=float(r[5]),
                quote_volume=float(r[7]),
                trades=int(r[8]),
            )
            for r in rows
            if isinstance(r, list) and len(r) >= 9
        ]
        self._klines[key] = (time.time(), candles)
        return candles

    async def funding_rates(self, symbols: list[str]) -> dict[str, float]:
        rows = await self._request("GET", "/fapi/v1/premiumIndex", signed=False, weight=10)
        wanted = set(symbols)
        return {
            str(row["symbol"]): float(row["lastFundingRate"])
            for row in rows if isinstance(row, dict) and row.get("symbol") in wanted
        } if isinstance(rows, list) else {}

    async def open_interest(self, symbols: list[str]) -> dict[str, float]:
        out: dict[str, float] = {}
        for symbol in symbols[:10]:
            try:
                row = await self._request(
                    "GET", "/fapi/v1/openInterest", {"symbol": symbol}, signed=False, weight=1
                )
                out[symbol] = float(row.get("openInterest", 0))
            except Exception:  # noqa: BLE001
                continue
        return out

    # ------------------------------------------------------------------ api
    async def ping(self) -> bool:
        client = await self._client()
        self._ensure_env()
        base = self._base or self._bases()[0]
        r = await client.get(f"{base}/fapi/v1/ping", timeout=6)
        return r.status_code == 200

    async def account(self) -> dict:
        return await self._request("GET", "/fapi/v3/account", weight=5)

    async def balance(self) -> list[dict]:
        return await self._request("GET", "/fapi/v3/balance", weight=5)

    async def positions(self, symbol: str | None = None) -> list[dict]:
        data = await self._request("GET", "/fapi/v3/positionRisk", {"symbol": symbol}, weight=5)
        return data if isinstance(data, list) else []

    async def set_leverage(self, symbol: str, leverage: int) -> dict:
        return await self._request("POST", "/fapi/v1/leverage", {"symbol": symbol, "leverage": int(leverage)})

    async def set_margin_type(self, symbol: str, margin_type: str = "ISOLATED") -> dict:
        try:
            return await self._request(
                "POST", "/fapi/v1/marginType", {"symbol": symbol, "marginType": margin_type}
            )
        except BinanceError as exc:
            if exc.code == -4046:  # No need to change margin type
                return {"msg": "already"}
            raise

    async def set_position_mode(self, dual: bool) -> dict:
        return await self._request("POST", "/fapi/v1/positionSide/dual", {"dualSidePosition": "true" if dual else "false"})

    async def get_position_mode(self) -> bool:
        data = await self._request("GET", "/fapi/v1/positionSide/dual", weight=1)
        return bool(data.get("dualSidePosition"))

    async def place_order(self, **params: Any) -> dict:
        return await self._request("POST", "/fapi/v1/order", params, weight=1, order=True)

    async def place_algo_order(self, **params: Any) -> dict:
        """条件单(止损/止盈): 币安已把 closePosition 类订单迁到 Algo 接口."""
        return await self._request("POST", "/fapi/v1/algoOrder", params, weight=1, order=True)

    async def open_algo_orders(self, symbol: str) -> list[dict]:
        data = await self._request("GET", "/fapi/v1/openAlgoOrders", {"symbol": symbol}, weight=1)
        if isinstance(data, dict):
            rows = data.get("orders") or data.get("data") or []
        else:
            rows = data
        return rows if isinstance(rows, list) else []

    async def cancel_all_algo(self, symbol: str) -> int:
        """没有"一键撤全部"接口, 只能列出来逐个撤."""
        rows = await self.open_algo_orders(symbol)
        count = 0
        for row in rows:
            algo_id = row.get("algoId") if isinstance(row, dict) else None
            if not algo_id:
                continue
            try:
                await self.cancel_algo(str(algo_id))
                count += 1
            except BinanceError:
                continue
        return count

    async def cancel_algo(self, algo_id: str) -> dict:
        return await self._request("DELETE", "/fapi/v1/algoOrder", {"algoId": algo_id}, weight=1)

    async def place_test_order(self, **params: Any) -> dict:
        return await self._request("POST", "/fapi/v1/order/test", params, weight=1)

    async def cancel_order(self, symbol: str, order_id: str | None = None, client_id: str | None = None) -> dict:
        params: dict[str, Any] = {"symbol": symbol}
        if order_id:
            params["orderId"] = order_id
        if client_id:
            params["origClientOrderId"] = client_id
        return await self._request("DELETE", "/fapi/v1/order", params, weight=1)

    async def cancel_all(self, symbol: str) -> dict:
        return await self._request("DELETE", "/fapi/v1/allOpenOrders", {"symbol": symbol}, weight=1)

    async def get_order(self, symbol: str, order_id: str | None = None, client_id: str | None = None) -> dict:
        params: dict[str, Any] = {"symbol": symbol}
        if order_id:
            params["orderId"] = order_id
        if client_id:
            params["origClientOrderId"] = client_id
        return await self._request("GET", "/fapi/v1/order", params, weight=1)

    async def open_orders(self, symbol: str | None = None) -> list[dict]:
        data = await self._request("GET", "/fapi/v1/openOrders", {"symbol": symbol}, weight=1)
        return data if isinstance(data, list) else []

    async def user_trades(self, symbol: str, limit: int = 50) -> list[dict]:
        data = await self._request("GET", "/fapi/v1/userTrades", {"symbol": symbol, "limit": limit}, weight=5)
        return data if isinstance(data, list) else []

    async def income(self, income_type: str = "REALIZED_PNL", limit: int = 100) -> list[dict]:
        data = await self._request(
            "GET", "/fapi/v1/income", {"incomeType": income_type, "limit": limit}, weight=30
        )
        return data if isinstance(data, list) else []

    async def countdown_cancel_all(self, symbol: str, ms: int = 120_000) -> dict:
        return await self._request(
            "POST", "/fapi/v1/countdownCancelAll", {"symbol": symbol, "countdownTime": ms}, weight=10
        )

    async def exchange_info(self) -> dict:
        return await self._request("GET", "/fapi/v1/exchangeInfo", signed=False, weight=1)


client = BinanceClient()
