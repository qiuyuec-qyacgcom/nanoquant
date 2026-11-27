# 半 AI + 半量化改造复查：亏损是否已解决

> 复查时间：2026-11-27 09:06（北京时间）
> 复查范围：2026-11-26 23:00 → 2026-11-27 03:50 的窗口改动 + 之后 03:00→09:02 的 P0–P4 改造
> 线上实例：http://185.242.232.210:8899

---

## 一、总结论

**没有解决。** 亏损仍在以完全相同的机制继续发生，而且刚刚又发生了一笔。

| 维度 | 状态 |
|---|---|
| **L1 止损口径（病根）** | ❌ **完全未动**，`manage.rs` / `fast_risk.rs` 的保证金收益率 ÷ 杠杆口径原封不动 |
| **L2 杠杆由 AI 决定** | ✅ 代码已修（`[A5]` 本地规则 + `[P4]` 移除 AI 覆盖），**但线上未部署** |
| **L3 保证金放大回滚** | ✅ 已由 `[P4] 8633de5` 恢复量化波动缩放，**但线上未部署** |
| 线上实际跑的版本 | **11-27 01:22 启动**（uptime 27918s），即 P0–P4 全部未上线 |

---

## 二、新增证据：亏损仍在继续（id=22）

复查期间新增 1 笔成交，**机制与窗口内 4 笔亏损完全一致**：

| # | 标的 | 方向 | 开仓 | 平仓 | 杠杆 | 价格% | ROI% | 盈亏 | 平仓原因 |
|---|---|---|---|---|---|---|---|---|---|
| 22 | RIFUSDT | short | 11-27 06:23 | 06:44 | **9.0** | **−1.1115%** | **−10.0035%** | **−1.697** | global_stop_loss |

**ROI = −10.00%** —— 精准落在 `global_stop_loss_pct = 10` 这条线上。价格只跌了 **1.11%** 就被扫掉，而该标的当时的 **ATR = 3.5458%**。

**止损/ATR = 1.11% ÷ 3.55% = 0.31×ATR** —— 比窗口内那几笔（0.50–0.78×ATR）**更窄**。

> RIFUSDT 的信号记录（06:23 那轮 `dec=pending`）显示 AI 当时给的是 `ai_rr=2.0, ai_dq=0.92`，即**风控数据很好**，仍然被机械止损扫掉。

线上累计：`trades=22, wins=13, losses=9, win_rate=59.09%, pnl=0.3502`。
窗口内（5 笔）仍是 **1 胜 4 负、−7.63 USDT**；加上 id=22 后累计盈亏从 +2.05 掉到 **+0.35**。

---

## 三、逐项复查：P0–P4 改造命中了哪些根因

### ✅ 已修复（但未部署）：L2 杠杆失控

`[P4] f1fd7fb refactor(ai): 移除 AI 方向/杠杆/止损覆盖` —— 规模 495 行改动，`review.rs` 删了 445 行：

- `EntryProposal` 删掉 `max_leverage` / `margin_min_pct` / `margin_max_pct`
- `ReviewedEntry` 删掉 `margin_pct` 字段
- 测试断言改为 `"保证金必须来自量化参数，AI 不得改写"`
- `[P4] 2ee06bb` 进一步移除 `Decision` 里的杠杆/止损/止盈字段
- `[P4] 3c352ea` AI 复核固定启用，移除 `require_agreement`

`[A5] d84ba8d` 的 `local_leverage` 让杠杆由本地规则按 ATR 计算（`vol = (vol_target/atr_pct).clamp(0.5, 2.0)`）——低波动时反而**降**杠杆，方向正确。

### ✅ 已修复（但未部署）：L3 保证金放大

`[P4] 8633de5 fix: 恢复量化波动保证金缩放` 把保证金改回量化中枢 × ATR 缩放，AI 不再叠加自己的 margin_pct。

### ❌ 完全未动：L1 止损口径（病根）

这是**决定性事实**：

**1. `manage.rs` 的判定顺序自 11-24 起从未修改**

```git
$ git log -- rust/nanoquant/src/manage.rs
719087d 11-27 03:34 [D1] feat(strategy): 增加单品种均值回归
f383c22 11-24 14:21 fix(risk): 清理剩余价格口径并让权益与保护单实时刷新
```

最新那次（`719087d`）只加了均值回归的独立阈值，**趋势仓路径一字未动**：

