/** 参数集页的纯格式化：证据摘要、状态标签、regime 中文与百分比。 */

export interface EvidenceLike {
  data?: { folds?: number; symbols_used?: string[]; target_regime?: string }
  candidate?: {
    oos_trades?: number
    oos_avg_return_pct?: number
    max_drawdown_pct?: number
    stressed_avg_return_pct?: number
    bootstrap_ci_low?: number
    bootstrap_p_positive?: number
  }
  gate?: { pass?: boolean; reasons?: string[] }
}

export const REGIME_LABELS: Record<string, string> = {
  trend: '趋势',
  range: '震荡',
  chaos: '混乱',
}

export function regimeLabel(regime: string | null | undefined): string {
  if (!regime) return '--'
  return REGIME_LABELS[regime] || regime
}

export function formatPercent(value: number | null | undefined, digits = 2): string {
  if (value === null || value === undefined || Number.isNaN(value)) return '--'
  return `${value > 0 ? '+' : ''}${value.toFixed(digits)}%`
}

function fixedOrDash(value: number | null | undefined, digits: number): string {
  if (value === null || value === undefined || Number.isNaN(value)) return '--'
  return value.toFixed(digits)
}

/** 证据摘要：折数、OOS 成交、均收益，并按需补充回撤/压力/CI/p。 */
export function evidenceSummary(evidence: EvidenceLike): string {
  const folds = evidence.data?.folds ?? 0
  const trades = evidence.candidate?.oos_trades ?? 0
  const parts = [
    `${folds} 折`,
    `${trades} 笔`,
    `均收益 ${formatPercent(evidence.candidate?.oos_avg_return_pct)}`,
  ]
  const drawdown = evidence.candidate?.max_drawdown_pct
  if (drawdown !== null && drawdown !== undefined) parts.push(`回撤 ${fixedOrDash(drawdown, 2)}%`)
  const stress = evidence.candidate?.stressed_avg_return_pct
  if (stress !== null && stress !== undefined) parts.push(`压力 ${formatPercent(stress)}`)
  const ci = evidence.candidate?.bootstrap_ci_low
  if (ci !== null && ci !== undefined) parts.push(`CI低 ${fixedOrDash(ci, 3)}`)
  const p = evidence.candidate?.bootstrap_p_positive
  if (p !== null && p !== undefined) parts.push(`p ${fixedOrDash(p, 2)}`)
  return parts.join(' · ')
}

export function statusTag(status: string): { text: string; cls: string } {
  const map: Record<string, { text: string; cls: string }> = {
    active: { text: '生效中', cls: 'tag-long' },
    approved: { text: '已通过', cls: 'tag-blue' },
    candidate: { text: '候选', cls: 'tag-dim' },
    rejected: { text: '已拒绝', cls: 'tag-short' },
    retired: { text: '已退役', cls: 'tag-dim' },
    bootstrap: { text: '基线', cls: 'tag-blue' },
  }
  return map[status] || { text: status, cls: 'tag-dim' }
}

/** 手动启用只允许当前 regime；缺失 regime 时与后端一致回退 range。 */
export function canManuallyActivate(
  status: string,
  regime: string,
  currentRegime: string | null | undefined
): boolean {
  if (status !== 'approved' && status !== 'manual') return false
  return regime === (currentRegime || 'range')
}

/** 手动回滚只允许当前 regime 的 active 集；缺失 regime 时与后端一致回退 range。 */
export function canRollback(
  status: string,
  regime: string,
  currentRegime: string | null | undefined
): boolean {
  if (status !== 'active') return false
  return regime === (currentRegime || 'range')
}

export function gateTag(evidence: EvidenceLike): { text: string; cls: string } {
  const gate = evidence.gate
  if (!gate) return { text: '无证据', cls: 'tag-dim' }
  if (gate.pass) return { text: '证据通过', cls: 'tag-long' }
  return { text: '证据未过', cls: 'tag-warn' }
}