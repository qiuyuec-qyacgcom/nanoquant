# 旧项目 → 新项目参数迁移表

> 生成时间：2026-11-28 ｜ 旧实例：http://185.242.232.210:8899（v0.1.0）｜ 新项目：当前仓库 main 分支

> 迁移原则：只列新项目参数页**当前可见**的字段；已隐藏的 AI 托管字段、敏感凭证、旧版已删除字段不进入参数表。
> 共 **106** 项。杠杆按要求固定 5–16x，每仓保证金 20%。

## 一、旧项目运行与数据

- 交易环境：live（实盘 · 币安主网）；引擎运行状态：未开启；轮询 600 秒
- 账户权益：112.0244 USDT；可用 112.0244；持仓 0 个
- 已平仓：22 笔；胜 13 / 负 9；胜率 59.09%；累计盈亏 0.35 USDT；盈亏比 1.02；平均持仓 7920 秒
- 方向分布：做多 7 笔，做空 15 笔
- 盈利前五：SOXXUSDT +5.55、ASMLUSDT +2.84、MOODENGUSDT +1.77、BIGTIMEUSDT +1.46、BLURUSDT +1.22
- 亏损前五：XAUUSDT -4.85、CFGUSDT -3.09、BEAMXUSDT -2.19、AIOTUSDT -2.09、RIFUSDT -1.7
- 离场原因：

| 离场原因 | 笔数 | 盈亏（USDT） |
|---|---:|---:|
| global_stop_loss | 6 | -10.19 |
| global_take_profit | 2 | 5.26 |
| manual | 2 | -2.17 |
| roi_target | 6 | 6.62 |
| venue_sync | 6 | 0.83 |

### 开平仓记录

| id | 标的 | 方向 | 杠杆 | 开仓时间 | 平仓时间 | ROI% | 盈亏 USDT | 离场原因 |
|---:|---|---|---:|---|---|---:|---:|---|
| 1 | LISTAUSDT | long | 5x | 2026-11-24 01:26 | 2026-11-24 04:41 | 10.04 | 0.10854 | venue_sync |
| 2 | CYSUSDT | short | 5x | 2026-11-24 04:42 | 2026-11-24 05:05 | 12.0621 | 0.269997 | roi_target |
| 3 | AIOTUSDT | short | 5x | 2026-11-24 05:17 | 2026-11-24 14:45 | -98.9952 | -2.08605112 | venue_sync |
| 4 | BLURUSDT | long | 5x | 2026-11-25 02:02 | 2026-11-25 02:59 | 14.7361 | 1.13559552 | venue_sync |
| 5 | AMATUSDT | short | 5x | 2026-11-25 01:01 | 2026-11-25 03:01 | 12.0412 | 1.0091 | roi_target |
| 6 | BIGTIMEUSDT | long | 5x | 2026-11-25 04:03 | 2026-11-25 04:08 | 29.2569 | 1.45710784 | venue_sync |
| 7 | SOXXUSDT | short | 5x | 2026-11-25 00:48 | 2026-11-25 07:03 | 11.7977 | 1.08780001 | roi_target |
| 8 | GIGADEVUSDT | short | 5x | 2026-11-25 06:07 | 2026-11-25 07:15 | 15.0087 | 1.71220005 | roi_target |
| 9 | ASMLUSDT | short | 5x | 2026-11-25 03:14 | 2026-11-25 07:41 | 10.0099 | 1.5824 | roi_target |
| 10 | GIGADEVUSDT | short | 5x | 2026-11-25 07:28 | 2026-11-25 08:35 | -12.9721 | -1.4892 | global_stop_loss |
| 11 | ASMLUSDT | short | 5x | 2026-11-25 08:08 | 2026-11-25 08:35 | -11.3734 | -2.0426 | global_stop_loss |
| 12 | SOXXUSDT | short | 6x | 2026-11-25 08:21 | 2026-11-25 12:23 | 6.9167 | 0.96140002 | roi_target |
| 13 | SOXXUSDT | short | 9x | 2026-11-25 13:27 | 2026-11-25 14:37 | 25.0126 | 3.49800004 | global_take_profit |
| 14 | ASMLUSDT | short | 6x | 2026-11-25 09:01 | 2026-11-25 14:38 | 19.054 | 3.30455394 | venue_sync |
| 15 | MOODENGUSDT | long | 7x | 2026-11-25 14:37 | 2026-11-25 15:32 | 40.3743 | 1.76519 | global_take_profit |
| 16 | XAUUSDT | short | 9x | 2026-11-25 14:52 | 2026-11-25 17:46 | -12.2502 | -2.5949 | global_stop_loss |
| 17 | BLURUSDT | long | 5x | 2026-11-25 17:46 | 2026-11-25 18:05 | 7.6783 | 0.08070286 | manual |
| 18 | XAUUSDT | short | 9x | 2026-11-25 18:11 | 2026-11-25 20:16 | -10.8057 | -2.25038 | manual |
| 19 | CFGUSDT | long | 8x | 2026-11-25 21:01 | 2026-11-25 21:11 | -16.3304 | -3.0924683 | venue_sync |
| 20 | BEAMXUSDT | short | 7x | 2026-11-26 08:28 | 2026-11-26 09:44 | -10.5344 | -2.1859266 | global_stop_loss |
| 21 | 2ZUSDT | long | 9x | 2026-11-26 15:38 | 2026-11-26 15:40 | -10.084 | -0.1842 | global_stop_loss |
| 22 | RIFUSDT | short | 9x | 2026-11-26 22:23 | 2026-11-26 22:44 | -10.0035 | -1.69670772 | global_stop_loss |

### 心法

- **方向与结构描述矛盾时禁止入场**（置信度 0.62，健康度 0.55，样本 1）：当 ai_review 的结构描述（如 EMA 多头排列）与 quant_side 或实际下单方向相反时，禁止入场，除非出现与该方向一致的明确结构确认。

### 全局策略

