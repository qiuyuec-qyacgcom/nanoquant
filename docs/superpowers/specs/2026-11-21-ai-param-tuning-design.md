# AI 参数读写与动态调参设计

日期：2026-11-21
状态：已批准

## 意图

参数配置页目前把「不该展示」的字段以 `HIDDEN_BY_GROUP` 写死在前端，隐藏清单与后端参数脱节；同时 AI 虽然能在复核上下文里读到全部参数，却没有写回通道。目标是让参数可见性由后端注册表统一决定，并让 AI 能读写全部隐藏参数，形成「调参 → 观察交易结果 → 再评估」的自进化闭环。

## 目标

- 前端不再维护写死的隐藏字段清单，展示/隐藏由后端参数目录决定。
- AI 决策可携带参数修改，写回 `app_settings`，并受注册表范围约束。
- 默认开启 AI 调参（用户已确认）。
- 可写范围为全部隐藏参数，且允许任意方向调整（用户已确认）。
- 每次 AI 调用可调整全部隐藏参数，不设数量上限。
- 每次修改留审计：旧值、新值、原因、决策来源、时间。
- 最近调整及其后续交易效果注入下一轮 AI 上下文，支持 AI 自行回滚。
- 策略页提供只读的「AI 调参记录」，便于排查。

## 非目标

- 不开放敏感字段：API Key、Secret、密码、主密钥一律不可读入目录、不可写。
- 不允许 AI 修改参数注册表本身、AI 供应商配置、交易环境 `engine.mode`。
- 不做无审计的自由写库；所有写入必须经过校验函数。
- 不替换现有设置页的保存流程，人工修改仍走原接口。

## 反目标

- 调整不可追溯、不可回滚。
- 隐藏参数变成「没人知道改了什么」的黑盒。
- 调参绕过注册表边界把数值写到非法区间。

## 约束

- Rust 后端与 Vue 前端现有结构不变，新增模块而非重写。
- 参数目录以 `default_settings.json` 与实际读取的键为准，测试必须覆盖一致性。
- AI 输出解析失败时 fail-closed：不写任何参数。
- 应用参数必须与调用 AI 的轮次在同一事务或紧邻事务中完成，失败不产生半更新。

## 实现方案

考虑过三条路径：

- **A. 后端参数注册表 + AI 写入管道（选用）**：可统一可见性、边界与审计，满足自进化闭环。
- **B. AI 只建议、人工确认**：安全但不满足自动动态调参。
- **C. AI 自由写 `app_settings`**：不可审计、不可回滚，放弃。

### 1. 参数注册表

新增 `rust/nanoquant/src/param_registry.rs`：

```rust
pub(crate) struct ParamMeta {
    pub group: &'static str,
    pub key: &'static str,
    pub kind: &'static str,      // number / bool / string / json / array
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub visible: bool,
    pub ai_writable: bool,
}
```

- 覆盖 `engine`、`strategy`、`risk`、`data`、`news`、`evolution`、`journal`、`signal_follow`、`system`、`public`、`backup` 的可写字段。
- `security` 与任何键名含 `secret/password/token/api_key/_enc` 的字段永不进入目录。
- 全部隐藏参数 `ai_writable = true`，允许任意方向调整；仅受类型与 `min/max` 约束。
- 目录接口 `GET /settings/catalog` 返回：

```json
{
  "groups": {
    "strategy": {
      "hidden": ["ema_fast", "rsi_period"],
      "writable": ["ema_fast", "rsi_period", "entry_threshold"]
    }
  }
}
```

### 2. 前端动态可见性

- `settingsCatalog.ts` 删除 `HIDDEN_BY_GROUP`，保留 `SETTINGS_SECTIONS`（展示顺序/标题）、`PROTECTED`、`SKIP`（运行时安全字段）。
- 新增 `useParamCatalog` store：页面加载时请求 `/settings/catalog`，得到每组的 `hidden` 集合。
- `StrategyView.vue` 的 `skipOf(group)` 改为 `PROTECTED + SKIP + catalog.hidden[group]`。
- 目录请求失败时回落到「只显示 section 内显式字段」，不阻塞保存。

### 3. AI 决策 schema 扩展

