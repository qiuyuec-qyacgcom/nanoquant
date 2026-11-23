import { numClass } from './format'

export interface SignalAuditRow {
  direction?: string | null
  score?: number | null
  ai_action?: string | null
  ai_confidence?: number | null
  features?: Record<string, unknown> | null
}

const DIRECTION_LABELS: Record<string, string> = { long: '多', short: '空', flat: '—' }

export function isGate(row: SignalAuditRow): boolean {
  return row.features?.stage === 'gate'
}

export function isPreview(row: SignalAuditRow): boolean {
  return isGate(row) || row.features?.stage === 'evaluate'
}

export function displayDirection(row: SignalAuditRow): { text: string; cls: string } {
  if (row.direction && row.direction !== 'flat') {
    return {
      text: (DIRECTION_LABELS[row.direction] || row.direction) + (isPreview(row) ? '(预)' : ''),
      cls: row.direction === 'long' ? 'up' : 'down',
    }
  }
  if (row.ai_action === 'open_long') return { text: '多(AI)', cls: 'up' }
  if (row.ai_action === 'open_short') return { text: '空(AI)', cls: 'down' }
  if (row.ai_action === 'hold') return { text: '观望', cls: 'flat' }
  return { text: '—', cls: 'flat' }
}

export function displayScore(row: SignalAuditRow): { text: string; cls: string } {
  if (row.score) {
    return { text: row.score.toFixed(2), cls: numClass(row.score) }
  }
  if (isGate(row)) {
    return { text: '—', cls: 'flat' }
  }
  return { text: '0.00', cls: 'flat' }
}