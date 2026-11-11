"""超短线多因子信号.

五个因子加权合成方向分 [-1, 1]:
  trend(EMA 结构) + momentum(RSI/短周期收益) + volume(量比) + funding(资金费率拥挤度) + oi(持仓量变化)

三条贯穿原则（改动前这套代码在三点上都踩坑）：

1. **缺失 ≠ 中性**。指标算不出来时（K 线不足 / 资金费率没拉到 / OI 只有一个快照）
   绝不能当成 0 分参与加权 —— 那等于用"没有信息"去稀释真实信号，
   还让下游看到一个"看起来中性"的结论。这里显式记录 `ok` / `missing`，
   只对可用因子归一化权重，趋势不可用就直接拦下。

2. **权重必须与真实参与计算的因子同源**。旧实现里 `oi_change_pct` 从来没有被赋值，
   OI 因子恒为 0，但它的权重仍留在分母里 —— 等于所有信号被系统性打了 88 折。

3. **盈亏比与时间止盈都要以 R 为单位卡下限**。R:R 只有 1.1 时要求 48% 以上胜率，
   扣掉手续费基本是负期望；时间阶梯止盈是静态收益率表，遇到宽止损会"在止损前落袋"，
   锁定的 R 小于 1，正是"赢小输大"的来源。
"""
from __future__ import annotations

import hashlib
import json
import math
import time
from dataclasses import dataclass, field
from typing import Any

from ..exchange.client import Candle
from ..services.settings import StrategySettings
from . import indicators as ta

#: 影响信号语义的配置项。任何一项变化都应改变策略指纹（可追溯"这笔单是哪个版本开的"）
_FP_KEYS = (
    "mode", "timeframe", "ema_fast", "ema_slow", "rsi_period", "atr_period",
    "entry_threshold", "exit_threshold", "volume_ratio_min",
    "atr_pct_min", "atr_pct_max", "min_24h_quote_volume",
    "sl_atr_mult", "tp_atr_mult", "min_risk_reward", "roi_min_r",
    "trailing_pct", "breakeven_at_r", "max_hold_minutes", "max_hold_minutes_chop",
    "chop_ema_gap_pct", "rsi_adaptive", "partial_tp_at_r", "partial_tp_ratio",
    "roi_table",
    "mr_window", "mr_z_entry", "mr_min_risk_reward", "mr_block_in_trend", "mr_trend_gap_pct",
    "weight_trend", "weight_momentum", "weight_volume", "weight_funding", "weight_oi",
)


@dataclass
class SignalResult:
    symbol: str
    direction: str = "flat"  # long / short / flat
    score: float = 0.0
    features: dict[str, Any] = field(default_factory=dict)
    reasons: list[str] = field(default_factory=list)
    sl_pct: float = 0.0
    tp_pct: float = 0.0
    rr: float = 0.0
    blocked: str = ""
    #: 本标的当轮实际参与打分的因子（缺失的不在其中）
    used_factors: list[str] = field(default_factory=list)
    #: 缺失的因子理由，供面板与 LLM 复审看到"这里没有数据"而不是"这里中性"
    missing: list[str] = field(default_factory=list)

    def as_dict(self) -> dict[str, Any]:
        return {
            "symbol": self.symbol,
            "direction": self.direction,
            "score": round(self.score, 4),
            "features": self.features,
            "reasons": self.reasons,
            "sl_pct": round(self.sl_pct, 4),
            "tp_pct": round(self.tp_pct, 4),
            "rr": round(self.rr, 4),
            "blocked": self.blocked,
            "used_factors": self.used_factors,
            "missing": self.missing,
        }


def _required_bars(cfg: StrategySettings) -> dict[str, int]:
    """各因子需要的 K 线根数。少了就是"算不出来"，不是"算出来是中性"。"""
    macd_bars = 26 + 9  # MACD(12,26,9) 需要慢线 + 信号线长度
    return {
        "trend": cfg.ema_slow + 1,
        "momentum": max(cfg.rsi_period + 1, macd_bars, 16),  # 16 = ret_15 需要 16 根
        "volume": 21,  # 量比基准取 20 根均值
    }


