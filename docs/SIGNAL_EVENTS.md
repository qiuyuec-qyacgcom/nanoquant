# 外部信号事件契约

跟单入口 `POST /api/v1/signal-follow/ingest`（`X-Signal-Token` 鉴权）接受的每条记录，
按 `event` 字段决定执行什么动作。上游实现是 `F:\cb\telegram-signal-gateway`。

## 1. 事件类型

| `event` | 需要的字段 | 执行动作 | 落地状态 |
|---|---|---|---|
| `open` / 开仓 | `symbol` + `side` | 按「权益 × 单仓保证金% × 杠杆」开仓 | `opened` |
| `add` / 加仓 | `symbol`（`side` 可选，缺省按现有持仓） | 加仓并**合并**到已有仓位：数量累加、均价加权重算、按新均价重挂保护单 | `added` |
| `reduce` / 减仓 | `symbol` + `reduce_pct`（缺省 50） | 按比例部分平仓，剩余仓位保护单保留 | `reduced` |
| `close` / 平仓 | `symbol` | 全部平仓并撤掉保护单 | `closed` |
| `move_stop` / 移止损 | `symbol` + `sl` 或 `stop_to_breakeven=true` | 只改止损，不动数量；止盈一起重挂 | `stop_moved` |
| `exit`（旧口径） | — | 只记录、不执行（向后兼容） | `skipped` |

`add` / `reduce` / `close` / `move_stop` **都要求本地有该标的的 open 仓位**，
没有就记 `skipped`（原因含「持仓」），绝不会凭空下单。

## 2. 字段

```jsonc
{
  "id": "-1001234567890:4521",   // 幂等键，频道消息 id
  "symbol": "BTCUSDT",
  "side": "long",                // long/short；close/reduce/move_stop 可空
  "entry": 66200,                // open/add 的参考价，0 = 按市价
  "sl": 64500,                   // 止损；move_stop 时是目标价
  "tp": 69000,
  "leverage": 10,
  "event": "add",
  "reduce_pct": 0,               // reduce 用，0/缺省 = 50%
  "stop_to_breakeven": false,    // true = 止损移到本系统持仓均价
  "ts": 1730000000,
  "channel": "频道名",
  "message_link": "https://t.me/xxx/123"
}
```

## 3. 两条容易踩的坑

**`stop_to_breakeven` 的优先级高于 `sl`。**
频道写的"止损移到入场价 65000"里的 65000 是**频道作者的**入场价，
和你实际成交均价不是一个数。所以上游会把 `stop_to_breakeven` 置真、
`sl` 留空，由下游用本地 `PositionRecord.entry_price`。

**`平仓 50%` 与 `全部平仓` 只差一个词。**
分类器先认"全平的明确说法"，再按"带比例 = 减仓"兜底。
顺序反了会把半仓平光 —— `tests/test_contracts.py` 里有对应断言。

## 4. 安全护栏（执行前）

| 事件 | 护栏 |
|---|---|
| `close` / `reduce` | 不做开仓冷却、不占每日开仓额度（否则该平的时候平不掉） |
| `move_stop` | 新止损必须落在现价安全侧，否则挂上立即触发 → 拒绝 |
| `add` | 方向必须与现有持仓一致；加仓后若没有止损，用 `default_sl_pct` 合成一个 |
| 全部 | 币种黑白名单、引擎总开关、信号有效期 `max_age_sec`、`dry_run` |

`add` 与开仓共用标的锁（避免开仓未成交时把额度算重）；
`close` / `reduce` / `move_stop` **不受标的锁限制** —— 那个锁在开仓成功后会刻意保留 90 秒，
把离场动作挡在后面会导致"开仓后一分钟内的平仓信号被忽略"。

## 5. 演练模式

`dry_run=true` 时四类事件都只写流水（状态 `dry_run`），不碰交易所。
建议先用 `dry_run` 跑通一轮完整序列：开仓 → 加仓 → 减仓 → 移止损 → 平仓，
在「信号跟单」页核对每一步的数量、均价、止损是否符合频道原文。