```rust
// manage.rs —— 至今仍是全局止损第一、ATR 单仓止损第四
let margin_pct = price_pct * pos.leverage.max(1.0);
if stop_loss_pct > 0.0 && margin_pct <= -stop_loss_pct {
    return ManageAction::Close { reason: "global_stop_loss".to_string() };
}
...
if pos.sl_price > 0.0 && hit(direction, pos.mark_price, pos.sl_price, true) {  // 第 4 位
```

**2. `fast_risk.rs`（0.5 秒快平）同样是老口径**

```rust
// fast_risk.rs:76 fast_risk_reason()
let margin_pct = (input.mark - input.entry) * direction / input.entry * 100.0
    * input.leverage.max(1.0);          // ← 依然 ÷ 杠杆
if input.global_stop_pct > 0.0 && margin_pct <= -input.global_stop_pct {
    return Some("global_stop_loss");
}
```

**3. `atr_stop_mult` 已经彻底退化成「给 AI 看的约束」**

全仓库搜索 `atr_stop_mult`，它现在只剩三处用途，**没有一处参与引擎算止损**：

| 位置 | 用途 |
|---|---|
| `ai_analysis.rs:581` | 塞进 AI prompt 的 `constraints` |
| `ai_review.rs:251` | 塞进 AI 复核上下文的 `constraints` |
| `param_registry.rs:61` | 参数注册表（范围校验） |

并且 `ai_analysis.rs:791` 的测试断言写着：

```rust
assert!(!system.contains("constraints.atr_stop_mult"));
assert!(!system.contains("s ≥ constraints.atr_stop_mult"));
```

**即：`[P4] f1fd7fb` 主动把「止损必须 ≥ 1.3×ATR」这条规则从 AI prompt 里删掉了。** 而引擎自己又从不算 ATR 止损 —— 于是 **1.3×ATR 这条约束在整条链路上彻底消失**。

**4. 开仓时 `stop_price` 的默认值仍是 ÷ 杠杆**

```rust
// runtime.rs:3127
let stop_price = if global_stop > 0.0 {
    price * (1.0 - direction * global_stop / leverage / 100.0)   // 10% ÷ 9 = 1.11%
} else { 0.0 };
```

**5. 全仓库没有「止损距离 / ATR」的下限约束**

搜索 `min_stop` / `stop_distance` / ATR 下限 —— 无任何实现。`atr_pct_min`/`atr_pct_max` 是**入场闸门**（筛标的），不是**止损下限**（防扫损），两者不能互相替代。

---

## 四、线上还在跑旧版本（部署滞后）

**服务启动时间：11-27 01:22**（uptime 27918s）。

而 P0–P4 全部提交在 05:00 之后：

```
51378fe 11-27 05:49 [P0] fix: 半 AI 指标按模式隔离并对齐否决率口径
f631ac5 11-27 05:46 [P0] feat(stats): 半 AI 效果指标
f1fd7fb 11-27 08:12 [P4] refactor(ai): 移除 AI 方向/杠杆/止损覆盖
8633de5 11-27 08:16 [P4] fix: 恢复量化波动保证金缩放
2ee06bb 11-27 08:38 [P4] refactor(ai): 移除 Decision 杠杆/止损/止盈字段
d1952d0 11-27 09:02 [P4] chore: 清理旧 AI 配置与参数建议残留
```

**独立佐证**：线上 `/api/v1/settings/catalog` 探测结果 ——

| 键 | 仓库中首次出现 | 线上 catalog |
|---|---|---|
| `mr_enabled` | `4d10aa0` 03:36 | **False** |
| `local_leverage_base` | `d84ba8d` 02:44 | **False** |
| `veto_only` | `0370088` 02:37 | **False** |
| `use_invalidation_price` | `c50f6e3` 22:28（前一日） | True |

→ 线上二进制是 **01:22 之前**的构建，`[A5]`/`[P4]` 全部未上线。这也解释了为什么 id=22 仍是 lev=9（旧版由 AI 下发 `l=9`）。

部署机制：`scripts/update.sh` 是**手动**执行（`sudo bash scripts/update.sh`），`ci.yml` 只做测试构建，**没有自动部署**。所以 03:50 之后这一大轮改动至今未生效。

---

## 五、其它值得注意的观察

### 5.1 `reviewed=False` 仍是老问题，且又多了一笔

id=20、id=21、id=22 三笔 `reviewed=False`。原因仍是 `execution.rs` 落库时 `reviewed` 写死常量 0 —— P0–P4 没有修这一条。

