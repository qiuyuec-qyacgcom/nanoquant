"""告警推送: 通用 Webhook (钉钉/飞书/企微) + Telegram.

设计约束:
- 只在 `warn/error` 触发 (可配最低级别), 同一类别在冷却窗口内只发一次;
- 发送是 fire-and-forget, 失败只记日志, 绝不影响交易主流程;
- 没有事件循环时 (例如写库线程) 直接跳过, 不阻塞调用方.
"""
from __future__ import annotations

import asyncio
import time
from typing import Any

import httpx

from ..logging_setup import get_logger
from .settings import get_system

log = get_logger("alerts")

_LEVELS = {"info": 0, "warn": 1, "error": 2}
_LAST: dict[str, float] = {}
_client: httpx.AsyncClient | None = None
#: 摘要缓冲: 开启 digest 后 warn 先攒着, 定时合并成一条
_PENDING: list[tuple[str, str]] = []
_LAST_DIGEST = 0.0


def _http() -> httpx.AsyncClient:
    global _client
    if _client is None or _client.is_closed:
        _client = httpx.AsyncClient(timeout=8)
    return _client


async def close() -> None:
    global _client
    if _client is not None and not _client.is_closed:
        await _client.aclose()
    _client = None


def _format(level: str, category: str, message: str, template: str = "") -> str:
    tpl = (template or "").strip()
    if tpl:
        try:
            return tpl.format(
                level=level.upper(),
                category=category,
                message=message,
                time=time.strftime("%Y-%m-%d %H:%M:%S"),
            )
        except (KeyError, IndexError, ValueError):
            log.warning("告警模板格式错误, 回落到默认格式")
    icon = {"error": "🔴", "warn": "🟠"}.get(level, "🔵")
    return f"{icon} [NanoQuant] {level.upper()} · {category}\n{message}"


def _in_quiet_hours(spec: str) -> bool:
    """判断当前是否处于静默时段 (支持跨天, 如 23:00-07:00)."""
    text = (spec or "").strip()
    if "-" not in text:
        return False
    try:
        start_text, end_text = [part.strip() for part in text.split("-", 1)]
        start_h, start_m = [int(x) for x in start_text.split(":")]
        end_h, end_m = [int(x) for x in end_text.split(":")]
    except (ValueError, TypeError):
        return False
    local = time.localtime()
    now_minutes = local.tm_hour * 60 + local.tm_min
    start = start_h * 60 + start_m
    end = end_h * 60 + end_m
    if start == end:
        return False
    if start < end:
        return start <= now_minutes < end
    return now_minutes >= start or now_minutes < end


async def _post_webhook(url: str, text: str) -> dict[str, Any]:
    # 钉钉/企微/飞书的"文本机器人"都接受这个结构
    payload = {"msgtype": "text", "text": {"content": text}}
    r = await _http().post(url, json=payload)
    return {"channel": "webhook", "ok": r.status_code < 400, "status": r.status_code, "body": r.text[:200]}


async def _post_telegram(token: str, chat_id: str, text: str) -> dict[str, Any]:
    url = f"https://api.telegram.org/bot{token}/sendMessage"
    r = await _http().post(url, json={"chat_id": chat_id, "text": text})
    return {"channel": "telegram", "ok": r.status_code < 400, "status": r.status_code, "body": r.text[:200]}


def _log(level: str, category: str, message: str, results: list[dict[str, Any]]) -> None:
    """把发送结果落库, 面板可查「到底发出去没有」."""
    if not results:
        return
    try:
        from ..db import session_scope
        from ..models import AlertLog

        channels = ",".join(str(item.get("channel")) for item in results)
        ok = all(item.get("ok") for item in results)
        error = "; ".join(
            str(item.get("error") or item.get("status")) for item in results if not item.get("ok")
        )[:500]
        with session_scope() as s:
            s.add(
                AlertLog(
                    level=level,
                    category=category,
                    message=message[:2000],
                    channels=channels,
                    ok=ok,
                    error=error,
                )
            )
    except Exception:  # noqa: BLE001
        log.warning("告警记录落库失败")


