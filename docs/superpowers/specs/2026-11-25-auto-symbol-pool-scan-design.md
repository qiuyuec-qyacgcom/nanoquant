# 自动标的池扫描设计

日期：2026-11-25
状态：已批准，待规格审查

## 1. 背景与目标

当前标的池固定为人工维护的 12 个合约。策略每轮只扫描池内标的，池子长期不变会错过量能突然放大的合约，也会保留量能持续萎缩的标的。

目标：新增独立后台任务，每 3 小时按 24h 成交额扫描前 300 个 USDT 永续合约，计算量能比并复用现有策略闸门，只替换「不达标且无持仓」的原池标的，并把池子补足到「每轮扫描标的数量」。

## 2. 非目标

- 不改变引擎主循环的交易周期与扫描逻辑。
- 不自动开仓；换池只影响下一轮引擎扫描的标的范围。
- 不新增独立的「目标池数量」参数，直接复用 `engine.max_symbols_per_cycle`。
- 不新增独立的最小量比参数，直接复用 `strategy.volume_ratio_min`。
- 不改动 AI 调参、回测或信号跟单链路。

## 3. 配置

新增参数分组 `symbol_scan`（参数页「自动换池」）：

| key | 类型 | 默认 | 范围 | 说明 |
|---|---|---|---|---|
| `enabled` | bool | `true` | - | 开启后按间隔自动扫描换池 |
| `interval_hours` | number | `3` | 1–24 | 扫描间隔（小时） |
| `market_limit` | number | `300` | 50–500 | 按 24h 成交额取前 N 个 USDT 永续 |

复用：

| 用途 | 参数 |
|---|---|
| 目标池大小 | `engine.max_symbols_per_cycle` |
| 最小量比 | `strategy.volume_ratio_min` |
| 扫描并发 | `engine.scan_concurrency` |
| ATR 区间 | `strategy.atr_pct_min` / `atr_pct_max` |
| 24h 成交额下限 | `strategy.min_24h_quote_volume` |
| 点差上限 | `strategy.max_spread_bps`（兼容旧 `spread_bps_max`） |

默认开启。首次启用且没有扫描历史时，启动后只记录当前时间，**3 小时后执行第一次自动扫描**，避免部署瞬间换池。若已有历史且距上次扫描超过间隔，启动后立即补扫一次。手动「立即扫描」不受间隔限制。

## 4. 扫描数据流

1. 读取当前交易环境（demo/live）对应的 `exchangeInfo` 域名。
2. 拉全市场 24h ticker 与 bookTicker。
3. 过滤：`contractType=PERPETUAL`、`status=TRADING`、`quoteAsset=USDT`。
4. 按 24h `quoteVolume` 降序取前 `market_limit` 个。
5. 对这批标的并发拉 K 线（并发数 `scan_concurrency`），用 `providers::candle_metrics` 计算：
   - `vol_ratio`：最新一根 K 线成交量 ÷ 最近 20 根平均成交量
   - `atr_pct`
6. 用 24h ticker 的 `quoteVolume` 和 bookTicker 的买卖价计算 `spread_bps`。
7. 组装 `scan::Candidate`，复用 `scan::risk_gate` 判断是否达标。

扫描任务与引擎主循环完全独立，不阻塞交易周期。单轮额外 API 权重约 650–700，按 3 小时摊薄约 3.7/分钟。

## 5. 原池检查与替换规则

### 5.1 原池判断

对当前 `trading_symbols` 中启用的标的逐个计算上述特征：

- `risk_gate` 全部通过 → 保留。
- 任一项不通过：
  - **有持仓**（当前 mode、`status='open'`）→ 保留，平仓后下一轮再判断。
  - **无持仓** → 加入待替换列表。

原池判断使用完整闸门（量能比 + ATR + 24h 成交额 + 点差），不再只看量能比。

### 5.2 候选

- 必须 `risk_gate` 全部通过。
- 必须不在当前池内。
- 按 `vol_ratio` 降序排列。

### 5.3 替换与补足

令：

- `target = engine.max_symbols_per_cycle`
- `replaceable = 原池中「不达标且无持仓」的数量`
- `slots = max(0, target - 原池保留数量)`

执行数量：

```text
replace_count = min(replaceable, 候选数量)
remaining     = max(0, 候选数量 - replace_count)
fill_count    = min(remaining, max(0, slots - replace_count))
```

- 先替换 `replace_count` 个待替换标的。
- 若原池仍不足 `target`，用剩余候选补足。
- 候选不足：只替换/补足有候选的部分，剩余不达标标的保留到下一轮。
- 原池数量超过 `target`：只替换不达标且无持仓的标的，不强行裁减合格标的或有持仓标的；池子允许暂时超过目标。
- 补足与替换都不删除有持仓的标的。

