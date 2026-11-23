import { describe, expect, it } from 'vitest'
import { displayDirection, displayScore, isPreview } from './signalAudit'

describe('signal audit row rendering', () => {
  it('shows a preview direction and score when quant missed the entry threshold', () => {
    const row = {
      direction: 'long',
      score: 0.397,
      ai_action: 'hold',
      ai_confidence: 0.62,
      features: { stage: 'evaluate' },
    }
    expect(isPreview(row)).toBe(true)
    expect(displayDirection(row)).toEqual({ text: '多(预)', cls: 'up' })
    expect(displayScore(row)).toEqual({ text: '0.40', cls: 'up' })
  })

  it('shows AI hold as watch when no direction was computed', () => {
    const row = {
      direction: 'flat',
      score: 0,
      ai_action: 'hold',
      ai_confidence: 0.62,
      features: { stage: 'evaluate' },
    }
    expect(displayDirection(row)).toEqual({ text: '观望', cls: 'flat' })
    expect(displayScore(row)).toEqual({ text: '0.00', cls: 'flat' })
  })

  it('does not disguise AI confidence as a quant score', () => {
    const row = {
      direction: 'flat',
      score: 0,
      ai_action: 'reject',
      ai_confidence: 0.62,
      features: { stage: 'evaluate' },
    }
    expect(displayScore(row)).toEqual({ text: '0.00', cls: 'flat' })
  })

  it('keeps the gate placeholder when no preview score exists', () => {
    const row = {
      direction: 'flat',
      score: 0,
      ai_action: 'none',
      ai_confidence: 0,
      features: { stage: 'gate' },
    }
    expect(displayScore(row)).toEqual({ text: '—', cls: 'flat' })
    expect(displayDirection(row)).toEqual({ text: '—', cls: 'flat' })
  })
})