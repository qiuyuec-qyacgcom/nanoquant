import { describe, expect, it } from 'vitest'
import type { EventItem } from '../types'
import { isReconcileNoise, visibleEvents } from './events'

function event(id: number, message: string, level = 'warn'): EventItem {
  return { id, ts: id, level, category: 'engine', message, payload: {} }
}

describe('visibleEvents', () => {
  it('hides order reconciliation warnings', () => {
    const items = [
      event(1, '订单对账不一致：本地 0 条，交易所 1 条'),
      event(2, '权益更新', 'info'),
    ]
    expect(isReconcileNoise(items[0])).toBe(true)
    expect(visibleEvents(items).map((item) => item.id)).toEqual([2])
  })

  it('keeps other engine warnings', () => {
    const items = [
      event(3, '订单对账不一致：本地 1 条，交易所 0 条'),
      event(4, 'AI 复核跳过开仓'),
    ]
    expect(visibleEvents(items).map((item) => item.id)).toEqual([4])
  })
})