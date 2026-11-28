export interface SettingGroup {
  id: string
  label: string
  hint: string
}

export interface SettingSection {
  title: string
  hint?: string
  fields: string[]
}

export const SETTINGS_GROUPS: readonly SettingGroup[] = [
  { id: 'engine', label: '交易执行', hint: '运行节奏、交易时段与下单方式' },
  { id: 'strategy', label: '策略', hint: '信号、入场过滤与出场保护' },
  { id: 'ai', label: 'AI 复核', hint: '复核分流、降仓、缓存与失效价/限价开关' },
  { id: 'ai_tuning', label: 'AI 调参', hint: 'AI 生成参数候选，证据门通过后自动激活' },
  { id: 'rl_exec', label: 'RL 执行', hint: '只决定下单时机、限价偏移与拆单，不改方向/杠杆/止损止盈；默认关闭' },
  { id: 'symbol_scan', label: '自动换池', hint: '按配置的间隔与数量扫描 USDT 永续，自动替换不达标且无持仓的标的' },
  { id: 'risk', label: '风控', hint: '仓位、杠杆、熔断与费用' },
  { id: 'data', label: '交易所', hint: '网络、代理与 API 凭证' },
  { id: 'news', label: '资讯', hint: '新闻抓取与情绪' },
  { id: 'system', label: '通知与系统', hint: '告警、日志与数据保留' },
  { id: 'backup', label: '备份', hint: '保留与加密' },
  { id: 'security', label: '安全', hint: '登录与会话' },
]

