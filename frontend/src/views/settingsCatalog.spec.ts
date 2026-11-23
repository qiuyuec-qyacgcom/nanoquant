import { describe, expect, it } from 'vitest'
import { explicitKeys, SETTINGS_GROUPS, SETTINGS_SECTIONS, visibleSettingKeys } from './settingsCatalog'

const groupIds = SETTINGS_GROUPS.map((group) => group.id)
const sectionKeys = Object.values(SETTINGS_SECTIONS)
  .flat()
  .flatMap((section) => section.fields)

describe('参数配置目录', () => {
  it('只保留量化系统常见的分组', () => {
    expect(groupIds).toEqual([
      'engine',
      'strategy',
      'parameter_set',
      'risk',
      'data',
      'news',
      'system',
      'backup',
      'security',
    ])
    expect(groupIds).not.toContain('evolution')
    expect(groupIds).not.toContain('journal')
    expect(groupIds).not.toContain('public')
  })

  it('不再暴露研究实验与内部调参字段', () => {
    const removed = [
      'weight_trend',
      'weight_momentum',
      'weight_volume',
      'weight_funding',
      'weight_oi',
      'ema_fast',
      'ema_slow',
      'rsi_period',
      'mode',
      'adx_filter',
      'adx_min',
      'mr_window',
      'mr_z_entry',
      'mr_min_risk_reward',
      'mr_block_in_trend',
      'mr_trend_gap_pct',
      'roi_min_r',
      'max_data_gaps',
      'data_stale_mult',
      'stoploss_guard_count',
      'stoploss_guard_window_min',
      'stoploss_guard_pause_min',
      'drawdown_peak_window_days',
      'liquidation_warn_pct',
      'max_slippage_pct',
      'rsi_adaptive',
      'adx_period',
      'scan_concurrency',
      'max_symbols_per_cycle',
      'recv_window_ms',
      'kline_limit',
      'ticker_ttl_sec',
      'request_timeout_sec',
      'max_pool_size',
      'alert_template',
      'alert_digest_enabled',
      'alert_quiet_hours',
      'rsshub_base',
      'max_items_per_source',
      'ui_refresh_sec',
    ]
    for (const key of removed) {
      expect(sectionKeys, key).not.toContain(key)
    }
  })

  it('策略分组展示当前唯一信号模型', () => {
    const model = SETTINGS_SECTIONS.strategy.find((section) => section.title === '信号模型 · NQ-Fusion')
    expect(model, '策略页必须能直接看到当前信号模型，便于排查').toBeTruthy()
    expect(model?.hint).toContain('唯一信号模型')
    expect(model?.fields).toEqual([])
  })

  it('保留实盘常用参数', () => {
    const required = [
      'cycle_interval_sec',
      'max_positions',
      'trading_windows',
      'order_type',
      'timeframe',
      'entry_threshold',
      'sl_atr_mult',
      'tp_atr_mult',
      'min_risk_reward',
      'trailing_pct',
      'partial_tp_at_r',
      'max_spread_bps',
      'risk_per_trade_pct',
      'margin_pct',
      'min_leverage',
      'max_leverage',
      'max_exposure_pct',
      'min_equity_usdt',
      'category_exposure_limit_pct',
      'vol_target_atr_pct',
      'auto_protect_external',
      'daily_loss_limit_pct',
      'max_drawdown_pct',
      'global_stop_loss_pct',
      'global_take_profit_pct',
      'consecutive_loss_limit',
      'fee_rate_taker',
      'fee_rate_maker',
      'live_base_url',
      'proxy',
      'refresh_minutes',
      'alert_enabled',
      'alert_webhook_url',
      'retention',
      'session_ttl_hours',
    ]
    for (const key of required) {
      expect(sectionKeys, key).toContain(key)
    }
  })

  it('分组字段不重复', () => {
    for (const [group, sections] of Object.entries(SETTINGS_SECTIONS)) {
      const keys = sections.flatMap((section) => section.fields)
      expect(new Set(keys).size, group).toBe(keys.length)
    }
  })

  it('风控不再提供默认杠杆，只保留上下限', () => {
    const risk = explicitKeys('risk')
    expect(risk.has('default_leverage')).toBe(false)
    expect(risk.has('min_leverage')).toBe(true)
    expect(risk.has('max_leverage')).toBe(true)
  })

  it('不再导出前端写死的隐藏清单', async () => {
    const catalog = await import('./settingsCatalog')
    expect('HIDDEN_BY_GROUP' in catalog).toBe(false)
    expect('HIDDEN_SETTINGS' in catalog).toBe(false)
  })

  it('目录失败时只显示 section 显式字段', () => {
    const keys = visibleSettingKeys('strategy', ['ema_fast', 'entry_threshold', 'timeframe'], new Set(), true)
    expect(keys).toEqual(['entry_threshold', 'timeframe'])
    expect(explicitKeys('strategy').has('ema_fast')).toBe(false)
  })

  it('目录正常时保留 section 之外的字段', () => {
    const keys = visibleSettingKeys('strategy', ['ema_fast', 'entry_threshold'], new Set(), false)
    expect(keys).toEqual(['ema_fast', 'entry_threshold'])
  })
})