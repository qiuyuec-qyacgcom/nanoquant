import { describe, expect, it } from 'vitest'
import { activeOrders, distancePct, isActiveOrder, isMakerOrder, orderPurposeLabel } from './dashboard'

describe('dashboard helpers', () => {
  it('filters final order statuses out of active orders', () => {
    const orders = [
      { id: 1, status: 'FILLED' },
      { id: 2, status: 'CANCELED' },
      { id: 3, status: 'CANCELLED' },
      { id: 4, status: 'EXPIRED' },
      { id: 5, status: 'REJECTED' },
    ]
    expect(activeOrders(orders)).toEqual([])
  })

  it('keeps pending and unknown statuses', () => {
    const orders = [
      { id: 1, status: 'NEW' },
      { id: 2, status: 'ACCEPTED' },
      { id: 3, status: 'PARTIALLY_FILLED' },
      { id: 4, status: 'UNKNOWN' },
      { id: 5, status: 'SUBMITTING' },
    ]
    expect(activeOrders(orders).map((order) => order.id)).toEqual([1, 2, 3, 4, 5])
  })

  it('treats missing status as active instead of hiding it', () => {
    expect(isActiveOrder({})).toBe(true)
  })

  it('maps order purposes to Chinese labels', () => {
    expect(orderPurposeLabel('entry')).toBe('开仓')
    expect(orderPurposeLabel('stop_loss')).toBe('止损')
    expect(orderPurposeLabel('take_profit')).toBe('止盈')
    expect(orderPurposeLabel('custom')).toBe('custom')
    expect(orderPurposeLabel()).toBe('--')
  })

  it('selects only maker limit orders', () => {
    const orders = [
      { id: 1, order_type: 'LIMIT' },
      { id: 2, order_type: 'MARKET' },
      { id: 3, order_type: 'STOP_MARKET' },
      { id: 4, order_type: 'LIMIT_POST_ONLY' },
      { id: 5, order_type: 'POST_ONLY' },
      { id: 6 },
    ]
    expect(orders.filter(isMakerOrder).map((order) => order.id)).toEqual([1, 4, 5])
  })

  it('computes absolute distance from mark to trigger', () => {
    expect(distancePct(100, 102)).toBeCloseTo(2)
    expect(distancePct(100, 98)).toBeCloseTo(2)
    expect(distancePct(0, 102)).toBeNull()
    expect(distancePct(100, 0)).toBeNull()
  })
})