export const SETTINGS_SECTIONS: Record<string, readonly SettingSection[]> = {
  engine: [
    { title: '运行节奏', fields: ['cycle_interval_sec', 'max_positions', 'max_symbols_per_cycle', 'scan_concurrency'] },
    { title: '交易时段', hint: '只在窗口内开新仓，持仓管理不受限', fields: ['trading_windows', 'timezone_offset'] },
    { title: '下单', fields: ['order_type', 'close_on_halt', 'recv_window_ms'] },
  ],
  strategy: [
    { title: '基础', fields: ['timeframe', 'higher_timeframe', 'valid_bars', 'ema_fast', 'ema_slow', 'rsi_period', 'atr_period'] },
    {
      title: '入场过滤',
      hint: '任一项不达标都不开仓',
      fields: ['entry_threshold', 'exit_threshold', 'volume_ratio_min', 'atr_pct_min', 'atr_pct_max', 'min_24h_quote_volume', 'min_entry_rr', 'funding_veto_bps'],
    },
    {
      title: '出场与保护',
      hint: '趋势仓止损由 ATR 倍数计算；仓位止盈止损率是保证金收益率的强制平仓线，与保护止损先到者生效',
      fields: [
        'trailing_pct',
        'trailing_arm_r',
        'breakeven_at_r',
        'partial_tp_at_r',
        'partial_tp_ratio',
        'max_spread_bps',
        'roi_table',
        'roi_min_r',
        'max_data_gaps',
        'data_stale_mult',
      ],
    },
    {
      title: '均值回归 · 震荡市',
      hint: '只在 regime=range 启用：RSI 超买/超卖与布林带越界同向共振，且量能收敛才开仓；止损止盈使用独立的保证金阈值，不继承趋势仓全局值。',
      fields: ['mr_enabled', 'mr_rsi_high', 'mr_rsi_low', 'mr_band_k', 'mr_sl_pct', 'mr_tp_pct'],
    },
    {
      title: '信号模型 · NQ-Fusion',
      hint: '当前唯一信号模型（fusion）。按趋势/震荡/高波动状态加权相对强弱、波动突破、资金费率与 OI/订单流；至少两个因子同向且置信度达标才开仓。',
      fields: [],
    },
  ],
  ai: [
    { title: '复核行为', hint: '受 AI 总开关控制；开启后生效于下一轮开仓复核；复核超时独立于模型请求超时，预算耗尽后剩余候选直接降级为量化', fields: ['use_invalidation_price', 'use_entry_price', 'review_timeout_sec', 'review_budget_sec'] },
    { title: '否决与降仓', hint: 'AI 只能同意/否决/降仓，不改写方向、杠杆、保证金、止损止盈', fields: ['scale_on_confidence', 'fail_open_scale'] },
    { title: '评分分流与缓存', hint: '高分直接放行、低分直接拒绝，仅中间区调用 AI；中间区结论按标的+方向+评分桶缓存；中间区复核按并发上限并行执行', fields: ['high_score_threshold', 'low_score_threshold', 'review_cache_ttl_sec', 'review_concurrency'] },
    { title: '供应商编排与熔断', hint: '按最近 24h P95 延迟升序选择供应商；连续失败达到阈值后短路一段时间，避免反复打 503/超时', fields: ['provider_order_mode', 'circuit_breaker_failures', 'circuit_breaker_sec'] },
    { title: '提示词档位', hint: '完整=14 根 K 线/8 条资讯/全部经验与心法；瘦身=8 根/4 条/3 条，仅压缩注入上下文，不改 prompt 文本与输出上限', fields: ['prompt_profile'] },
  ],
  ai_tuning: [
    { title: '启用', fields: ['enabled'] },
    { title: 'regime 与回滚', fields: ['regime_confirm_sec', 'regime_cooldown_sec', 'no_candidate_rollback_sec'] },
    { title: '证据门', fields: ['min_oos_trades'] },
    {
      title: 'GA 离线搜索',
      hint: '生成候选参数集并走证据门，通过后自动激活；关闭时不影响手动触发',
      fields: ['ga_enabled', 'ga_population', 'ga_generations', 'ga_mutation_rate', 'ga_seed'],
    },
  ],
  rl_exec: [
    {
      title: '开关与探索',
      hint: '只影响下单方式（市价/限价/等待/拆单）；关闭时完全走原执行逻辑，Q 表不参与',
      fields: ['enabled', 'epsilon'],
    },
    {
      title: '等待与学习',
      hint: '等待与拆单暂无执行器支持时自动退化为限价挂单/单笔；alpha/gamma 用于离线回放训练',
      fields: ['max_wait_ms', 'alpha', 'gamma'],
    },
  ],
  symbol_scan: [
    { title: '扫描', fields: ['enabled', 'interval_hours', 'market_limit'] },
  ],
  risk: [
    {
      title: '仓位与杠杆',
      hint: '杠杆由本地规则按 ATR% 与信号评分计算，并夹在上下限之间；单仓保证金是中心值，按 ATR 波动缩放；AI 只能缩下单数量',
      fields: [
        'margin_pct',
        'min_leverage',
        'max_leverage',
        'local_leverage_base',
        'local_leverage_vol_target',
        'local_leverage_score_weight',
        'max_exposure_pct',
        'max_same_direction',
        'correlation_limit',
        'direction_exposure_limit_pct',
      ],
    },
    {
      title: '仓位止盈止损',
      hint: '你的心理预期（保证金收益率）：盈利/亏损达到该比例就强制平仓，由 0.5 秒快平任务实时判定；止损价另由波动保护（stop_distance_atr_mult × ATR%）给出，两者先到者生效，0=关闭',
      fields: ['global_stop_loss_pct', 'global_take_profit_pct', 'stop_distance_atr_mult'],
    },
    {
      title: '熔断与冷却',
      hint: '触发后只锁新开仓，不强制平仓',
      fields: ['daily_loss_limit_pct', 'max_drawdown_pct', 'consecutive_loss_limit', 'cooldown_minutes', 'symbol_cooldown_minutes'],
    },
    {
      title: '高级风控',
      hint: '默认关闭或保持保守值；这些参数会实际参与仓位与保护判定',
      fields: [
        'min_equity_usdt',
        'category_exposure_limit_pct',
        'vol_target_atr_pct',
        'auto_protect_external',
        'liquidation_warn_pct',
        'drawdown_peak_window_days',
        'stoploss_guard_count',
        'stoploss_guard_window_min',
        'stoploss_guard_pause_min',
        'max_slippage_pct',
      ],
    },
    {
      title: '执行成本',
      fields: ['min_notional_buffer', 'fee_rate_taker', 'fee_rate_maker'],
    },
  ],
  data: [
    {
      title: '交易所网络',
      hint: '模拟盘走测试网，实盘走主网；API 凭证在上方单独维护',
      fields: ['demo_base_url', 'live_base_url', 'backup_base_urls', 'demo_backup_urls', 'proxy'],
    },
    {
      title: '行情缓存',
      hint: '只影响引擎内部取价与 API 权重，不改变对外行情接口',
      fields: ['kline_limit', 'ticker_ttl_sec', 'kline_ttl_sec', 'request_timeout_sec', 'max_pool_size'],
    },
    {
      title: '本地盘口',
      hint: 'WebSocket bookTicker 优先，关闭或过期时回退 REST',
      fields: ['book_ws_enabled', 'book_stale_ms'],
    },
  ],
  news: [
    { title: '抓取', fields: ['enabled', 'refresh_minutes', 'keep_days', 'sources', 'max_items_per_source', 'rsshub_base'] },
    { title: '情绪', fields: ['ai_summary', 'sentiment_threshold'] },
    { title: '事件否决', hint: '最近窗口内出现标的负面事件时否决同向开仓', fields: ['veto_enabled', 'veto_window_min', 'veto_max_items'] },
    { title: '网络', hint: '留空则复用「交易所 → HTTP 代理」', fields: ['proxy', 'request_timeout_sec'] },
  ],
  system: [
    { title: '运行与日志', fields: ['log_level', 'ui_refresh_sec'] },
    { title: '数据保留', fields: ['event_retention_days', 'signal_retention_days', 'equity_retention_days'] },
    {
      title: '告警',
      hint: '引擎停摆、账户异常与连续止损等事件推送到 Webhook 或 Telegram',
      fields: [
        'alert_enabled',
        'alert_min_level',
        'alert_webhook_url',
        'alert_error_webhook_url',
        'alert_telegram_token',
        'alert_telegram_chat_id',
        'alert_cooldown_sec',
        'alert_quiet_hours',
        'alert_template',
        'alert_digest_enabled',
        'alert_digest_minutes',
      ],
    },
    {
      title: '安全与观测',
      hint: 'API 限频按 IP 计；健康检查与 /metrics 不计数',
      fields: ['api_rate_limit_per_min', 'metrics_enabled', 'risk_disclaimer_accepted'],
    },
  ],
  backup: [{ title: '备份策略', fields: ['retention', 'encrypt', 'auto_daily', 'include_logs'] }],
  security: [{ title: '登录与会话', fields: ['username', 'require_login', 'session_ttl_hours', 'login_fail_limit', 'login_fail_window_sec'] }],
}
/** section 内显式展示的字段。目录失败时页面只允许显示这些字段。 */
export function explicitKeys(group: string): Set<string> {
  return new Set((SETTINGS_SECTIONS[group] || []).flatMap((section) => section.fields))
}

/** 目录失败时不回落到“显示全部”：只保留 section 显式字段。 */
export function visibleSettingKeys(
  group: string,
  keys: string[],
  skip: Set<string>,
  catalogFailed: boolean,
): string[] {
  const explicit = explicitKeys(group)
  return keys.filter((key) => !skip.has(key) && (!catalogFailed || explicit.has(key)))
}