def _closed_only(candles: list[Candle]) -> list[Candle]:
    """去掉正在形成的那根 K 线。

    行情接口返回的最后一根是**未收盘**的：它的 volume 只是"这一根到目前为止"的量。
    拿它去除以 20 根完整 K 线的均量，量比会系统性偏小 —— 一手实测里 5 个主流标的
    全部被判成"量能不足 0.15 < 0.8"，等于量能闸门随机失灵。

    判定方式不依赖外部传入周期：用相邻两根的时间差推出一根的时长，
    若最后一根的结束时间还没到当前时刻，就说明它正在形成。
    历史数据（回测）天然满足"结束时间已过"，不会被误删。
    """
    if len(candles) < 3:
        return candles
    step = candles[-1].open_time - candles[-2].open_time
    if step <= 0:
        return candles
    now_ms = int(time.time() * 1000)
    # open_time 是毫秒时间戳；秒级时间戳（测试造数）天然通过后面这个判断
    if candles[-1].open_time > 10_000_000_000 and candles[-1].open_time + step > now_ms:
        return candles[:-1]
    return candles


def build_features(
    candles: list[Candle],
    cfg: StrategySettings,
    *,
    funding: float | None = None,
    open_interest: float = 0.0,
    oi_change_pct: float | None = None,
    change_24h: float = 0.0,
    quote_volume: float = 0.0,
) -> dict[str, Any]:
    """构造因子字典。

    `funding` / `oi_change_pct` 用 `None` 表示"没取到"，与"取到 0"区分开。
    传 0.0 会被当成真实的零费率/零变化，是两类完全不同的信息。
    末尾那根未收盘的 K 线会被剔除，避免用半根的量能判"量能不足"。
    """
    candles = _closed_only(candles)
    closes = [c.close for c in candles]
    vols = [c.volume for c in candles]
    last = candles[-1] if candles else None
    bars = len(candles)
    need = _required_bars(cfg)

    f: dict[str, Any] = {
        "bars": bars,
        "close": last.close if last else 0.0,
        "ema_fast": ta.ema(closes, cfg.ema_fast),
        "ema_slow": ta.ema(closes, cfg.ema_slow),
        "rsi": ta.rsi(closes, cfg.rsi_period),
        "atr": ta.atr(candles, cfg.atr_period),
        "macd_hist": ta.macd_hist(closes),
        "ret_1": ta.pct_change(closes, 1),
        "ret_5": ta.pct_change(closes, 5),
        "ret_15": ta.pct_change(closes, 15),
        "vol_ratio": 0.0,
        # 缺失写成 None 而不是 0：None 会被 prompt 的数值白名单过滤掉，
        # 模型看到的是"没有这一项"；写 0 则等于告诉模型"费率恰好为 0 / 持仓量没变"
        "funding": float(funding) if funding is not None else None,
        "open_interest": open_interest,
        "oi_change_pct": float(oi_change_pct) if oi_change_pct is not None else None,
        "change_24h": change_24h,
        "quote_volume": quote_volume,
    }

    # ---- 可用性判定：缺失写成缺失，不写中性 ----
    ok = {
        "trend": bars >= need["trend"],
        "momentum": bars >= need["momentum"],
        "volume": bars >= need["volume"],
        "funding": funding is not None,
        "oi": oi_change_pct is not None,
    }
    missing: list[str] = []
    if not ok["trend"]:
        missing.append(f"趋势:K线{bars}<{need['trend']}")
    if not ok["momentum"]:
        missing.append(f"动量:K线{bars}<{need['momentum']}")
    if not ok["volume"]:
        missing.append(f"量能:K线{bars}<{need['volume']}")
    if not ok["funding"]:
        missing.append("资金费率:未取到")
    if not ok["oi"]:
        missing.append("持仓量变化:仅单快照无法计算")
    # 均值回归需要足够长的窗口才能算标准差
    ok["meanrev"] = bars >= max(3, cfg.mr_window)
    if not ok["meanrev"]:
        missing.append(f"均值回归:K线{bars}<{cfg.mr_window}")
    f["ok"] = ok
    f["missing"] = missing

    avg_vol = sum(vols[-20:]) / max(1, len(vols[-20:])) if vols else 0.0
    f["vol_ratio"] = round(vols[-1] / avg_vol, 4) if avg_vol > 0 else 0.0
    price = f["close"] or 1.0
    f["atr_pct"] = round(f["atr"] / price * 100, 4)
    f["ema_gap_pct"] = round((f["ema_fast"] - f["ema_slow"]) / price * 100, 4)
    f["range_pct"] = round((last.high - last.low) / price * 100, 4) if last else 0.0
    # 布林标准分：均值回归模式的核心输入（偏离中轨多少个标准差）
    z, mid, sd = ta.zscore_last(closes, cfg.mr_window)
    f["band_z"] = round(z, 4)
    f["band_mid"] = round(mid, 8)
    f["band_sd_pct"] = round(sd / price * 100, 4)
    for key in ("close", "ema_fast", "ema_slow", "atr"):
        f[key] = round(f[key], 8)
    return f


