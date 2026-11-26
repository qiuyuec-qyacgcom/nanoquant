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
  { id: 'ai', label: 'AI 复核', hint: '失效价与限价开关（AI 复核行为的开关）' },
  { id: 'ai_tuning', label: 'AI 调参', hint: 'AI 每周期自动调整隐藏参数' },
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
    { title: '基础', fields: ['timeframe', 'valid_bars', 'ema_fast', 'ema_slow', 'rsi_period', 'atr_period'] },
    {
      title: '入场过滤',
      hint: '任一项不达标都不开仓',
      fields: ['entry_threshold', 'exit_threshold', 'volume_ratio_min', 'atr_pct_min', 'atr_pct_max', 'min_24h_quote_volume', 'min_entry_rr', 'funding_veto_bps'],
    },
    {
      title: '出场与保护',
      hint: 'AI 在开仓复核时给出每仓止损/止盈价；仓位止盈止损率是保证金收益率的强制平仓线',
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
      title: '信号模型 · NQ-Fusion',
      hint: '当前唯一信号模型（fusion）。按趋势/震荡/高波动状态加权相对强弱、波动突破、资金费率与 OI/订单流；至少两个因子同向且置信度达标才开仓。',
      fields: [],
    },
  ],
  ai: [
    { title: '复核行为', hint: '默认关闭；开启后生效于下一轮开仓复核', fields: ['use_invalidation_price', 'use_entry_price'] },
    { title: '否决与降仓', hint: 'AI 只决定能否开仓与降仓，不改写方向、杠杆、止损止盈', fields: ['veto_only', 'scale_on_confidence', 'fail_open_scale'] },
  ],
  ai_tuning: [
    { title: '启用与限速', fields: ['enabled', 'auto_apply', 'max_auto_apply_per_24h', 'auto_apply_timeframes'] },
    { title: 'regime 与回滚', fields: ['regime_confirm_sec', 'regime_cooldown_sec', 'no_candidate_rollback_sec'] },
    { title: '证据门', fields: ['min_oos_trades'] },
  ],
  symbol_scan: [
    { title: '扫描', fields: ['enabled', 'interval_hours', 'market_limit'] },
  ],
  risk: [
    {
      title: '仓位与杠杆',
      hint: '杠杆由本地规则按 ATR% 与信号评分计算，并夹在上下限之间；单仓保证金是中心值，AI 只能在复核时降仓',
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
        'atr_stop_mult',
      ],
    },
    {
      title: '仓位止盈止损',
      hint: '你的心理预期（保证金收益率）：盈利/亏损达到该比例就强制平仓；与 AI 给出的价格先触发者生效，0=关闭',
      fields: ['global_stop_loss_pct', 'global_take_profit_pct'],
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