async def send(level: str, category: str, message: str) -> list[dict[str, Any]]:
    """真正发送, 返回每个通道的结果 (给测试接口用).

    分级路由: error 优先走 `alert_error_webhook_url` (值班群), 未配置时回落到默认 webhook。
    """
    cfg = get_system()
    text = _format(level, category, message, cfg.alert_template)
    webhook = cfg.alert_webhook_url
    if level == "error" and (cfg.alert_error_webhook_url or "").strip():
        webhook = cfg.alert_error_webhook_url.strip()

    # 支持逗号分隔多个值班群: error 通道可以同时发给多个人
    urls = [u.strip() for u in (webhook or "").split(",") if u.strip()]

    results: list[dict[str, Any]] = []
    for url in urls:
        try:
            results.append(await _post_webhook(url, text))
        except Exception as exc:  # noqa: BLE001
            results.append({"channel": "webhook", "ok": False, "error": str(exc)[:200]})
    if cfg.alert_telegram_token and cfg.alert_telegram_chat_id:
        try:
            results.append(await _post_telegram(cfg.alert_telegram_token, cfg.alert_telegram_chat_id, text))
        except Exception as exc:  # noqa: BLE001
            results.append({"channel": "telegram", "ok": False, "error": str(exc)[:200]})
    _log(level, category, message, results)
    return results


async def _safe_send(level: str, category: str, message: str) -> None:
    try:
        for item in await send(level, category, message):
            if not item.get("ok"):
                log.warning("告警发送失败 %s: %s", item.get("channel"), item.get("error") or item.get("status"))
    except Exception as exc:  # noqa: BLE001
        log.warning("告警发送异常: %s", str(exc)[:200])


async def flush_digest(force: bool = False) -> dict[str, Any] | None:
    """把攒下的 warn 合并成一条摘要; 未到间隔则跳过 (force 用于停机/手动触发)."""
    global _LAST_DIGEST
    cfg = get_system()
    if not _PENDING:
        _LAST_DIGEST = time.time()
        return None
    interval = max(60, cfg.alert_digest_minutes * 60)
    if not force and time.time() - _LAST_DIGEST < interval:
        return None

    items = list(_PENDING)
    _PENDING.clear()
    _LAST_DIGEST = time.time()

    counter: dict[str, int] = {}
    for category, _ in items:
        counter[category] = counter.get(category, 0) + 1
    top = "、".join(f"{cat}×{n}" for cat, n in sorted(counter.items(), key=lambda kv: -kv[1])[:5])
    text = f"过去 {cfg.alert_digest_minutes} 分钟共 {len(items)} 条告警：{top}"
    results = await send("warn", "digest", text)
    return {"count": len(items), "results": results}


def pending_digest() -> int:
    """摘要缓冲区里的条数 (诊断页展示用)."""
    return len(_PENDING)


def on_event(level: str, category: str, message: str) -> None:
    """事件总线钩子: 级别过滤 + 同类冷却 + 异步发送."""
    try:
        cfg = get_system()
    except Exception:  # noqa: BLE001
        return
    if not cfg.alert_enabled:
        return
    if _LEVELS.get(level, 0) < _LEVELS.get(cfg.alert_min_level, 1):
        return
    # 静默时段只放行 error, 避免夜里被 warn 吵醒
    if level != "error" and _in_quiet_hours(cfg.alert_quiet_hours):
        return
    # 摘要模式: warn 先攒着, 由 flush_digest 定时合并, 避免刷屏
    if level == "warn" and cfg.alert_digest_enabled:
        _PENDING.append((category, message))
        return
    has_webhook = bool(cfg.alert_webhook_url or (level == "error" and cfg.alert_error_webhook_url))
    if not has_webhook and not (cfg.alert_telegram_token and cfg.alert_telegram_chat_id):
        return
    now = time.time()
    if now - _LAST.get(category, 0.0) < max(10, cfg.alert_cooldown_sec):
        return
    _LAST[category] = now
    try:
        loop = asyncio.get_running_loop()
    except RuntimeError:
        # 写库线程等无事件循环的上下文: 直接跳过, 不能阻塞
        return
    loop.create_task(_safe_send(level, category, message))
