# 杠杆由 AI 在全局区间内决定

## 意图与背景

风控页原先有三项杠杆配置：`default_leverage`、`min_leverage`、`max_leverage`。其中默认杠杆既是
开仓的目标值，也是保证金上限（`权益 × margin_pct × leverage`）的基准，导致即使 AI 参与了复核，
它也只能在默认值之下下调杠杆。需求是去掉「默认杠杆」，全局只保留上下限，开仓杠杆改由 AI
结合波动率、置信度与止损距离在区间内给出。

## 行为

- 风控分组只保留 `min_leverage`（下限）与 `max_leverage`（上限）。
- 开仓杠杆允许区间：`[min_leverage, min(max_leverage, 交易所上限, 100)]`。
- AI 可以上调也可以下调，越界值由主机钳回区间；不再保留「AI 只能降杠杆」的限制。
- AI 未给杠杆（AI 复核未启用、返回 `l<=0`、或回测）时，回退到 `min_leverage`。
- 杠杆只影响保证金上限与可用余额约束，单笔风险预算仍由 `risk_per_trade_pct × 止损距离` 决定。
- 老库里的 `default_leverage` 在 `schema::migrate` 时从 `app_settings.risk` 中移除，
  避免读路径把已失效的旧值合并回前端。

## 实现要点

- `review::EntryProposal` 新增 `max_leverage`；`leverage` 语义改为「下限 / 回退值」，
  `apply_review` 用 `clamp_leverage` 把 AI 值钳到 `[leverage, max_leverage]`。
- `runtime` 生成开仓草案时以 `min_leverage` 作为基线，并把逐标的的 `upper` 一并传入。
- `ai_review::decision_to_review` 接受区间内的任意杠杆，并把「收紧止损」也计入触发
  `Tighten` 的条件，避免 AI 只收紧止损时被当成 `Agree` 丢弃。
- 回测固定使用 `min_leverage`，与实盘回退规则一致。
- 引擎自检以 `min_leverage` 估算仓位，并在文案中给出下限–上限的名义额区间。

## 非目标

- 不改变单笔风险预算、敞口上限与止损规则。
- 不为无 AI 的场景重新引入隐藏默认杠杆。
- 不调整跟单（`signal_follow`）的杠杆加成逻辑，仍受同一上限钳制。

## 验证

- Rust：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test`（494 通过）。
- 前端：`vue-tsc --noEmit`、`vitest run`（39 通过）。
- 新增测试覆盖：区间上下限钳制、AI 上调杠杆、缺失时回退下限、旧库字段清理。
