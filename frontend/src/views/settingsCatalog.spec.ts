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
      'ai',
      'ai_tuning',
      'rl_exec',
      'symbol_scan',
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
      'mode',
      'adx_filter',
      'adx_min',
      'mr_window',
      'mr_z_entry',
      'mr_min_risk_reward',
      'mr_block_in_trend',
      'mr_trend_gap_pct',
      'rsi_adaptive',
      'adx_period',
    ]
    for (const key of removed) {
      expect(sectionKeys, key).not.toContain(key)
    }
  })

  it('RL 执行分组只暴露执行层参数', () => {
    const sections = SETTINGS_SECTIONS.rl_exec
    expect(sections, 'RL 执行参数必须可配置').toBeTruthy()
    expect(sections.flatMap((section) => section.fields)).toEqual([
      'enabled',
      'epsilon',
      'max_wait_ms',
      'alpha',
      'gamma',
    ])
  })

  it('策略分组展示当前唯一信号模型', () => {
    const model = SETTINGS_SECTIONS.strategy.find((section) => section.title === '信号模型 · NQ-Fusion')
    expect(model, '策略页必须能直接看到当前信号模型，便于排查').toBeTruthy()
    expect(model?.hint).toContain('唯一信号模型')
    expect(model?.fields).toEqual([])
  })

  it('策略分组展示均值回归参数', () => {
    const section = SETTINGS_SECTIONS.strategy.find((s) => s.title === '均值回归 · 震荡市')
    expect(section, '震荡市均值回归参数必须可配置').toBeTruthy()
    expect(section?.fields).toEqual([
      'mr_enabled',
      'mr_rsi_high',
      'mr_rsi_low',
      'mr_band_k',
      'mr_sl_pct',
      'mr_tp_pct',
    ])
  })

  it('保留实盘常用参数', () => {
    const required = [
      'cycle_interval_sec',
      'max_positions',
      'trading_windows',
      'order_type',
      'timeframe',
      'entry_threshold',

      'trailing_pct',
      'partial_tp_at_r',
      'max_spread_bps',

      'margin_pct',
      'min_leverage',
      'max_leverage',
      'max_exposure_pct',
      'atr_stop_mult',
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

  it('AI 调参分组覆盖全部十三键', () => {
    const keys = [...explicitKeys('ai_tuning')].sort()
    expect(keys).toEqual([
      'auto_apply',
      'auto_apply_timeframes',
      'enabled',
      'ga_enabled',
      'ga_generations',
      'ga_mutation_rate',
      'ga_population',
      'ga_seed',
      'max_auto_apply_per_24h',
      'min_oos_trades',
      'no_candidate_rollback_sec',
      'regime_confirm_sec',
      'regime_cooldown_sec',
    ])
  })

  it('自动换池分组覆盖三个字段', () => {
    const keys = [...explicitKeys('symbol_scan')].sort()
    expect(keys).toEqual(['enabled', 'interval_hours', 'market_limit'])
  })

  it('风控不再提供默认杠杆，只保留上下限', () => {
    const risk = explicitKeys('risk')
    expect(risk.has('default_leverage')).toBe(false)
    expect(risk.has('min_leverage')).toBe(true)
    expect(risk.has('max_leverage')).toBe(true)
  })

  it('风控暴露本地杠杆规则参数', () => {
    const risk = explicitKeys('risk')
    expect(risk.has('local_leverage_base')).toBe(true)
    expect(risk.has('local_leverage_vol_target')).toBe(true)
    expect(risk.has('local_leverage_score_weight')).toBe(true)
  })

  it('不再导出前端写死的隐藏清单', async () => {
    const catalog = await import('./settingsCatalog')
    expect('HIDDEN_BY_GROUP' in catalog).toBe(false)
    expect('HIDDEN_SETTINGS' in catalog).toBe(false)
  })

  it('目录失败时只显示 section 显式字段', () => {
    const keys = visibleSettingKeys('strategy', ['adx_period', 'entry_threshold', 'timeframe'], new Set(), true)
    expect(keys).toEqual(['entry_threshold', 'timeframe'])
    expect(explicitKeys('strategy').has('adx_period')).toBe(false)
  })

  it('目录正常时保留 section 之外的字段', () => {
    const keys = visibleSettingKeys('strategy', ['ema_fast', 'entry_threshold'], new Set(), false)
    expect(keys).toEqual(['ema_fast', 'entry_threshold'])
  })

  it('valid_bars 紧跟 K 线周期', () => {
    const basic = SETTINGS_SECTIONS.strategy.find((s) => s.title === '基础')
    const fields = basic?.fields || []
    expect(fields.indexOf('valid_bars')).toBe(fields.indexOf('timeframe') + 1)
  })

  it('min_entry_rr 出现在策略入场过滤', () => {
    const filter = SETTINGS_SECTIONS.strategy.find((s) => s.title === '入场过滤')
    expect(filter?.fields).toContain('min_entry_rr')
  })

  it('AI 复核分组覆盖否决与降仓开关', () => {
    expect([...explicitKeys('ai')].sort()).toEqual([
      'circuit_breaker_failures',
      'circuit_breaker_sec',
      'fail_open_scale',
      'high_score_threshold',
      'low_score_threshold',
      'prompt_profile',
      'provider_order_mode',
      'review_cache_ttl_sec',
      'review_concurrency',
      'review_timeout_sec',
      'scale_on_confidence',
      'use_entry_price',
      'use_invalidation_price',

    ])
  })
})
