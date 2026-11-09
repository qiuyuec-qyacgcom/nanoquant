"""精度与最小下单约束.

小资金场景最容易死在 -1013 (min notional) 与 -1111 (精度) 上,
这里把"有效最小下单额"显式算出来, 让准入判断可解释.
"""
from __future__ import annotations

import math
from dataclasses import dataclass, field


def _decimals(step: float) -> int:
    if step <= 0:
        return 8
    text = f"{step:.12f}".rstrip("0")
    if "." not in text:
        return 0
    return len(text.split(".")[1])


def _to_step(value: float, step: float, n: int) -> float:
    """按 step 的小数位定格，减少浮点累积误差（落库/比较用）.

    注意：float 本身无法精确表示 0.1 的倍数，`float("12417.2")` 在 `:.12f` 下
    仍会显示 `12417.200000000001`。因此**发给交易所的最终字符串必须走
    `SymbolFilters.format_qty()`**，而不是 `:.12f`；这里只保证数值语义干净。
    """
    return float(f"{n * step:.{_decimals(step)}f}")


def floor_to_step(value: float, step: float) -> float:
    if step <= 0:
        return value
    return _to_step(value, step, math.floor(value / step + 1e-9))


def ceil_to_step(value: float, step: float) -> float:
    if step <= 0:
        return value
    return _to_step(value, step, math.ceil(value / step - 1e-9))


def round_to_tick(price: float, tick: float, *, mode: str = "nearest") -> float:
    if tick <= 0:
        return price
    if mode == "down":
        return floor_to_step(price, tick)
    if mode == "up":
        return ceil_to_step(price, tick)
    return _to_step(price, tick, round(price / tick))


@dataclass
class SymbolFilters:
    symbol: str
    price_tick: float = 0.0001
    qty_step: float = 0.001
    min_qty: float = 0.0
    max_qty: float = 1e12
    min_notional: float = 5.0
    max_leverage: int = 125
    last_price: float = 0.0
    quote_volume_24h: float = 0.0
    source: str = "live"
    extra: dict = field(default_factory=dict)

    # --------------------------------------------------------------- 计算
    def align_qty(self, qty: float, *, up: bool = False) -> float:
        return ceil_to_step(qty, self.qty_step) if up else floor_to_step(qty, self.qty_step)

    def align_price(self, price: float, *, up: bool = False) -> float:
        return ceil_to_step(price, self.price_tick) if up else floor_to_step(price, self.price_tick)

    def qty_decimals(self) -> int:
        return _decimals(self.qty_step)

    def price_decimals(self) -> int:
        return _decimals(self.price_tick)

    def format_qty(self, qty: float) -> str:
        """给交易所的 quantity 字符串：必须严格等于 qty_step 的小数位，不能用 :.12f.

        否则 `12417.200000000001` 这种浮点尾会变成 12 位小数，命中 -1111。
        """
        return f"{self.align_qty(qty):.{self.qty_decimals()}f}"

    def format_price(self, price: float) -> str:
        """给交易所的 price / triggerPrice 字符串，严格按 price_tick 的小数位."""
        return f"{self.align_price(price):.{self.price_decimals()}f}"

    def effective_min_qty(self, price: float | None = None) -> float:
        """按步进上取整后的真实最小可下单量."""
        px = price or self.last_price or 1.0
        min_by_notional = ceil_to_step(self.min_notional / px, self.qty_step) if px > 0 else 0.0
        return max(self.min_qty, min_by_notional, self.qty_step)

    def effective_min_notional(self, price: float | None = None) -> float:
        px = price or self.last_price or 1.0
        return round(self.effective_min_qty(px) * px, 8)

    def validate(self, qty: float, price: float) -> tuple[bool, str]:
        if qty <= 0:
            return False, "数量为 0"
        if qty < self.min_qty - 1e-12:
            return False, f"低于最小数量 {self.min_qty}"
        if qty > self.max_qty + 1e-9:
            return False, f"超过最大数量 {self.max_qty}"
        notional = qty * price
        if notional < self.min_notional - 1e-9:
            return False, f"名义额 {notional:.4f} USDT 低于交易所下限 {self.min_notional} USDT"
        return True, ""

    def as_dict(self) -> dict:
        return {
            "symbol": self.symbol,
            "price_tick": self.price_tick,
            "qty_step": self.qty_step,
            "min_qty": self.min_qty,
            "min_notional": self.min_notional,
            "max_leverage": self.max_leverage,
            "effective_min_qty": self.effective_min_qty(),
            "effective_min_notional": self.effective_min_notional(),
            "last_price": self.last_price,
            "source": self.source,
        }


def parse_exchange_info(info: dict) -> dict[str, SymbolFilters]:
    out: dict[str, SymbolFilters] = {}
    for item in info.get("symbols", []):
        if item.get("contractType") not in (None, "PERPETUAL"):
            continue
        if item.get("status") not in (None, "TRADING"):
            continue
        f = SymbolFilters(symbol=item["symbol"], source="live")
        for flt in item.get("filters", []):
            kind = flt.get("filterType")
            if kind == "PRICE_FILTER":
                f.price_tick = float(flt.get("tickSize", f.price_tick))
            elif kind == "LOT_SIZE":
                f.qty_step = float(flt.get("stepSize", f.qty_step))
                f.min_qty = float(flt.get("minQty", 0) or 0)
                f.max_qty = float(flt.get("maxQty", 1e12) or 1e12)
            elif kind == "MARKET_LOT_SIZE":
                step = float(flt.get("stepSize", 0) or 0)
                if step > 0:
                    f.qty_step = max(f.qty_step, step)
            elif kind == "MIN_NOTIONAL":
                f.min_notional = float(flt.get("notional", flt.get("minNotional", 5)) or 5)
        out[item["symbol"]] = f
    return out