- **熬鹰策略v4**（优先级 10，启用 是）：全池 · 不限方向
  - 注意：旧策略正文要求 AI 决定方向、杠杆和止损，新项目已改为半 AI，不能原样导入；需改写为“只否决/降仓”的规则。

### 标的池与 AI 调参

- 标的池 12 个：SKYUSDT、BANKUSDT、THEUSDT、GUAUSDT、WLFIUSDT、FFUSDT、STXUSDT、ERAUSDT、XPINUSDT、ROBOUSDT、ACEUSDT、BMTUSDT
- 标的池统一参数：max_leverage 16x；sl_atr_mult 2.2；min_notional 5
- AI 调参记录：candidate=2，rejected=2，retired=1，active=2
- 决策经验：0 条

## 二、迁移原则

- 旧项目已有的同名参数：原则上沿用；新增字段取新项目默认值。
- 强制值：`risk.min_leverage=5`、`risk.max_leverage=16`、`risk.margin_pct=20`。
- 新项目策略页只保留结构开关；入场阈值、ATR、止盈止损等 20 个数值字段已隐藏并交给 AI 调参，因此不在本表。
- 旧项目的 AI 全权改方向/杠杆/止损开关、`signal_follow` 组、`ai_tuning.auto_apply` 等已被新版本移除，不迁移。

## 三、新项目参数表


### 交易执行（6 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 轮询间隔（秒） | `cycle_interval_sec` | 600 | 600 | 沿用旧值 600 秒；旧实例实际约 10 分钟一轮 |
| 最大同时持仓（个） | `max_positions` | 3 | 3 | 沿用旧项目 |
| 交易时段 | `trading_windows` | 00:00-24:00 | 00:00-24:00 | 沿用旧项目 |
| 时区偏移（小时） | `timezone_offset` | 8 | 8 | 沿用旧项目 |
| 下单类型 | `order_type` | market | market | 沿用旧项目 |
| 熔断时平仓 | `close_on_halt` | 关闭 | 关闭 | 沿用旧项目 |

### 策略（6 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| K 线周期 | `timeframe` | 1h | 1h | 沿用旧项目 |
| 高周期过滤 | `higher_timeframe` | — | 空 | 旧项目没有高周期过滤，留空关闭 |
| 信号有效期（根 K 线） | `valid_bars` | 6 | 6 | 沿用旧项目 |
| 最低开仓盈亏比（R） | `min_entry_rr` | 0.8 | 0.8 | 沿用旧项目 |
| 时间阶梯止盈 | `roi_table` | 空（关闭） | 空（关闭） | 沿用旧项目空表，关闭时间阶梯止盈 |
| 均值回归 | `mr_enabled` | — | 关闭 | 旧项目没有均值回归；为保证行为一致设关闭，需要时手动开启 |

### AI 复核（14 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 启用 AI 失效价平仓 | `use_invalidation_price` | 关闭 | 关闭 | 沿用旧值关闭 |
| 启用 AI 建议限价 | `use_entry_price` | 关闭 | 关闭 | 沿用旧值关闭 |
| 单次复核超时（秒） | `review_timeout_sec` | — | 30 | 新半 AI 复核字段，取新项目默认 30 秒 |
| 每轮复核预算（秒） | `review_budget_sec` | — | 20 | 新半 AI 复核字段，取新项目默认 20 秒 |
| 按置信度降仓 | `scale_on_confidence` | — | 开启 | 新半 AI 复核字段，取新项目默认开启 |
| AI 不可用时仓位（×） | `fail_open_scale` | — | 0.5 | 新半 AI 复核字段，取新项目默认 0.5 倍仓 |
| 高分免复核线 | `high_score_threshold` | — | 0.75 | 新半 AI 评分分流字段，取默认 0.75 |
| 低分直接拒绝线 | `low_score_threshold` | — | 0.45 | 新半 AI 评分分流字段，取默认 0.45 |
| 复核结果缓存（秒） | `review_cache_ttl_sec` | — | 60 | 新半 AI 复核缓存字段，取默认 60 秒 |
| 复核并发（路） | `review_concurrency` | — | 8 | 新半 AI 复核并发字段，取默认 8 路 |
| 供应商排序 | `provider_order_mode` | — | fastest | 新半 AI 供应商排序字段，取默认按延迟 |
| 连续失败熔断阈值（次） | `circuit_breaker_failures` | — | 3 | 新半 AI 熔断字段，取默认 3 次 |
| 熔断冷却时长（秒） | `circuit_breaker_sec` | — | 60 | 新半 AI 熔断字段，取默认 60 秒 |
| 提示词档位 | `prompt_profile` | — | full | 新半 AI 提示词档位，取默认完整上下文 |

### AI 调参（10 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 启用 AI 调参 | `enabled` | 开启 | 开启 | 沿用旧值开启；新版本证据门通过后自动激活 |
| regime 确认时长（秒） | `regime_confirm_sec` | 1800 | 1800 | 沿用旧项目 |
| regime 冷却（秒） | `regime_cooldown_sec` | 1800 | 1800 | 沿用旧项目 |
| 无候选回滚时长（秒） | `no_candidate_rollback_sec` | 1800 | 1800 | 沿用旧项目 |
| 样本外成交下限（笔） | `min_oos_trades` | 30 | 30 | 沿用旧项目 |
| 启用遗传算法搜索 | `ga_enabled` | — | 关闭 | 旧项目没有 GA 搜索，取新默认并关闭 |
| 遗传算法种群规模（个） | `ga_population` | — | 30 | 旧项目没有 GA，取新默认 30 |
| 遗传算法迭代代数（代） | `ga_generations` | — | 20 | 旧项目没有 GA，取新默认 20 |
| 遗传算法变异率 | `ga_mutation_rate` | — | 0.1 | 旧项目没有 GA，取新默认 0.1 |
| 遗传算法随机种子 | `ga_seed` | — | 42 | 旧项目没有 GA，取新默认 42 |