### 5.2 AI 供应商故障率显著改善

11-26 23:46 → 11-27 00:29 曾出现**连续 100+ 次 `HTTP 503 no_account`**；11-27 06:00 之后 **227 次调用仅 18 次错误**。`[P2] 6dc06be 备用域名 failover` 与 `[P3] d5567e4 按 p95 排序供应商并熔断失败` 起了作用 —— 这条修复是有效的。

### 5.3 RIFUSDT 这笔的信号质量反而很好

06:23 那轮 `ai_rr=2.0, ai_dq=0.92`，AI 理由明确说了「2×ATR 空间未验证，建议观望」——但 AI **没有否决权以外的手段**：它只能否决，不能把止损放宽。于是这笔在「AI 认为空间不足」的情况下仍被开出，然后被 1.11% 的止损扫掉。

这恰好暴露了 P4 的一个副作用：`f1fd7fb` 移除 AI 止损覆盖后，**AI 再也无法为「空间不足」的高质量信号放宽止损**，而量化侧又没有 ATR 止损兜底。**AI 的判断被彻底丢弃了。**

---

## 六、下一步建议

### 立即（不需要发版，线上 API 直接改）

| 动作 | 接口 | 理由 |
|---|---|---|
| `risk.max_leverage` 16 → **6** | `PUT /api/v1/settings/risk` | 杠杆 6x 时价格止损 = 10/6 = 1.67%，把止损/ATR 从 0.31 拉回 0.47–0.9 |
| `risk.global_stop_loss_pct` 10 → **25** | 同上 | 杠杆 6x 时价格止损 = 25/6 = 4.17%，≈1.2×ATR，真正脱离噪音区 |
| `risk.min_leverage` 5 → 5 | 保持 | |

两者必须**同时**改：只提 `global_stop_loss_pct` 而不动杠杆，止损/ATR 仍是 10/16=0.625×ATR；只降杠杆而不提阈值，止损/ATR = 10/6=1.67% 但亏损金额被放大 2.7 倍（margin_pct=20 未变）。

> ⚠️ 这两个参数改动是**收紧风险**，会降低单笔盈利幅度。请在下一个交易窗口生效、观察 5–10 笔后再评估，不要用单笔结果下结论。

### 短期（需发版，优先级高于 P4 其余部分）

**P0-新：给止损加 ATR 下限** —— 这是唯一能根治 L1 的改动，且**不能等**：

```rust
// manage.rs 与 fast_risk.rs 都要改
let atr_floor = atr_pct * cfg.atr_stop_mult;   // 至少 1.3×ATR 的价格距离
let roi_floor = atr_floor * leverage;            // 换算回保证金收益率口径
let effective_stop = stop_loss_pct.max(roi_floor);
```

配套：`positions` 需要存 `atr_pct` 快照（`entry_terms` 里已有 `features`，可直接取 `features.atr_pct`）。

**同时把 `f1fd7fb` 删掉的 ATR 约束加回量化侧**：`ai_analysis.rs:791` 那两条断言说明 prompt 里的 `s ≥ atr_stop_mult × atr_pct × l` 被移除是有意的 —— 但**移错了地方**。正确做法不是从 prompt 删掉，而是把这条规则实现在**引擎**里（因为引擎才是执行止损的一方）。

### 中期

1. **部署 P0–P4**（手动 `sudo bash scripts/update.sh`），并加一条 CI 自动部署或至少加个部署提醒 —— 这次 8 小时的修复滞后本身就是事故来源之一。
2. **修 `execution.rs` 的 `reviewed` 常量**（P0–P4 未覆盖），让开仓的 AI 结论可追溯。
3. **给 GA 证据门加「止损距离/ATR」维度**（`[D3]` 尚未提交）—— 否则 GA 仍用保证金收益率口径衡量，永远看不到「价格止损被杠杆压缩」这个问题。

---

## 七、一句话结论

**这轮改造把「AI 乱改杠杆」治好了（治标），但把「AI 算的 ATR 止损」也一起删了（治反了），而引擎自己的止损口径分毫未动 —— 结果是止损距离只剩 0.31×ATR 的噪音区，今天早上 06:23 又照旧扫掉一笔 RIFUSDT。**

**且这 8 小时的修复（05:00–09:02）根本没上线：线上跑的还是 01:22 的旧二进制。**