### 5.4 写入

在单个 SQLite 事务内：

1. 删除待替换标的（再次校验无持仓）。
2. 插入新标的，复用 `symbols::add_with_contracts` 的规则解析与写入（precision、tick、min_sz、min_notional、max_leverage、ct_val、note、sort_order）。
3. 写入扫描状态与事件。

任一步失败 → 回滚，保留原池，记录错误事件，下轮重试。

## 6. 代码结构

### 6.1 新模块 `symbol_scan.rs`

- 纯函数 `plan_replacements(current, scanned, gates, target) -> ScanPlan`：
  - 输入原池、扫描结果、闸门、目标数量。
  - 输出：保留、待替换、候选、最终新增、最终删除、原因。
  - 便于单元测试，不访问网络/数据库。
- async `execute(...)`：
  - 拉合约表、行情、K 线。
  - 调用 `plan_replacements`。
  - 事务落库。
  - 写 `engine_state.symbol_scan.last_run_ts` / `last_result`。
  - 写 `symbol_scan` 事件。

### 6.2 `providers.rs`

新增：

- 全市场 24h ticker 读取。
- 全市场 bookTicker 读取。
- 单标的 K 线读取（可与现有 snapshot 复用解析）。

### 6.3 `symbols.rs`

把 `add_with_contracts` 的规则解析与写入提取为 `pub(crate)` 函数，供手动添加和自动换池共用；保留现有持仓删除保护。

### 6.4 `runtime.rs`

新增独立后台任务 `symbol_scan_loop`：

- 每 60 秒检查 `enabled` 与 `last_run_ts`。
- 到达间隔时执行扫描。
- 不占用引擎 `tick` 的 `cycle_busy`。
- 与引擎写库通过 SQLite 事务/锁串行。

### 6.5 API

- `GET /api/v1/symbols`：响应增加 `scan` 字段（`last_run_ts`、`last_result`）。
- `POST /api/v1/symbols/auto-scan`：
  - 默认 `dry_run=true`：只返回扫描计划，不写库。
  - `apply=true`：执行替换并返回结果。
  - 用于标的池页「立即扫描」按钮。

## 7. 前端

### 7.1 参数页

新增分组「自动换池」：

- 自动换池开关
- 扫描间隔（小时）
- 扫描标的数量（前 N）

中文标签与说明，补进 `settingsCatalog`、`BASE_META` / `GROUP_META`、`default_settings.json`、`param_registry`。

### 7.2 标的池页

- 显示上次扫描时间与结果：扫描数、候选数、替换数、保留数。
- 新增「立即扫描」按钮：
  1. 先 `dry_run=true` 拉计划。
  2. 弹窗展示将替换/新增/保留的标的与原因。
  3. 用户确认后 `apply=true` 执行。
  4. 展示执行结果并刷新列表。

## 8. 失败处理

| 场景 | 处理 |
|---|---|
| exchangeInfo / ticker 拉取失败 | 保留原池，写 warn 事件，下轮重试 |
| 单个 K 线失败 | 跳过该标的；候选要求 `data_ok=true` |
| 候选不足 | 只替换有候选的部分，剩余保留 |
| 数据库写入失败 | 事务回滚，保留原池，写 error 事件 |
| 有持仓标的不达标 | 保留，平仓后下一轮再判断 |
| 扫描耗时过长 | 不阻塞引擎；下次检查跳过重叠执行（任务内 `busy` 标志） |

## 9. 测试

- `symbol_scan::plan_replacements`：
  - 原池全部通过 → 不替换。
  - 原池部分不达标且有候选 → 只替换对应数量。
  - 原池不达标但有持仓 → 保留。
  - 原池不足目标 → 补足到目标。
  - 候选不足 → 只替换有候选的部分。
  - 原池超过目标 → 不裁减合格标的。
  - 原池判断使用完整闸门（ATR/成交额/点差任一失败都进入待替换）。
- 配置：
  - `symbol_scan` 默认值、范围、前端 catalog 覆盖。
- API：
  - dry-run 不写库。
  - apply 写库并返回替换结果。
- 失败回滚：
  - 插入失败时原池不变。

## 10. 验收标准

- 默认开启；首次启用 3 小时后自动扫描一次。
- 扫描按 24h 成交额取前 300 个 USDT 永续。
- 候选同时满足量能比、ATR、24h 成交额、点差。
- 原池不达标且无持仓的标的被替换；有持仓的保留。
- 池子补足到 `engine.max_symbols_per_cycle`，候选不足时不强删。
- 「立即扫描」先展示计划，确认后再执行。
- 引擎主循环不被扫描阻塞，API 权重与耗时在可接受范围。