### RL 执行（5 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 启用 RL 执行 | `enabled` | — | 关闭 | 旧项目没有 RL 执行，取新默认并关闭 |
| 探索率 ε | `epsilon` | — | 0.05 | 旧项目没有 RL 执行，取新默认 |
| 最长等待（毫秒） | `max_wait_ms` | — | 3000 | 旧项目没有 RL 执行，取新默认 |
| 学习率 α | `alpha` | — | 0.1 | 旧项目没有 RL 执行，取新默认 |
| 折扣因子 γ | `gamma` | — | 0.9 | 旧项目没有 RL 执行，取新默认 |

### 自动换池（3 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 启用自动换池 | `enabled` | 开启 | 开启 | 沿用旧项目 |
| 扫描间隔（小时） | `interval_hours` | 3 | 3 | 沿用旧项目 |
| 扫描数量（个） | `market_limit` | 300 | 300 | 沿用旧项目 |

### 风控（25 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 单仓保证金中枢（%） | `margin_pct` | 20 | 20 | 按要求：每仓保证金 20% |
| 杠杆下限（x） | `min_leverage` | 5 | 5 | 按要求：杠杆区间下限 5x |
| 杠杆上限（x） | `max_leverage` | 16 | 16 | 按要求：杠杆区间上限 16x |
| 本地杠杆基准（x） | `local_leverage_base` | — | 10 | 新增本地杠杆规则基准；ATR≈2% 时约 10x，最终夹在 5–16x |
| 杠杆波动目标（%） | `local_leverage_vol_target` | — | 2 | 新增；ATR 2% 视为中性波动，低波动升杠杆、高波动降杠杆 |
| 杠杆评分权重（x） | `local_leverage_score_weight` | — | 10 | 新增；评分相对 0.5 每高 1.0 加 10x，最终仍夹在 5–16x |
| 总敞口上限（%） | `max_exposure_pct` | 3000 | 3000 | 沿用旧值 3000% |
| 同向持仓上限（个） | `max_same_direction` | 2 | 2 | 沿用旧值 2 个 |
| 高相关同向上限（个） | `correlation_limit` | — | 2 | 新增字段；与同向上限 2 保持一致 |
| 同向敞口上限（%） | `direction_exposure_limit_pct` | — | 1000 | 新增字段；16x × 20% 单仓≈320% 权益，1000% 最多容纳 3 仓（该字段上限即 1000） |
| 仓位止损率（%） | `global_stop_loss_pct` | 10 | 10 | 沿用旧值 10%；注意高杠杆下价格止损仅 0.6–2%，可能先于 ATR 止损触发，历史 6 笔此项共 -10.19，必要时改为 0 |
| 仓位止盈率（%） | `global_take_profit_pct` | 15 | 15 | 沿用旧值 15%；高杠杆下会较早止盈，必要时改为 0 交给 ROI/移动止损 |
| 开仓止损 ATR 倍数（×） | `stop_distance_atr_mult` | 1.3 | 1.3 | 对应旧 risk.atr_stop_mult=1.3 |
| 日内亏损上限（%） | `daily_loss_limit_pct` | 10 | 10 | 沿用旧项目 |
| 最大回撤（%） | `max_drawdown_pct` | 25 | 25 | 沿用旧项目 |
| 连亏熔断（笔） | `consecutive_loss_limit` | 4 | 4 | 沿用旧项目 |
| 熔断冷静期（分钟） | `cooldown_minutes` | 35 | 35 | 沿用旧项目 |
| 单标的冷却（分钟） | `symbol_cooldown_minutes` | 20 | 20 | 沿用旧项目 |
| 最低运行权益（USDT） | `min_equity_usdt` | 0.5 | 0.5 | 沿用旧项目 |
| 相关性组敞口（%） | `category_exposure_limit_pct` | 500 | 500 | 沿用旧项目 |
| 波动率目标（%） | `vol_target_atr_pct` | 2 | 2 | 沿用旧值 2%；用于保证金波动缩放 |
| 自动补挂保护单 | `auto_protect_external` | 开启 | 开启 | 沿用旧项目 |
| 最小名义额缓冲（×） | `min_notional_buffer` | 1.05 | 1.05 | 沿用旧项目 |
| 吃单费率 | `fee_rate_taker` | 0.0005 | 0.0005 | 沿用旧项目 |
| 挂单费率 | `fee_rate_maker` | 0.0002 | 0.0002 | 沿用旧项目 |

### 交易所（6 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 模拟盘域名 | `demo_base_url` | https://testnet.binancefuture.com | https://testnet.binancefuture.com | 沿用旧项目 |
| 实盘域名 | `live_base_url` | https://fapi.binance.com | https://fapi.binance.com | 沿用旧项目 |
| 实盘备用域名 | `backup_base_urls` | https://fapi1.binance.com、https://fapi2.binance.com、https://fapi3.binance.com | https://fapi1.binance.com、https://fapi2.binance.com、https://fapi3.binance.com | 沿用旧项目 |
| HTTP 代理 | `proxy` | 空 | 空 | 沿用旧项目 |
| 本地盘口 | `book_ws_enabled` | — | 开启 | 新字段，取默认开启本地盘口 |
| 盘口过期（毫秒） | `book_stale_ms` | — | 3000 | 新字段，取默认 3000 毫秒 |

### 资讯（9 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 启用 | `enabled` | 开启 | 开启 | 沿用旧项目 |
| 刷新间隔（分钟） | `refresh_minutes` | 10 | 10 | 沿用旧项目 |
| 保留天数（天） | `keep_days` | 7 | 7 | 沿用旧项目 |
| 资讯源 | `sources` | cointelegraph、theblock、decrypt、bitcoinmagazine、newsbtc、beincrypto、utoday、binance_square、binance_announcement、coindesk | cointelegraph、theblock、decrypt、bitcoinmagazine、newsbtc、beincrypto、utoday、binance_square、binance_announcement、coindesk | 沿用旧项目 |
| AI 摘要 | `ai_summary` | 开启 | 开启 | 沿用旧项目 |
| 情绪阈值 | `sentiment_threshold` | 0.35 | 0.35 | 沿用旧项目 |
| 事件否决 | `veto_enabled` | — | 开启 | 新字段，取默认开启事件否决 |
| 事件回看窗口（分钟） | `veto_window_min` | — | 30 | 新字段，取默认回看 30 分钟 |
| 否决引用条数（条） | `veto_max_items` | — | 3 | 新字段，取默认引用 3 条 |

