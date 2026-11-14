"""FastAPI 应用装配."""
from __future__ import annotations

import asyncio
import time
from contextlib import asynccontextmanager

from fastapi import FastAPI, Request
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse, JSONResponse
from fastapi.staticfiles import StaticFiles

from . import __version__
from .ai.client import ai_client
from .api.routes import (
    ai, auth, backup, backtest, evolution, news, public, signal_follow, strategies, symbols, system, trade, ws
)
from .config import APP_NAME, paths
from .db import init_db, session_scope
from .engine.worker import engine
from .exchange import client as binance_client
from .exchange.kline_stream import kline_hub
from .services import alerts, api_guard
from .services.db_writer import db_writer
from .exchange.ticker_stream import ticker_hub
from .logging_setup import get_logger, setup_logging
from .models import TradingSymbol
from .security import hash_password, verify_password
from .services import events
from .services.settings import (
    SettingsValidationError, get_security, get_system, has_explicit_key, update_group,
)

log = get_logger("main")

#: 未改出厂密码时仍然放行的接口（只能登录、改密、查当前用户、探活）
_PASSWORD_CHANGE_ALLOWED = {
    "/api/v1/auth/login",
    "/api/v1/auth/password",
    "/api/v1/auth/me",
    "/api/v1/health",
    "/api/v1/metrics",
}
#: 前台公开站点面向访客, 不涉及后台权限, 不因出厂密码被挡
_PUBLIC_API_PREFIX = "/api/v1/public/"


def bootstrap() -> None:
    init_db()
    sec = get_security()
    if not sec.password_hash:
        update_group("security", {"password_hash": hash_password("admin123"), "password_is_default": True})
        events.record("warn", "system", "已生成默认管理员 admin / admin123, 请登录后立即修改密码")
    elif not has_explicit_key("security", "password_is_default"):
        # 老库升级路径: 之前没有这个标记, 这里补一次判定 —— 密码仍是 admin123 的
        # 老实例同样必须被拦下, 否则"新装才有防护"等于没修
        if verify_password("admin123", sec.password_hash):
            update_group("security", {"password_is_default": True})
            events.record("warn", "system", "检测到仍在使用的初始密码 admin123, 已要求修改后才能继续操作")
    # 标的池不再预置默认值: 由用户在「标的池」页显式添加, 避免引擎扫描一堆用不到 (或买不起) 的品种
    with session_scope() as s:
        if s.query(TradingSymbol).count() == 0:
            log.info("标的池为空: 请在后台「标的池」页添加要交易的合约")


@asynccontextmanager
async def lifespan(app: FastAPI):
    # 注意顺序: 配置存在数据库里, 必须先建表再读日志级别 (全新部署时尤为关键)
    init_db()
    setup_logging(get_system().log_level)
    bootstrap()
    events.record("info", "system", f"{APP_NAME} v{__version__} 启动")
    async def _probe() -> None:
        await asyncio.sleep(0.5)
        try:
            # 只校时并预热合约表; 不提供任何行情接口
            await binance_client.client.sync_time()
            await binance_client.client.symbol_filters()
            # 预热到当前环境的 TCP + TLS + HTTP/2 连接:
            # 否则第一笔真实下单要额外付一次握手 (50~300ms) 的长尾
            await binance_client.client.book_tickers(["BTCUSDT"])
        except Exception as exc:  # noqa: BLE001
            log.warning("初始交易所探测失败: %s", exc)

    from .services import runtime_probe

    runtime_probe.start()
    # 每日自动备份：配置里的 auto_daily 之前没有任何执行者，backups/ 永远是空的
    from .services import backup as backup_service

    backup_service.start_auto()
    probe_task = asyncio.create_task(_probe())
    await engine.start()
    try:
        yield
    finally:
        runtime_probe.stop()
        backup_service.stop_auto()
        probe_task.cancel()
        await engine.stop()
        # 停机前把队列里的订单写完, 不让已 ack 的成交丢在内存里
        db_writer.stop()
        await alerts.close()
        await kline_hub.close()
        await ticker_hub.close()
        await ai_client.close()
        await binance_client.client.close()
        events.record("info", "system", "服务已停止")