def rsi_zone_score(rsi: float, ema_gap_pct: float) -> float:
    """按趋势体制解读 RSI，返回 [-1, 1]。

    静态 30/70 在单边趋势里必然失效：上涨趋势里 RSI 65 是"健康"而不是"超买"，
    而 RSI 80 才是"别追"。这里沿用"强势区间看健康、极值区间看拥挤"的分档，
    使得同一个 RSI 数值在趋势与震荡里给出相反的结论。
    """
    if rsi <= 0 or rsi >= 100:
        return 0.0
    if ema_gap_pct > 0:
        if rsi >= 78:
            return -0.6  # 多头过热：不追高，但不做空
        if rsi >= 72:
            return 0.2
        if rsi >= 52:
            return 0.6  # 多头健康区
        if rsi >= 40:
            return 0.9  # 顺势回踩买点
        if rsi >= 30:
            return 0.3
        return -0.2  # 深跌：动能已破，等结构修复
    if ema_gap_pct < 0:
        if rsi <= 22:
            return 0.6
        if rsi <= 28:
            return -0.2
        if rsi <= 48:
            return -0.6
        if rsi <= 60:
            return -0.9
        if rsi <= 70:
            return -0.3
        return 0.2
    # 震荡：回到经典超买超卖
    if rsi >= 68:
        return -0.7
    if rsi <= 32:
        return 0.7
    return ta.clamp((rsi - 50.0) / 30.0)


def entry_regime(features: dict[str, Any], cfg: StrategySettings) -> str:
    """入场时的体制：`chop` = 无趋势，`trend` = 顺势。

    用于把时间止损与保本门槛调紧：震荡市里 3 小时不突破就该释放资金，
    空耗到统一上限只会让资金占着不动。
    """
    if not features:
        return ""
    gap = abs(float(features.get("ema_gap_pct") or 0.0))
    return "chop" if gap < cfg.chop_ema_gap_pct else "trend"


def _pass_gates(res: SignalResult, features: dict[str, Any], cfg: StrategySettings, volume_ok: bool) -> bool:
    """两种模式共用的准入闸门。不过就写 blocked 并返回 False。"""
    if features["atr_pct"] < cfg.atr_pct_min:
        res.blocked = f"波动率过低 {features['atr_pct']:.3f}% < {cfg.atr_pct_min}%"
        return False
    if features["atr_pct"] > cfg.atr_pct_max:
        res.blocked = f"波动率过高 {features['atr_pct']:.3f}% > {cfg.atr_pct_max}%"
        return False
    if volume_ok and features["vol_ratio"] < cfg.volume_ratio_min:
        res.blocked = f"量能不足 {features['vol_ratio']:.2f} < {cfg.volume_ratio_min}"
        return False
    if features.get("quote_volume", 0) < cfg.min_24h_quote_volume:
        res.blocked = f"24h 成交额偏低 {features.get('quote_volume', 0) / 1e6:.1f}M"
        return False
    return True


def score_signal(
    symbol: str, features: dict[str, Any], cfg: StrategySettings
) -> SignalResult:
    """按 `cfg.mode` 选择信号模型。两者共用准入闸门与出场规则。"""
    if cfg.mode == "meanrev":
        return _score_meanrev(symbol, features, cfg)
    return _score_trend(symbol, features, cfg)