`ai_client::Decision` 增加：

```rust
pub struct ParamUpdate {
    pub key: String,
    pub value: Value,
    pub reason: String,
}
pub params: Vec<ParamUpdate>,
```

- v2 短 JSON 增加可选 `"p": [{"k":"entry_threshold","v":0.35,"w":"..."}]`。
- v1 长 JSON 增加可选 `"params"` 同结构。
- 解析后逐项校验：字段存在、`ai_writable=true`、类型匹配、数值 clamp 到注册表范围；非法项丢弃并写入 `risk_flags`。
- 不限制单次数量，不限制同字段重复调整；全部合法项按顺序应用。

### 4. 写回与审计

新增表（迁移内创建）：

```sql
CREATE TABLE IF NOT EXISTS parameter_changes(
  id INTEGER PRIMARY KEY,
  ts INTEGER NOT NULL,
  group_name TEXT NOT NULL,
  key TEXT NOT NULL,
  old_value TEXT,
  new_value TEXT NOT NULL,
  reason TEXT NOT NULL,
  decision_id INTEGER DEFAULT 0,
  mode TEXT NOT NULL,
  source TEXT NOT NULL
);
```

- 写回函数 `apply_param_updates(tx, updates, decision_id, mode, now)`：
  1. 对每项从 `group_value` 取当前值；
  2. 校验并构造候选组；
  3. 调用现有 `validate(group, candidate)`；
  4. 写入 `app_settings`；
  5. 写入 `parameter_changes`；
  6. 写 `engine_events` 一条 `param_change` 事件。
- `source` 取值 `ai` / `rollback`。
- 单次多项时按字段分组批量写入，任一字段非法只跳过该字段，不阻断其他合法调整。

### 5. 自进化闭环

- `review_parameters` 增加 `parameter_changes`：最近 10 条，附调整后 5 笔已平仓交易的净盈亏合计（按 `mode` 与时间窗关联）。
- AI 系统提示新增：可调整字段列表、范围、最近调整效果；允许用新值覆盖旧值实现回滚。
- 策略页新增只读区块「AI 调参记录」：时间、字段、旧值→新值、原因、效果；提供单条回滚按钮。

### 6. 配置开关

- `ai` 组新增 `allow_param_tuning`，默认 `true`（用户已确认）。
- `false` 时 AI 上下文仍提供参数，但 `params` 会被整体忽略并记录 `risk_flags`。

## 数据流

1. 每轮 `collect_entries` 组装候选。
2. `EngineAiReviewer::review_entry` 调模型，解析 `Decision`（含 `params`）。
3. `allow_param_tuning` 与注册表校验通过后写回参数并审计。
4. 下一轮 `trade_policy` / 风控读取到新值，行为改变。
5. 后续交易平仓后，效果汇总进入下一轮 AI 上下文。
6. 前端读取 `/settings/catalog` 决定隐藏项，读取 `/settings/parameter-changes` 展示记录。

## 安全与回滚

- 敏感字段硬排除，不依赖键名黑名单之外的逻辑。
- 所有数值 clamp 到注册表范围；类型不符直接跳过并留痕。
- 每条变更可单独回滚；回滚也写审计。
- AI 调参失败只影响参数写入，不影响开仓决策本身。

## 分阶段实现

1. 注册表 + `/settings/catalog` + 前端动态隐藏（删除 `HIDDEN_BY_GROUP`）。
2. `Decision.params` 解析 + 校验 + 写回 + `parameter_changes` 审计。
3. 自进化上下文 + 效果汇总 + 前端调参记录/回滚。
4. 全量测试：注册表覆盖、越界拒绝、默认开启、前端过滤。

## 验收标准

- 前端源码不再出现写死的隐藏字段清单。
- `GET /settings/catalog` 对敏感字段返回空，对隐藏字段返回完整列表。
- AI 返回越界值时被 clamp 到范围并留痕；合法值时写回成功且可在 `parameter_changes` 查到。
- `allow_param_tuning=false` 时任何 `params` 都不写入。
- 最近调整记录与效果出现在 AI 上下文和策略页。
- `cargo fmt`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 全通过；前端 `typecheck`、`test`、`build` 通过。