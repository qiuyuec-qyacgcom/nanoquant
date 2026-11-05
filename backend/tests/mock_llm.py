"""本地 OpenAI 兼容 Mock 服务, 用于离线验证 AI 链路.

启动: python tests/mock_llm.py [port]
"""
from __future__ import annotations

import json
import sys
import time

from fastapi import FastAPI, Request
import uvicorn

app = FastAPI(title="NanoQuant Mock LLM")


def _decision(text: str) -> dict:
    bearish = "short" in text or "空头" in text
    action = "open_short" if bearish else "open_long"
    return {
        "action": action,
        "confidence": 0.78,
        "leverage": 18,
        "sl_pct": 0.6,
        "tp_pct": 1.2,
        "reason": "EMA 结构同向且量比 1.12, ATR 占比 0.31%, 预期波动覆盖双边手续费 4 倍以上",
        "risk_flags": ["资金费率为正, 多头略拥挤"],
        "invalidation": "若 5 分钟内价格反向突破前一根 K 线低点则判断失效",
    }


def _reply(messages: list[dict]) -> str:
    text = "\n".join(m.get("content", "") for m in messages)
    if "只回答两个字" in text:
        return "正常"
    if '"ok":true' in text.replace(" ", ""):
        return '{"ok":true}'
    if "复盘官" in text:
        return json.dumps(
            {
                "lessons": [
                    {
                        "title": "高波动期缩短持仓周期",
                        "rule": "当 ATR 占比高于 0.5% 且持仓超过 5 分钟仍未触及止盈一半时, 主动减半仓位并上移止损到成本价.",
                        "category": "exit",
                        "confidence": 0.63,
                        "evidence": "该笔交易 ATR 0.52%, 持仓 6 分钟后回撤",
                    }
                ],
                "verdict": "信号方向正确但离场偏慢",
                "score": 62,
            },
            ensure_ascii=False,
        )
    if "自我进化官" in text:
        return json.dumps(
            {
                "diagnosis": ["止盈目标偏保守, 平均盈亏比不足 1.2", "亏损集中在入场后前 3 分钟"],
                "lessons": [
                    {
                        "title": "分批止盈保留趋势仓",
                        "rule": "当浮盈达到 1 倍 ATR 时先平掉一半仓位, 剩余仓位改用 0.4% 移动止损跟随, 直到信号反转.",
                        "category": "exit",
                        "confidence": 0.6,
                        "evidence": "近 10 笔中 7 笔在 1 倍 ATR 附近回吐",
                    }
                ],
                "retire": [],
                "adjust": ["方向: 提高入场确认严格度", "方向: 降低单笔名义额 10%"],
            },
            ensure_ascii=False,
        )
    if "资讯分析员" in text:
        return json.dumps(
            {"summary": "市场情绪偏中性略多, 资金面稳定, 短期以震荡为主。", "sentiment": 0.22, "hot": ["ETF 资金流", "Layer2"], "risk": "杠杆率抬升时注意插针"},
            ensure_ascii=False,
        )
    if "行情分析助手" in text:
        return json.dumps(
            {
                "bias": "bullish",
                "confidence": 0.66,
                "horizon": "1h",
                "key_levels": {"support": [96000, 94800], "resistance": [98200, 99500]},
                "drivers": ["量能温和放大", "资金费率中性"],
                "risks": ["宏观数据公布前的波动"],
                "plan": "回踩 96000 附近分批做多, 止损 94800 下方, 目标 98200。",
            },
            ensure_ascii=False,
        )
    return json.dumps(_decision(text), ensure_ascii=False)


@app.post("/v1/chat/completions")
async def chat(request: Request) -> dict:
    body = await request.json()
    text = _reply(body.get("messages") or [])
    return {
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": body.get("model", "mock"),
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 320, "completion_tokens": 120, "total_tokens": 440},
    }


@app.get("/v1/models")
async def models() -> dict:
    return {"object": "list", "data": [{"id": "mock-1", "object": "model"}, {"id": "mock-2", "object": "model"}]}


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8898
    uvicorn.run(app, host="127.0.0.1", port=port, log_level="warning")
