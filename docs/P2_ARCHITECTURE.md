# P2 架构级优化：落地方案

> 状态：**设计完成，待实施**。这几项是架构级改动，直接往正在跑的实盘里塞半成品
> 会造成状态错乱，所以先沉淀成可执行的设计，等 P0/P1 验证通过后再单独迭代。
> 每项都写清了"为什么、改哪里、怎么落地、怎么验收"。

---

## P2-8 信号生成与组合层分离

**现状**：`score_signal` 产出方向 → 直接进 `symbol_gates` → 下单。选币、仓位、
风控全挤在一条链上。加一个策略/一个资金约束都要动这条链。

**目标**：三阶段流水线，每层可独立测试、可插拔。

```
打分(score_signal) → 候选池(rank) → 组合层(select + size) → 下单
   纯函数、无状态      按分数/流动性排序   相关性/边际风险/资金预算
```

**改哪里**：
- 新增 `nanoquant/portfolio.py`：`select(candidates, positions, account, risk_cfg) -> list[Allocation]`。
  输入候选信号 + 当前持仓 + 账户，输出"要不要开、开多大"，把 `symbol_gates` 里的
  敞口/同向/相关性判断挪到这里，做成纯函数。
- `worker._scan` 只负责打分并交给组合层，不再逐标的直接下单。

**验收**：组合层单测——给定 5 个候选 + 3 个已有山寨多仓，能正确拒绝第 4 个山寨
（相关性/敞口超限），且决策可解释（每个候选带 accept/reject 原因）。

---

## P2-11 分批出场状态机

**现状**：出场是隐式组合——`partial_tp_at_r` 平一次 + `trailing` 扫尾 + `roi` 时间阶梯，
互相之间没有显式状态，改一个参数常会意外牵动另一个（模拟盘 24 笔 trailing 全亏的
深层原因之一就是这些出场规则没有统一的"已发生什么"状态）。

**目标**：每个仓位一个显式状态机：

```
OPEN → (浮盈≥1R) → PARTIAL_1(平X%) → (≥2R) → PARTIAL_2(平Y%) → TRAILING → CLOSED
                 ↘ 保本上移(止损=入场)          ↘ 时间止损
```

**改哪里**：
- `PositionRecord` 已有 `partial_taken_at`，扩展为 `exit_stage`（枚举）字段。
- 新增 `nanoquant/engine/exit_fsm.py`：`next(stage, ctx) -> (action, new_stage)`，
  把 `detect_exit` 的多个 if 分支重构成状态转移表，`detect_exit` 变成薄封装。
- 回测引擎复用同一个 FSM，杜绝"回测一套出场、实盘另一套"。

**验收**：FSM 单测覆盖每个状态转移；回测与实盘用同一 FSM 函数（代码级断言）。

---

## P2-20 订单状态机（OMS）

**现状**：下单→成交/部分成交/撤单→挂保护单的逻辑散在 `broker.py` 的 if/else 里，
靠 `_CLOSING` 集合 + `db_writer` 异步落库兜底。单标的单线程够用，一旦加
"限价转市价"、"部分成交补单"、"撤单重试"，散逻辑必然出状态错乱。

**目标**：显式订单生命周期状态机 + 每笔订单可追溯。

```
PENDING → SUBMITTED → FILLED / PARTIALLY_FILLED / REJECTED / EXPIRED
        ↘ CANCELING → CANCELED
```

**改哪里**：
- 新增 `nanoquant/exchange/oms.py`：`OrderState` + `apply_event(order, event) -> OrderState`
  纯函数；`BinanceBroker` 的 `open_position/close_position/amend_stop` 变成"发单 + 轮询
  状态机推进"，而不是散落的 try/except。
- 关键不变式：任何状态的最终落点都写 `orders.status`，供 `_reconcile_live` 对账。

**验收**：用桩交易所模拟"部分成交后价格反向"、"撤单被拒"、"查询超时"三类异常，
断言订单状态机落在正确终态、不产生孤儿单。

---

## P2-22 告警分级与 runbook

**现状**：`alerts.py` 有分级 + digest + 静默时段，但每条告警是"通知"，没有
"该做什么"的绑定。出问题靠人翻代码。

**目标**：告警 = 事件 + 级别 + **runbook 链接**，可执行、可自动恢复的才自动。

**改哪里**：
- 新增 `nanoquant/alerts_runbook.py`：`RUNBOOK = {category: {"level", "action", "auto_recover"}}`。
  例如 `health.ws_disconnect` → level=warn、action="检查网络/代理，行情已降级为 REST"、
  auto_recover=True（WS 重连自动解除）。
- `alerts.send` 在消息里带上 runbook 摘要；面板加"告警"页按 runbook 归类。

**验收**：每条 `events.record` 的 category 都能在 RUNBOOK 里查到（测试遍历代码里的
所有 category，缺 runbook 的告警会报错）。

---

## P2-4 盘口不平衡因子（已部分落地）

`book_tickers` 已用于点差闸门；下一步把 bid/ask 量比作为一个因子接进 `build_features`
的量能维度。**依赖 P2-8 组合层就绪后再加**（否则又是往打分里堆一个未验证的因子）。

## P2-14 资金费计入成本模型

`run_backtest` 已有 `funding_bps_8h`；缺的是**实盘 pre-trade 预估**——开仓前算
"这笔预计跨几个结算点 × 当前费率"，低于阈值不开。等 P2-8 的组合层就绪后，放进
`Allocation` 的成本项。

## P2-17 防未来函数审查

已修掉主要泄漏：`_closed_only` 剔除未收盘 K 线。剩余要做的是**系统性断言**：
新增一个测试，对回测引擎的每根 bar 断言"成交价只用 t 之后的 bar、信号只用 t 之前的 bar"。
这是测试级工作，可在任意时候补，不依赖其它 P2。

---

## 实施顺序建议

1. P2-17（半天，独立、纯测试）
2. P2-11 出场 FSM（先做，因为它直接消除"出场规则互相打架"这一类 bug）
3. P2-8 组合层（再做，是 P2-4/P2-14 的地基）
4. P2-20 OMS（最后，风险最高，需在 demo 里长时间跑稳再上 live）
5. P2-4 / P2-14 / P2-22 挂在对应地基之后