### 通知与系统（13 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 日志级别 | `log_level` | WARNING | WARNING | 沿用旧项目 |
| 事件保留（天） | `event_retention_days` | 14 | 14 | 沿用旧项目 |
| 信号保留（天） | `signal_retention_days` | 14 | 14 | 沿用旧项目 |
| 权益曲线保留（天） | `equity_retention_days` | 30 | 30 | 沿用旧项目 |
| 启用告警 | `alert_enabled` | 关闭 | 关闭 | 沿用旧项目 |
| 最低级别 | `alert_min_level` | warn | warn | 沿用旧项目 |
| Webhook 地址 | `alert_webhook_url` | 空 | 空 | 沿用旧项目 |
| Error 专用 Webhook | `alert_error_webhook_url` | 空 | 空 | 沿用旧项目 |
| Telegram Bot Token | `alert_telegram_token` | 空 | 空 | 沿用旧项目 |
| Telegram Chat ID | `alert_telegram_chat_id` | 空 | 空 | 沿用旧项目 |
| 同类告警冷却（秒） | `alert_cooldown_sec` | 120 | 120 | 沿用旧项目 |
| API 限频（次/分/IP） | `api_rate_limit_per_min` | 0 | 0 | 沿用旧项目 |
| Prometheus 指标 | `metrics_enabled` | 开启 | 开启 | 沿用旧项目 |

### 备份（4 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 备份保留份数（份） | `retention` | 10 | 10 | 沿用旧项目 |
| 备份加密 | `encrypt` | 开启 | 开启 | 沿用旧项目 |
| 每日自动备份 | `auto_daily` | 开启 | 开启 | 沿用旧项目 |
| 包含日志文件 | `include_logs` | 关闭 | 关闭 | 沿用旧项目 |

### 安全（5 项）

| 中文字段 | 参数键 | 旧项目值 | 新项目值 | 说明 |
|---|---|---|---|---|
| 管理员账号 | `username` | admin | admin | 沿用旧项目 |
| 强制登录 | `require_login` | 开启 | 开启 | 沿用旧项目 |
| 会话有效期（小时） | `session_ttl_hours` | 12 | 12 | 沿用旧项目 |
| 登录失败锁定阈值（次） | `login_fail_limit` | 8 | 8 | 沿用旧项目 |
| 失败计数窗口（秒） | `login_fail_window_sec` | 900 | 900 | 沿用旧项目 |

## 四、旧项目有、新项目不再暴露/已删除

| 旧字段 | 处理 |
|---|---|
| `ai.allow_ai_override` / `ai.require_agreement` / `ai.veto_only` | 新项目半 AI 固定为：AI 只能同意/否决/降仓，字段已删除 |
| `ai_tuning.auto_apply` / `auto_apply_timeframes` / `max_auto_apply_per_24h` | 新项目改为证据门通过后自动激活；配置项已删除 |
| `signal_follow.*` | 新项目没有该交易链路，不迁移 |
| `risk.atr_stop_mult` | 已映射为 `risk.stop_distance_atr_mult=1.3` |
| `ai.timeout_sec` 等全 AI 参数 | 新项目复核用 `review_timeout_sec` / `review_budget_sec`，其余隐藏 |
| `strategy` 的 20 个入场/出场数值参数 | 已隐藏并由 AI 调参接管，按你的要求不进入本表 |
| `engine.mode/enabled/isolated` | 参数页隐藏；旧项目当前为 live、引擎未开启、逐仓开启 |

## 五、不能直接迁移的数据

- 开平仓记录、权益曲线、信号审计、AI 调参历史属于运行数据，不是参数配置；新旧数据库结构不同，不建议直接拷贝旧库，否则会污染新项目的统计、AI 复盘与风控基准。
- 心法可手工重建到「AI → 自进化心法」：旧项目有 1 条“方向与结构描述矛盾时禁止入场”，与新项目否决模式兼容。
- 全局策略正文需改写后再用：旧策略要求 AI 决定方向/杠杆/止损，新项目 AI 不再拥有这些权限。
- 标的池需在新项目重新建立：旧池 12 个标的，统一 max_leverage=16、sl_atr_mult=2.2。
- AI 供应商/Key 需在新项目重新配置；旧项目供应商接口未迁移到 Rust。

## 附录：旧项目全部参数去向

旧项目共 183 个配置字段，逐个标注去向，确保没有漏项。

