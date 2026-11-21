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
  { id: 'risk', label: '风控', hint: '仓位、杠杆、熔断与费用' },
  { id: 'data', label: '交易所', hint: '网络、代理与 API 凭证' },
  { id: 'news', label: '资讯', hint: '新闻抓取与情绪' },
  { id: 'system', label: '通知与系统', hint: '告警、日志与数据保留' },
  { id: 'backup', label: '备份', hint: '保留与加密' },
  { id: 'security', label: '安全', hint: '登录与会话' },
]

export const SETTINGS_SECTIONS: Record<string, readonly SettingSection[]> = {
  engine: [
    { title: '运行节奏', fields: ['cycle_interval_sec', 'max_positions'] },
    { title: '交易时段', hint: '只在窗口内开新仓，持仓管理不受限', fields: ['trading_windows', 'timezone_offset'] },
    { title: '下单', fields: ['order_type', 'close_on_halt'] },
  ],
  strategy: [
    { title: '基础', fields: ['timeframe'] },
    {
      title: '入场过滤',
      hint: '任一项不达标都不开仓',
      fields: ['entry_threshold', 'exit_threshold', 'volume_ratio_min', 'atr_pct_min', 'atr_pct_max', 'min_24h_quote_volume'],
    },
    {
      title: '出场与保护',
      hint: '止损止盈按 ATR 计算；全局止盈止损优先于这里的 ATR 规则',
      fields: [
        'sl_atr_mult',
        'tp_atr_mult',
        'min_risk_reward',
        'trailing_pct',
        'breakeven_at_r',
        'partial_tp_at_r',
        'partial_tp_ratio',
        'max_spread_bps',
        'roi_table',
      ],
    },
    {
      title: '信号模型 · NQ-Fusion',
      hint: '当前唯一信号模型（fusion）。按趋势/震荡/高波动状态加权相对强弱、波动突破、资金费率与 OI/订单流；至少两个因子同向且置信度达标才开仓。',
      fields: [],
    },
  ],
  risk: [
    {
      title: '仓位与杠杆',
      hint: '这里只设杠杆区间；开仓杠杆由 AI 在区间内按波动与置信度给出，AI 未给值时取杠杆下限',
      fields: ['risk_per_trade_pct', 'margin_pct', 'min_leverage', 'max_leverage', 'max_exposure_pct', 'max_same_direction'],
    },
    {
      title: '全局止盈止损',
      hint: '按开仓价涨跌幅计算；优先于 ATR 止损、部分止盈与 ROI 阶梯，0=关闭',
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
      fields: ['min_equity_usdt', 'category_exposure_limit_pct', 'vol_target_atr_pct', 'auto_protect_external'],
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
      fields: ['demo_base_url', 'live_base_url', 'backup_base_urls', 'proxy'],
    },
  ],
  news: [
    { title: '抓取', fields: ['enabled', 'refresh_minutes', 'keep_days', 'sources'] },
    { title: '情绪', fields: ['ai_summary', 'sentiment_threshold'] },
  ],
  system: [
    { title: '运行与日志', fields: ['log_level'] },
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
      ],
    },
    {

      title: '安全与观测',
      hint: 'API 限频按 IP 计；健康检查与 /metrics 不计数',
      fields: ['api_rate_limit_per_min', 'metrics_enabled'],
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