def _score_trend(
    symbol: str, features: dict[str, Any], cfg: StrategySettings
) -> SignalResult:
    res = SignalResult(symbol=symbol, features=features)
    price = features.get("close") or 0.0
    if price <= 0:
        res.blocked = "行情缺失"
        return res

    ok = features.get("ok") or {}
    missing = list(features.get("missing") or [])
    # 老特征字典没有 `ok` 字段时按"全部可用"处理，避免破坏历史调用方
    def avail(key: str) -> bool:
        return bool(ok.get(key, True))

    res.missing = missing

    # 趋势结构是唯一不可替代的因子：没有它就没有方向，只能观望
    if not avail("trend"):
        res.blocked = f"K线不足({features.get('bars', 0)}根)，无法确认趋势结构"
        return res

    trend = ta.tanh_norm(features["ema_gap_pct"], 0.22)

    if cfg.rsi_adaptive:
        rsi_term = rsi_zone_score(float(features["rsi"]), float(features["ema_gap_pct"]))
    else:
        rsi_term = ta.clamp((features["rsi"] - 50.0) / 50.0)
    momentum = ta.clamp(
        0.55 * ta.tanh_norm(features["ret_5"], 0.45)
        + 0.30 * rsi_term
        + 0.15 * ta.tanh_norm(features["macd_hist"] / price * 100, 0.25)
    )
    volume = ta.clamp((features["vol_ratio"] - 1.0) * 0.9) * (1 if features["ret_1"] >= 0 else -1)
    funding_s = ta.clamp(-(features.get("funding") or 0.0) / 0.0008)
    oi_s = ta.clamp((features.get("oi_change_pct") or 0.0) / 3.0)

    # ---- 只对可用因子加权，权重跟着一起归一 ----
    candidates = (
        ("trend", trend, cfg.weight_trend),
        ("momentum", momentum, cfg.weight_momentum),
        ("volume", volume, cfg.weight_volume),
        ("funding", funding_s, cfg.weight_funding),
        ("oi", oi_s, cfg.weight_oi),
    )
    active = [(name, v, w) for name, v, w in candidates if avail(name) and w > 0]
    total_w = sum(w for _, _, w in active) or 1.0
    score = sum(v * w for _, v, w in active) / total_w
    res.score = round(ta.clamp(score), 4)
    res.used_factors = [name for name, _, _ in active]

    res.reasons = [
        f"EMA 结构 {features['ema_gap_pct']:+.3f}%",
        f"RSI {features['rsi']:.1f}"
        + (f"→{rsi_term:+.2f}(体制自适应)" if cfg.rsi_adaptive else ""),
        f"1/5 分钟收益 {features['ret_1']:+.3f}% / {features['ret_5']:+.3f}%",
        f"量比 {features['vol_ratio']:.2f}",
        f"ATR {features['atr_pct']:.3f}%",
    ]
    if avail("funding"):
        res.reasons.append(f"资金费率 {features['funding'] * 100:+.4f}%")
    if avail("oi"):
        res.reasons.append(f"持仓量变化 {features['oi_change_pct']:+.2f}%")
    if missing:
        # 把"缺什么"写进 reasons，而不是让它以 0 分隐身
        res.reasons.append("缺失因子(未计入打分): " + "、".join(missing))

    # ---- 准入过滤 (可解释的拒绝原因)
    if not _pass_gates(res, features, cfg, avail("volume")):
        return res

    # ---- 止损/止盈：以 R 为单位，先定风险再定回报 ----
    atr_pct = features["atr_pct"]
    sl_pct = round(min(3.0, max(0.18, atr_pct * cfg.sl_atr_mult)), 4)
    tp_pct = round(min(8.0, max(atr_pct * cfg.tp_atr_mult, sl_pct * cfg.min_risk_reward)), 4)
    res.sl_pct = sl_pct
    res.tp_pct = tp_pct
    res.rr = round(tp_pct / sl_pct, 3) if sl_pct > 0 else 0.0
    # 止盈被上限截断后可能凑不够 R:R，此时宁可不开
    if res.rr + 1e-9 < cfg.min_risk_reward:
        res.blocked = (
            f"盈亏比不足 {res.rr:.2f}R < {cfg.min_risk_reward}R "
            f"(止损 {sl_pct}% / 止盈上限 8%, 波动过大时无法给出合规的回报空间)"
        )
        return res

    if res.score >= cfg.entry_threshold:
        res.direction = "long"
    elif res.score <= -cfg.entry_threshold:
        res.direction = "short"
    else:
        res.direction = "flat"

    # 结构确认: 趋势方向必须与分数一致, 避免逆势抄底
    if res.direction == "long" and features["ema_fast"] <= features["ema_slow"]:
        res.blocked = "多头信号缺少均线结构确认"
        res.direction = "flat"
    if res.direction == "short" and features["ema_fast"] >= features["ema_slow"]:
        res.blocked = "空头信号缺少均线结构确认"
        res.direction = "flat"
    return res