def create_app() -> FastAPI:
    app = FastAPI(
        title=f"{APP_NAME} API",
        version=__version__,
        docs_url="/api/docs",
        openapi_url="/api/openapi.json",
        lifespan=lifespan,
    )
    app.add_middleware(
        CORSMiddleware,
        allow_origins=["*"],
        allow_credentials=False,
        allow_methods=["*"],
        allow_headers=["*"],
    )

    @app.middleware("http")
    async def _force_password_change(request: Request, call_next):
        """出厂密码未修改前，除改密外的后台接口一律 403.

        只写一条"请修改密码"的事件不构成防护：面板一旦映射到公网，
        admin123 就是一个现成的后门。这里的判定放在中间件而不是各路由上，
        是因为路由有几十个，漏一个就等于没做。
        """
        cfg = get_security()
        if not cfg.require_login or not cfg.password_is_default:
            return await call_next(request)
        path = request.url.path
        if path in _PASSWORD_CHANGE_ALLOWED or path.startswith(_PUBLIC_API_PREFIX):
            return await call_next(request)
        if path.startswith("/api/"):
            return JSONResponse(
                status_code=403,
                content={
                    "detail": "当前仍是初始密码 admin123，请先修改密码后再操作",
                    "code": "password_change_required",
                },
            )
        return await call_next(request)

    @app.middleware("http")
    async def _cache_policy(request: Request, call_next):
        """前端缓存策略.

        assets 文件名带内容 hash → 长缓存;
        index.html 必须 no-cache, 否则重新构建后浏览器仍引用旧 chunk, 懒加载 404 会白屏/跳登录.
        """
        response = await call_next(request)
        path = request.url.path
        if path.startswith("/assets/"):
            response.headers["Cache-Control"] = "public, max-age=31536000, immutable"
        elif not path.startswith("/api/"):
            response.headers["Cache-Control"] = "no-cache, no-store, must-revalidate"
        return response

    @app.middleware("http")
    async def _api_rate_limit(request: Request, call_next):
        """每 IP 每分钟请求上限（默认关闭）。健康检查与 metrics 不计数，避免探针被误伤。"""
        path = request.url.path
        exempt = {"/api/v1/health", "/api/v1/metrics", "/api/openapi.json", "/api/docs"}
        if not path.startswith("/api/") or path in exempt:
            return await call_next(request)
        try:
            from .services.settings import get_system

            limit = int(get_system().api_rate_limit_per_min)
        except Exception:  # noqa: BLE001  配置读不到时不能把 API 全部挡掉
            limit = 0
        key = request.client.host if request.client else "unknown"
        allowed, retry = api_guard.limiter.check(key, limit)
        if not allowed:
            return JSONResponse(
                status_code=429,
                content={"detail": "请求过于频繁，请稍后再试"},
                headers={"Retry-After": str(retry)},
            )
        return await call_next(request)

    @app.exception_handler(SettingsValidationError)
    async def _settings_error(_: Request, exc: SettingsValidationError) -> JSONResponse:
        return JSONResponse(status_code=400, content={"detail": exc.detail, "group": exc.group})

    @app.exception_handler(Exception)
    async def _unhandled(request: Request, exc: Exception) -> JSONResponse:  # pragma: no cover
        log.exception("未处理异常 %s %s", request.method, request.url.path)
        return JSONResponse(status_code=500, content={"detail": f"服务端异常: {type(exc).__name__}: {exc}"[:300]})

    for module in (
        auth, public, system, symbols, trade, ai, evolution, news, backup, backtest,
        signal_follow, strategies,
    ):
        app.include_router(module.router, prefix="/api/v1")
    app.include_router(ws.router, prefix="/api/v1")

    dist = paths.FRONTEND_DIST
    if dist.exists() and (dist / "index.html").exists():
        assets = dist / "assets"
        if assets.exists():
            app.mount("/assets", StaticFiles(directory=assets), name="assets")

        no_cache = {"Cache-Control": "no-cache, no-store, must-revalidate"}

        @app.get("/{full_path:path}", include_in_schema=False)
        async def spa(full_path: str):
            if full_path.startswith("api/"):
                return JSONResponse(status_code=404, content={"detail": "Not Found"})
            candidate = (dist / full_path).resolve()
            if full_path and str(candidate).startswith(str(dist.resolve())) and candidate.is_file():
                return FileResponse(candidate)
            return FileResponse(dist / "index.html", headers=no_cache)

        @app.get("/", include_in_schema=False)
        async def index():
            return FileResponse(dist / "index.html", headers=no_cache)
    else:

        @app.get("/", include_in_schema=False)
        async def placeholder() -> JSONResponse:
            return JSONResponse(
                {
                    "app": APP_NAME,
                    "version": __version__,
                    "message": "前端未构建, 请执行 cd frontend && npm install && npm run dev",
                    "docs": "/api/docs",
                    "ts": int(time.time()),
                }
            )

    return app


app = create_app()