| 旧分组 | 旧参数键 | 旧项目值 | 处理 | 新位置 | 说明 |
|---|---|---|---|---|---|
| ai | `allow_ai_override` | 开启 | 已移除 | — | 新半 AI 固定为同意/否决/降仓 |
| ai | `allow_param_tuning` | 开启 | 已隐藏 | `ai.allow_param_tuning` | 内部/高级参数，参数页不展示 |
| ai | `decision_schema_version` | 2 | 已隐藏 | `ai.decision_schema_version` | 内部/高级参数，参数页不展示 |
| ai | `enabled` | 开启 | 已隐藏 | `ai.enabled` | 内部/高级参数，参数页不展示 |
| ai | `fallback_provider_ids` | 空（关闭） | 已隐藏 | `ai.fallback_provider_ids` | 内部/高级参数，参数页不展示 |
| ai | `inject_mantras` | 开启 | 已隐藏 | `ai.inject_mantras` | 内部/高级参数，参数页不展示 |
| ai | `inject_news` | 开启 | 已隐藏 | `ai.inject_news` | 内部/高级参数，参数页不展示 |
| ai | `max_calls_per_hour` | 1000 | 已隐藏 | `ai.max_calls_per_hour` | 内部/高级参数，参数页不展示 |
| ai | `min_confidence` | 0.46 | 已隐藏 | `ai.min_confidence` | 内部/高级参数，参数页不展示 |
| ai | `mode` | single | 已隐藏 | `ai.mode` | 内部/高级参数，参数页不展示 |
| ai | `monthly_budget_usd` | 300 | 已隐藏 | `ai.monthly_budget_usd` | 内部/高级参数，参数页不展示 |
| ai | `param_tuning_min_interval_sec` | 86400 | 已隐藏 | `ai.param_tuning_min_interval_sec` | 内部/高级参数，参数页不展示 |
| ai | `personas` | {"enabled":true,"focus":"多周期趋势与关键价位","name":"趋势研判官","weight":0.35}、{"enabled":true,"focus":"量价背离、资金费率与持仓量变化","name":"动能量化官","weight":0.35}、{"enabled":true,"focus":"回撤、爆仓距离与流动性风险","name":"风险审计官","weight":0.3} | 已隐藏 | `ai.personas` | 内部/高级参数，参数页不展示 |
| ai | `provider_id` | 1 | 已隐藏 | `ai.provider_id` | 内部/高级参数，参数页不展示 |
| ai | `require_agreement` | 开启 | 已移除 | — | 新半 AI 固定为同意/否决/降仓 |
| ai | `temperature` | 0.5 | 已隐藏 | `ai.temperature` | 内部/高级参数，参数页不展示 |
| ai | `timeout_sec` | 120 | 已隐藏 | `ai.timeout_sec` | 内部/高级参数，参数页不展示 |
| ai | `use_entry_price` | 关闭 | 已进入新参数表 | `ai.use_entry_price` | 见新参数表 |
| ai | `use_invalidation_price` | 关闭 | 已进入新参数表 | `ai.use_invalidation_price` | 见新参数表 |
| ai_tuning | `auto_apply` | 开启 | 已移除 | — | 改为证据门通过后自动激活 |
| ai_tuning | `auto_apply_timeframes` | 1h | 已移除 | — | 改为证据门通过后自动激活 |
| ai_tuning | `enabled` | 开启 | 已进入新参数表 | `ai_tuning.enabled` | 见新参数表 |
| ai_tuning | `max_auto_apply_per_24h` | 1 | 已移除 | — | 改为证据门通过后自动激活 |
| ai_tuning | `min_oos_trades` | 30 | 已进入新参数表 | `ai_tuning.min_oos_trades` | 见新参数表 |
| ai_tuning | `no_candidate_rollback_sec` | 1800 | 已进入新参数表 | `ai_tuning.no_candidate_rollback_sec` | 见新参数表 |
| ai_tuning | `regime_confirm_sec` | 1800 | 已进入新参数表 | `ai_tuning.regime_confirm_sec` | 见新参数表 |
| ai_tuning | `regime_cooldown_sec` | 1800 | 已进入新参数表 | `ai_tuning.regime_cooldown_sec` | 见新参数表 |
| backup | `auto_daily` | 开启 | 已进入新参数表 | `backup.auto_daily` | 见新参数表 |
| backup | `encrypt` | 开启 | 已进入新参数表 | `backup.encrypt` | 见新参数表 |
| backup | `include_logs` | 关闭 | 已进入新参数表 | `backup.include_logs` | 见新参数表 |
| backup | `last_run_ts` | 1795802861 | 已隐藏 | `backup.last_run_ts` | 内部/高级参数，参数页不展示 |
| backup | `retention` | 10 | 已进入新参数表 | `backup.retention` | 见新参数表 |
| data | `backup_base_urls` | https://fapi1.binance.com、https://fapi2.binance.com、https://fapi3.binance.com | 已进入新参数表 | `data.backup_base_urls` | 见新参数表 |
| data | `demo_api_key_enc` | 空 | 新项目保留但不在参数页 | `data.demo_api_key_enc` | 敏感凭证单独维护 |
| data | `demo_api_secret_enc` | 空 | 新项目保留但不在参数页 | `data.demo_api_secret_enc` | 敏感凭证单独维护 |
| data | `demo_backup_urls` | 空（关闭） | 已隐藏 | `data.demo_backup_urls` | 内部/高级参数，参数页不展示 |
| data | `demo_base_url` | https://testnet.binancefuture.com | 已进入新参数表 | `data.demo_base_url` | 见新参数表 |
| data | `kline_limit` | 200 | 已隐藏 | `data.kline_limit` | 内部/高级参数，参数页不展示 |
| data | `kline_ttl_sec` | 20 | 已隐藏 | `data.kline_ttl_sec` | 内部/高级参数，参数页不展示 |
| data | `live_api_key_enc` | [已脱敏] | 新项目保留但不在参数页 | `data.live_api_key_enc` | 敏感凭证单独维护 |
| data | `live_api_secret_enc` | [已脱敏] | 新项目保留但不在参数页 | `data.live_api_secret_enc` | 敏感凭证单独维护 |
| data | `live_base_url` | https://fapi.binance.com | 已进入新参数表 | `data.live_base_url` | 见新参数表 |
| data | `max_pool_size` | 20 | 已隐藏 | `data.max_pool_size` | 内部/高级参数，参数页不展示 |
| data | `proxy` | 空 | 已进入新参数表 | `data.proxy` | 见新参数表 |
| data | `request_timeout_sec` | 12 | 已隐藏 | `data.request_timeout_sec` | 内部/高级参数，参数页不展示 |
| data | `ticker_ttl_sec` | 5 | 已隐藏 | `data.ticker_ttl_sec` | 内部/高级参数，参数页不展示 |
| engine | `close_on_halt` | 关闭 | 已进入新参数表 | `engine.close_on_halt` | 见新参数表 |
| engine | `cycle_interval_sec` | 600 | 已进入新参数表 | `engine.cycle_interval_sec` | 见新参数表 |
| engine | `enabled` | 关闭 | 由界面开关管理 | `engine.enabled` | 交易环境/引擎启停，不在参数表单 |
| engine | `isolated` | 开启 | 已隐藏 | `engine.isolated` | 内部/高级参数，参数页不展示 |
| engine | `max_positions` | 3 | 已进入新参数表 | `engine.max_positions` | 见新参数表 |
| engine | `max_symbols_per_cycle` | 12 | 已隐藏 | `engine.max_symbols_per_cycle` | 内部/高级参数，参数页不展示 |
| engine | `mode` | live | 由界面开关管理 | `engine.mode` | 交易环境/引擎启停，不在参数表单 |
| engine | `order_type` | market | 已进入新参数表 | `engine.order_type` | 见新参数表 |
| engine | `recv_window_ms` | 5000 | 已隐藏 | `engine.recv_window_ms` | 内部/高级参数，参数页不展示 |
| engine | `scan_concurrency` | 3 | 已隐藏 | `engine.scan_concurrency` | 内部/高级参数，参数页不展示 |
| engine | `timezone_offset` | 8 | 已进入新参数表 | `engine.timezone_offset` | 见新参数表 |
| engine | `trading_windows` | 00:00-24:00 | 已进入新参数表 | `engine.trading_windows` | 见新参数表 |
| evolution | `archive_health_threshold` | 0.45 | 新项目保留但不在参数页 | `evolution.archive_health_threshold` | 对应独立页面或内部配置 |
| evolution | `auto_disable_negative` | 开启 | 新项目保留但不在参数页 | `evolution.auto_disable_negative` | 对应独立页面或内部配置 |
| evolution | `batch_review_size` | 5 | 新项目保留但不在参数页 | `evolution.batch_review_size` | 对应独立页面或内部配置 |
| evolution | `enabled` | 开启 | 新项目保留但不在参数页 | `evolution.enabled` | 对应独立页面或内部配置 |
| evolution | `global_review_every_trades` | 10 | 新项目保留但不在参数页 | `evolution.global_review_every_trades` | 对应独立页面或内部配置 |
| evolution | `guard_strict` | 开启 | 新项目保留但不在参数页 | `evolution.guard_strict` | 对应独立页面或内部配置 |
| evolution | `max_active_mantras` | 40 | 新项目保留但不在参数页 | `evolution.max_active_mantras` | 对应独立页面或内部配置 |
| evolution | `max_injected` | 8 | 新项目保留但不在参数页 | `evolution.max_injected` | 对应独立页面或内部配置 |
| evolution | `max_rule_chars` | 420 | 新项目保留但不在参数页 | `evolution.max_rule_chars` | 对应独立页面或内部配置 |
| evolution | `min_confidence` | 0.5 | 新项目保留但不在参数页 | `evolution.min_confidence` | 对应独立页面或内部配置 |
| evolution | `min_health_sample` | 10 | 新项目保留但不在参数页 | `evolution.min_health_sample` | 对应独立页面或内部配置 |
| evolution | `min_lesson_sample` | 2 | 新项目保留但不在参数页 | `evolution.min_lesson_sample` | 对应独立页面或内部配置 |
| evolution | `review_on_close` | 开启 | 新项目保留但不在参数页 | `evolution.review_on_close` | 对应独立页面或内部配置 |
| evolution | `ttl_days` | 7 | 新项目保留但不在参数页 | `evolution.ttl_days` | 对应独立页面或内部配置 |
| journal | `correct_threshold_pct` | 0.15 | 新项目保留但不在参数页 | `journal.correct_threshold_pct` | 对应独立页面或内部配置 |
| journal | `enabled` | 开启 | 新项目保留但不在参数页 | `journal.enabled` | 对应独立页面或内部配置 |
| journal | `inject_experiences` | 开启 | 新项目保留但不在参数页 | `journal.inject_experiences` | 对应独立页面或内部配置 |
| journal | `label_after_sec` | 86400 | 新项目保留但不在参数页 | `journal.label_after_sec` | 对应独立页面或内部配置 |
| journal | `max_experiences` | 200 | 新项目保留但不在参数页 | `journal.max_experiences` | 对应独立页面或内部配置 |
| journal | `min_record_confidence` | 0.45 | 新项目保留但不在参数页 | `journal.min_record_confidence` | 对应独立页面或内部配置 |
| journal | `min_similarity` | 0.15 | 新项目保留但不在参数页 | `journal.min_similarity` | 对应独立页面或内部配置 |
| journal | `retrieve_top_k` | 5 | 新项目保留但不在参数页 | `journal.retrieve_top_k` | 对应独立页面或内部配置 |
| news | `ai_summary` | 开启 | 已进入新参数表 | `news.ai_summary` | 见新参数表 |
| news | `enabled` | 开启 | 已进入新参数表 | `news.enabled` | 见新参数表 |
| news | `keep_days` | 7 | 已进入新参数表 | `news.keep_days` | 见新参数表 |
| news | `max_items_per_source` | 30 | 已隐藏 | `news.max_items_per_source` | 内部/高级参数，参数页不展示 |
| news | `proxy` | 空 | 已隐藏 | `news.proxy` | 内部/高级参数，参数页不展示 |
| news | `refresh_minutes` | 10 | 已进入新参数表 | `news.refresh_minutes` | 见新参数表 |
| news | `request_timeout_sec` | 8 | 已隐藏 | `news.request_timeout_sec` | 内部/高级参数，参数页不展示 |
| news | `rsshub_base` | 空 | 已隐藏 | `news.rsshub_base` | 内部/高级参数，参数页不展示 |
| news | `sentiment_threshold` | 0.35 | 已进入新参数表 | `news.sentiment_threshold` | 见新参数表 |
| news | `sources` | cointelegraph、theblock、decrypt、bitcoinmagazine、newsbtc、beincrypto、utoday、binance_square、binance_announcement、coindesk | 已进入新参数表 | `news.sources` | 见新参数表 |
| public | `hero_note` | 空 | 新项目保留但不在参数页 | `public.hero_note` | 对应独立页面或内部配置 |
| public | `show_engine_status` | 开启 | 新项目保留但不在参数页 | `public.show_engine_status` | 对应独立页面或内部配置 |
| public | `show_mantras` | 开启 | 新项目保留但不在参数页 | `public.show_mantras` | 对应独立页面或内部配置 |
| public | `show_performance` | 关闭 | 新项目保留但不在参数页 | `public.show_performance` | 对应独立页面或内部配置 |
| public | `site_enabled` | 开启 | 新项目保留但不在参数页 | `public.site_enabled` | 对应独立页面或内部配置 |
| public | `site_tagline` | 小资金 · 币安 U 本位永续 · 超短线量化终端 | 新项目保留但不在参数页 | `public.site_tagline` | 对应独立页面或内部配置 |
| public | `site_title` | NanoQuant | 新项目保留但不在参数页 | `public.site_title` | 对应独立页面或内部配置 |
| risk | `atr_stop_mult` | 1.3 | 重命名为新参数 | `risk.stop_distance_atr_mult` | 值 1.3 已进入新参数表 |
| risk | `auto_protect_external` | 开启 | 已进入新参数表 | `risk.auto_protect_external` | 见新参数表 |
| risk | `category_exposure_limit_pct` | 500 | 已进入新参数表 | `risk.category_exposure_limit_pct` | 见新参数表 |
| risk | `consecutive_loss_limit` | 4 | 已进入新参数表 | `risk.consecutive_loss_limit` | 见新参数表 |
| risk | `cooldown_minutes` | 35 | 已进入新参数表 | `risk.cooldown_minutes` | 见新参数表 |
| risk | `daily_loss_limit_pct` | 10 | 已进入新参数表 | `risk.daily_loss_limit_pct` | 见新参数表 |
| risk | `fee_rate_maker` | 0.0002 | 已进入新参数表 | `risk.fee_rate_maker` | 见新参数表 |
| risk | `fee_rate_taker` | 0.0005 | 已进入新参数表 | `risk.fee_rate_taker` | 见新参数表 |
| risk | `global_stop_loss_pct` | 10 | 已进入新参数表 | `risk.global_stop_loss_pct` | 见新参数表 |
| risk | `global_take_profit_pct` | 15 | 已进入新参数表 | `risk.global_take_profit_pct` | 见新参数表 |
| risk | `margin_pct` | 20 | 已进入新参数表 | `risk.margin_pct` | 见新参数表 |
| risk | `max_drawdown_pct` | 25 | 已进入新参数表 | `risk.max_drawdown_pct` | 见新参数表 |
| risk | `max_exposure_pct` | 3000 | 已进入新参数表 | `risk.max_exposure_pct` | 见新参数表 |
| risk | `max_leverage` | 16 | 已进入新参数表 | `risk.max_leverage` | 见新参数表 |
| risk | `max_same_direction` | 2 | 已进入新参数表 | `risk.max_same_direction` | 见新参数表 |
| risk | `min_equity_usdt` | 0.5 | 已进入新参数表 | `risk.min_equity_usdt` | 见新参数表 |
| risk | `min_leverage` | 5 | 已进入新参数表 | `risk.min_leverage` | 见新参数表 |
| risk | `min_notional_buffer` | 1.05 | 已进入新参数表 | `risk.min_notional_buffer` | 见新参数表 |
| risk | `symbol_cooldown_minutes` | 20 | 已进入新参数表 | `risk.symbol_cooldown_minutes` | 见新参数表 |
| risk | `vol_target_atr_pct` | 2 | 已进入新参数表 | `risk.vol_target_atr_pct` | 见新参数表 |
| security | `login_fail_limit` | 8 | 已进入新参数表 | `security.login_fail_limit` | 见新参数表 |
| security | `login_fail_window_sec` | 900 | 已进入新参数表 | `security.login_fail_window_sec` | 见新参数表 |
| security | `password_hash` | [已脱敏] | 新项目保留但不在参数页 | `security.password_hash` | 安全内部字段 |
| security | `password_is_default` | 关闭 | 已隐藏 | `security.password_is_default` | 内部/高级参数，参数页不展示 |
| security | `require_login` | 开启 | 已进入新参数表 | `security.require_login` | 见新参数表 |
| security | `session_ttl_hours` | 12 | 已进入新参数表 | `security.session_ttl_hours` | 见新参数表 |
| security | `token_version` | 1 | 新项目保留但不在参数页 | `security.token_version` | 安全内部字段 |
| security | `username` | admin | 已进入新参数表 | `security.username` | 见新参数表 |
| signal_follow | `auto_add_symbol` | 开启 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `default_sl_pct` | 1.2 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `dry_run` | 开启 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `enabled` | 关闭 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `ingest_token` | 空 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `jsonl_path` | 空 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `leverage_bonus` | 10 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `margin_pct_override` | 0 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `max_age_sec` | 180 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `max_per_hour` | 6 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `poll_sec` | 1 | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `sizing_mode` | margin_pct | 已移除 | — | 新项目没有跟随信号交易链路 |
| signal_follow | `strict_events` | 关闭 | 已移除 | — | 新项目没有跟随信号交易链路 |
| strategy | `atr_pct_max` | 10 | 已隐藏，交 AI 调参 | `strategy.atr_pct_max` | 按你的要求不进入参数表 |
| strategy | `atr_pct_min` | 0.6 | 已隐藏，交 AI 调参 | `strategy.atr_pct_min` | 按你的要求不进入参数表 |
| strategy | `atr_period` | 14 | 已隐藏 | `strategy.atr_period` | 整数周期参数，AI 不做整数取整，保持内部配置 |
| strategy | `breakeven_at_r` | 1.5 | 已隐藏，交 AI 调参 | `strategy.breakeven_at_r` | 按你的要求不进入参数表 |
| strategy | `data_stale_mult` | 2 | 已隐藏，交 AI 调参 | `strategy.data_stale_mult` | 按你的要求不进入参数表 |
| strategy | `ema_fast` | 9 | 已隐藏 | `strategy.ema_fast` | 整数周期参数，AI 不做整数取整，保持内部配置 |
| strategy | `ema_slow` | 21 | 已隐藏 | `strategy.ema_slow` | 整数周期参数，AI 不做整数取整，保持内部配置 |
| strategy | `entry_threshold` | 0.45 | 已隐藏，交 AI 调参 | `strategy.entry_threshold` | 按你的要求不进入参数表 |
| strategy | `exit_threshold` | 0.1 | 已隐藏，交 AI 调参 | `strategy.exit_threshold` | 按你的要求不进入参数表 |
| strategy | `max_data_gaps` | 1 | 已隐藏 | `strategy.max_data_gaps` | 整数周期参数，AI 不做整数取整，保持内部配置 |
| strategy | `max_spread_bps` | 16 | 已隐藏，交 AI 调参 | `strategy.max_spread_bps` | 按你的要求不进入参数表 |
| strategy | `min_24h_quote_volume` | 100000 | 已隐藏，交 AI 调参 | `strategy.min_24h_quote_volume` | 按你的要求不进入参数表 |
| strategy | `min_entry_rr` | 0.8 | 已进入新参数表 | `strategy.min_entry_rr` | 见新参数表 |
| strategy | `mode` | fusion | 已隐藏 | `strategy.mode` | 内部/高级参数，参数页不展示 |
| strategy | `partial_tp_at_r` | 2 | 已隐藏，交 AI 调参 | `strategy.partial_tp_at_r` | 按你的要求不进入参数表 |
| strategy | `partial_tp_ratio` | 0.3 | 已隐藏，交 AI 调参 | `strategy.partial_tp_ratio` | 按你的要求不进入参数表 |
| strategy | `roi_min_r` | 1 | 已隐藏，交 AI 调参 | `strategy.roi_min_r` | 按你的要求不进入参数表 |
| strategy | `roi_table` | 空（关闭） | 已进入新参数表 | `strategy.roi_table` | 见新参数表 |
| strategy | `rsi_period` | 14 | 已隐藏 | `strategy.rsi_period` | 整数周期参数，AI 不做整数取整，保持内部配置 |
| strategy | `timeframe` | 1h | 已进入新参数表 | `strategy.timeframe` | 见新参数表 |
| strategy | `trailing_arm_r` | 1 | 已隐藏，交 AI 调参 | `strategy.trailing_arm_r` | 按你的要求不进入参数表 |
| strategy | `trailing_pct` | 2 | 已隐藏，交 AI 调参 | `strategy.trailing_pct` | 按你的要求不进入参数表 |
| strategy | `valid_bars` | 6 | 已进入新参数表 | `strategy.valid_bars` | 见新参数表 |
| strategy | `volume_ratio_min` | 0.8 | 已隐藏，交 AI 调参 | `strategy.volume_ratio_min` | 按你的要求不进入参数表 |
| symbol_scan | `enabled` | 开启 | 已进入新参数表 | `symbol_scan.enabled` | 见新参数表 |
| symbol_scan | `interval_hours` | 3 | 已进入新参数表 | `symbol_scan.interval_hours` | 见新参数表 |
| symbol_scan | `market_limit` | 300 | 已进入新参数表 | `symbol_scan.market_limit` | 见新参数表 |
| system | `alert_cooldown_sec` | 120 | 已进入新参数表 | `system.alert_cooldown_sec` | 见新参数表 |
| system | `alert_digest_enabled` | 关闭 | 已隐藏 | `system.alert_digest_enabled` | 内部/高级参数，参数页不展示 |
| system | `alert_digest_minutes` | 60 | 已隐藏 | `system.alert_digest_minutes` | 内部/高级参数，参数页不展示 |
| system | `alert_enabled` | 关闭 | 已进入新参数表 | `system.alert_enabled` | 见新参数表 |
| system | `alert_error_webhook_url` | 空 | 已进入新参数表 | `system.alert_error_webhook_url` | 见新参数表 |
| system | `alert_min_level` | warn | 已进入新参数表 | `system.alert_min_level` | 见新参数表 |
| system | `alert_quiet_hours` | 空 | 已隐藏 | `system.alert_quiet_hours` | 内部/高级参数，参数页不展示 |
| system | `alert_telegram_chat_id` | 空 | 已进入新参数表 | `system.alert_telegram_chat_id` | 见新参数表 |
| system | `alert_telegram_token` | 空 | 已进入新参数表 | `system.alert_telegram_token` | 见新参数表 |
| system | `alert_template` | 空 | 已隐藏 | `system.alert_template` | 内部/高级参数，参数页不展示 |
| system | `alert_webhook_url` | 空 | 已进入新参数表 | `system.alert_webhook_url` | 见新参数表 |
| system | `api_rate_limit_per_min` | 0 | 已进入新参数表 | `system.api_rate_limit_per_min` | 见新参数表 |
| system | `equity_retention_days` | 30 | 已进入新参数表 | `system.equity_retention_days` | 见新参数表 |
| system | `event_retention_days` | 14 | 已进入新参数表 | `system.event_retention_days` | 见新参数表 |
| system | `log_level` | WARNING | 已进入新参数表 | `system.log_level` | 见新参数表 |
| system | `metrics_enabled` | 开启 | 已进入新参数表 | `system.metrics_enabled` | 见新参数表 |
| system | `risk_disclaimer_accepted` | 关闭 | 已隐藏 | `system.risk_disclaimer_accepted` | 内部/高级参数，参数页不展示 |
| system | `signal_retention_days` | 14 | 已进入新参数表 | `system.signal_retention_days` | 见新参数表 |

> 旧项目参数总数：183；新项目参数表：106。

> 表内“新项目值”是建议值，不是自动执行结果；应用前请先备份数据库，并在模拟盘验证。