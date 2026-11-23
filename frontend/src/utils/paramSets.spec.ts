import { describe, expect, it } from 'vitest'
import {
  canManuallyActivate,
  evidenceSummary,
  formatPercent,
  gateTag,
  regimeLabel,
  statusTag,
} from './paramSets'

describe('paramSets formatting', () => {
  it('summarizes evidence gate', () => {
    const summary = evidenceSummary({
      data: { folds: 3, symbols_used: ['BTCUSDT'] },
      candidate: { oos_trades: 31, oos_avg_return_pct: 0.42, bootstrap_ci_low: 0.01 },
      gate: { pass: true, reasons: [] },
    })
    expect(summary).toContain('31 笔')
    expect(summary).toContain('3 折')
    expect(summary).toContain('+0.42%')
    expect(summary).toContain('CI低 0.010')
  })

  it('includes drawdown, stress and p when present', () => {
    const summary = evidenceSummary({
      data: { folds: 3 },
      candidate: {
        oos_trades: 40,
        oos_avg_return_pct: 0.5,
        max_drawdown_pct: 8.25,
        stressed_avg_return_pct: -0.1,
        bootstrap_ci_low: 0.02,
        bootstrap_p_positive: 0.95,
      },
    })
    expect(summary).toContain('回撤 8.25%')
    expect(summary).toContain('压力 -0.10%')
    expect(summary).toContain('p 0.95')
  })

  it('handles missing evidence without throwing', () => {
    expect(evidenceSummary({})).toBe('0 折 · 0 笔 · 均收益 --')
  })

  it('maps status tags', () => {
    expect(statusTag('active').text).toBe('生效中')
    expect(statusTag('candidate').text).toBe('候选')
    expect(statusTag('rejected').text).toBe('已拒绝')
    expect(statusTag('bootstrap').text).toBe('基线')
    expect(statusTag('unknown').text).toBe('unknown')
  })

  it('only allows manual activation in the current regime', () => {
    expect(canManuallyActivate('approved', 'range', 'range')).toBe(true)
    expect(canManuallyActivate('manual', 'range', 'range')).toBe(true)
    expect(canManuallyActivate('approved', 'trend', 'range')).toBe(false)
    expect(canManuallyActivate('manual', 'chaos', 'trend')).toBe(false)
    expect(canManuallyActivate('candidate', 'range', 'range')).toBe(false)
    expect(canManuallyActivate('active', 'range', 'range')).toBe(false)
    expect(canManuallyActivate('approved', 'range', null)).toBe(true)
    expect(canManuallyActivate('approved', 'range', '')).toBe(true)
  })

  it('maps regime labels', () => {
    expect(regimeLabel('trend')).toBe('趋势')
    expect(regimeLabel('range')).toBe('震荡')
    expect(regimeLabel('chaos')).toBe('混乱')
    expect(regimeLabel(null)).toBe('--')
    expect(regimeLabel('custom')).toBe('custom')
  })

  it('formats percentages with sign', () => {
    expect(formatPercent(0.42)).toBe('+0.42%')
    expect(formatPercent(-1.5)).toBe('-1.50%')
    expect(formatPercent(null)).toBe('--')
  })

  it('maps gate tags', () => {
    expect(gateTag({ gate: { pass: true } }).text).toBe('证据通过')
    expect(gateTag({ gate: { pass: false } }).text).toBe('证据未过')
    expect(gateTag({}).text).toBe('无证据')
  })
})