def _score_meanrev(
    symbol: str, features: dict[str, Any], cfg: StrategySettings
) -> SignalResult:
    """均值回归：价格偏离中轨到极值、且没有跟随量时，赌它回到中轨。

    为什么值得单独做一套：5m 级别的趋势跟随在加密永续上长期是负期望
    （实测 BTC/ETH/SOL 全周期为负，见 docs/STRATEGY_REVIEW.md），
    原因是大量时间在震荡。回归模型赚的正是那段震荡，而它的代价是
    胜率高但单笔 R 小 —— 所以用单独的更低的盈亏比底线，而不是趋势那套 2R。

    三条纪律：
    - |z| 不到 `mr_z_entry` 不做（偏离不够就没有回归空间）
    - 强趋势里不做逆势回归（顺势的极值通常继续走，不是回归）
    - 有跟随量（vol_ratio 明显放大）时降低置信度（放量突破不等于超买）
    """
    res = SignalResult(symbol=symbol, features=features)
    price = features.get("close") or 0.0
    if price <= 0:
        res.blocked = "行情缺失"
        return res

    ok = features.get("ok") or {}
    res.missing = list(features.get("missing") or [])

    def avail(key: str) -> bool:
        return bool(ok.get(key, True))

    if not avail("meanrev"):
        res.blocked = f"K线不足({features.get('bars', 0)}根)，无法计算回归窗口 {cfg.mr_window}"
        return res

    z = float(features.get("band_z") or 0.0)
    sd_pct = float(features.get("band_sd_pct") or 0.0)
    gap = float(features.get("ema_gap_pct") or 0.0)
    vol_ratio = float(features.get("vol_ratio") or 0.0)
    res.used_factors = ["meanrev", "volume"]

    res.reasons = [
        f"标准分 z={z:+.2f}（偏离中轨 {abs(z):.2f}σ）",
        f"回归窗口带宽 {sd_pct:.3f}%",
        f"EMA 间距 {gap:+.3f}%",
        f"量比 {vol_ratio:.2f}",
        f"RSI {features['rsi']:.1f}",
    ]
    if res.missing:
        res.reasons.append("缺失因子: " + "、".join(res.missing))

    if not _pass_gates(res, features, cfg, avail("volume")):
        return res

    # 强趋势里不做逆势回归
    if cfg.mr_block_in_trend and abs(gap) >= cfg.mr_trend_gap_pct:
        res.blocked = (
            f"处于趋势中(EMA 间距 {gap:+.3f}% ≥ {cfg.mr_trend_gap_pct}%)，"
            f"极值更可能是继续而非回归"
        )
        return res

    if abs(z) < cfg.mr_z_entry:
        res.blocked = f"偏离不足 |z|={abs(z):.2f} < {cfg.mr_z_entry}"
        return res
    if sd_pct <= 0:
        res.blocked = "带宽为 0（横盘无波动），无法定位回归空间"
        return res

    # 放量突破不算超买超卖：置信度打折，但不停用（有些放量确实是衰竭）
    follow_through = 1.0 if vol_ratio <= 1.2 else 0.6
    strength = ta.clamp(-ta.tanh_norm(z, cfg.mr_z_entry) * follow_through)
    res.score = round(strength, 4)
    res.direction = "long" if z < 0 else "short"

    # 止损放在极值之外，止盈回到中轨附近
    sl_pct = round(min(3.0, max(0.18, (abs(z) + 0.5) * sd_pct)), 4)
    tp_pct = round(min(8.0, max(abs(z) * sd_pct * 0.9, sl_pct * cfg.mr_min_risk_reward)), 4)
    res.sl_pct = sl_pct
    res.tp_pct = tp_pct
    res.rr = round(tp_pct / sl_pct, 3) if sl_pct > 0 else 0.0
    if res.rr + 1e-9 < cfg.mr_min_risk_reward:
        res.blocked = (
            f"盈亏比不足 {res.rr:.2f}R < {cfg.mr_min_risk_reward}R "
            f"(偏离 {abs(z):.2f}σ 时回到中轨的空间小于止损距离)"
        )
        res.direction = "flat"
        return res
    return res


def strategy_fingerprint(cfg: StrategySettings) -> str:
    """策略参数指纹（8 位十六进制）。

    写进每条信号与订单，事后复盘才能回答"这笔亏损是策略版本问题还是执行问题"。
    只取影响信号语义的参数，避免无关改动（如日志级别）让指纹乱跳。
    """
    payload = {key: getattr(cfg, key, None) for key in _FP_KEYS}
    blob = json.dumps(payload, sort_keys=True, default=str, ensure_ascii=False)
    return hashlib.sha256(blob.encode("utf-8")).hexdigest()[:8]


def partial_tp_r(side: str, entry_price: float, initial_sl: float, mark_price: float) -> float:
    """当前浮盈相当于多少个初始风险 (R).

    R = |开仓价 - 初始止损价|; 用**初始**止损而不是当前 sl_price,
    否则移动止损上移后 R 会失真, 部分止盈永远触发不了。
    """
    # initial_sl <= 0 表示该仓位没有记录初始止损 (老数据), 不能拿 0 当止损算出假 R
    if initial_sl <= 0 or entry_price <= 0:
        return 0.0
    risk = abs(entry_price - initial_sl)
    if risk <= 0:
        return 0.0
    direction = 1 if side == "long" else -1
    return (mark_price - entry_price) * direction / risk


def partial_close_qty(qty: float, ratio: float, qty_step: float, min_qty: float) -> float:
    """部分止盈的实际下单量: 按步进取整, 低于最小量则放弃.

    抽成纯函数是为了可测: 部分平仓最容易出错的地方就是"取整后为 0 / 低于最小下单量",
    此时必须返回 0 让调用方跳过, 而不是发一笔必然被交易所拒绝的单。
    """
    if qty <= 0 or ratio <= 0:
        return 0.0
    raw = qty * ratio
    step = qty_step if qty_step > 0 else 0.0
    if step:
        raw = math.floor(raw / step + 1e-12) * step
    raw = round(raw, 12)
    if raw <= 0 or raw < min_qty - 1e-12:
        return 0.0
    return raw


def roi_target(table: list[list[float]], minutes: float) -> float | None:
    """时间阶梯止盈目标 (freqtrade minimal_roi 思路).

    取"分钟数不超过当前持仓时长"的最高一档: 持仓越久, 要求的收益率越低,
    让 1 分钟级别的单子尽快落袋, 而不是拖到超时被行情反噬.
    """
    target: float | None = None
    for row in table or []:
        try:
            minutes_at, pct = float(row[0]), float(row[1])
        except (TypeError, ValueError, IndexError):
            continue
        if minutes >= minutes_at:
            target = pct
        else:
            break
    return target


def detect_exit(
    *,
    side: str,
    entry_price: float,
    mark_price: float,
    peak_price: float,
    sl_price: float,
    tp_price: float,
    opened_at: int,
    now_ts: int,
    cfg: StrategySettings,
    score: float,
    entry_regime: str = "",
) -> tuple[bool, str]:
    """本地出场判断: 硬止损 / 止盈 / 阶梯止盈 / 移动止损 / 信号反转 / 超时.

    `entry_regime` 传入入场时的体制（`chop` / `trend`）：
    震荡市的时间止损调紧，避免资金空耗在不会突破的仓位上。
    """
    direction = 1 if side == "long" else -1
    if sl_price > 0 and ((direction == 1 and mark_price <= sl_price) or (direction == -1 and mark_price >= sl_price)):
        return True, "stop_loss"
    if tp_price > 0 and ((direction == 1 and mark_price >= tp_price) or (direction == -1 and mark_price <= tp_price)):
        return True, "take_profit"
    # 时间阶梯止盈: 与固定 TP 并存, 先到者先平
    if entry_price > 0 and cfg.roi_table:
        profit_pct = (mark_price - entry_price) * direction / entry_price * 100
        target = roi_target(cfg.roi_table, max(0.0, (now_ts - opened_at) / 60.0))
        if target is not None and target > 0 and profit_pct >= target:
            # 静态收益率表 + 宽止损会组合出"0.4R 就落袋"。低于 roi_min_r 倍的
            # 风险收益不主动了结 —— 那是"赢小输大"，比不落袋更伤期望。
            risk_pct = (
                abs(entry_price - sl_price) / entry_price * 100
                if entry_price > 0 and sl_price > 0
                else 0.0
            )
            if cfg.roi_min_r <= 0 or risk_pct <= 0 or profit_pct + 1e-9 >= risk_pct * cfg.roi_min_r:
                return True, "roi"
    if cfg.trailing_pct > 0 and peak_price > 0:
        if direction == 1 and mark_price <= peak_price * (1 - cfg.trailing_pct / 100):
            return True, "trailing_stop"
        if direction == -1 and mark_price >= peak_price * (1 + cfg.trailing_pct / 100):
            return True, "trailing_stop"
    hold_limit = cfg.max_hold_minutes
    if entry_regime == "chop" and cfg.max_hold_minutes_chop > 0:
        hold_limit = min(hold_limit, cfg.max_hold_minutes_chop)
    if (now_ts - opened_at) >= hold_limit * 60:
        return True, "timeout"
    if direction == 1 and score <= -cfg.exit_threshold:
        return True, "signal_flip"
    if direction == -1 and score >= cfg.exit_threshold:
        return True, "signal_flip"
    return False, ""
