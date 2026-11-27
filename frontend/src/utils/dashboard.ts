const ORDER_PURPOSE_LABELS: Record<string, string> = {
  entry: '开仓',
  stop_loss: '止损',
  take_profit: '止盈',
  close: '平仓',
  partial_take_profit: '部分止盈',
  adjust: '调整',
}

export function orderPurposeLabel(purpose?: string): string {
  if (!purpose) return '--'
  return ORDER_PURPOSE_LABELS[purpose] || purpose
}

const FINAL_ORDER_STATUSES = new Set(['FILLED', 'CANCELED', 'CANCELLED', 'EXPIRED', 'REJECTED'])

export function isActiveOrder(order: { status?: string }): boolean {
  return !FINAL_ORDER_STATUSES.has(String(order.status || '').toUpperCase())
}

const MAKER_ORDER_TYPES = new Set(['LIMIT', 'LIMIT_POST_ONLY', 'POST_ONLY'])

export function isMakerOrder(order: { order_type?: string }): boolean {
  return MAKER_ORDER_TYPES.has(String(order.order_type || '').toUpperCase())
}

export function activeOrders<T extends { status?: string }>(orders: T[]): T[] {
  return orders.filter(isActiveOrder)
}

export function distancePct(mark: number, trigger: number): number | null {
  if (!(mark > 0) || !(trigger > 0)) return null
  return Math.abs((trigger - mark) / mark) * 100
}
export function protectionForPosition(
  position: { symbol?: string; side?: string },
  protections: { symbol?: string; side?: string; type?: string; trigger_price?: number }[],
): { stop?: { trigger_price?: number }; takeProfit?: { trigger_price?: number } } {
  const closeSide = position.side === 'long' ? 'SELL' : 'BUY'
  const rows = protections.filter(
    (order) => order.symbol === position.symbol && (!order.side || order.side === closeSide),
  )
  return {
    stop: rows.find((order) => order.type === 'STOP_MARKET'),
    takeProfit: rows.find((order) => order.type === 'TAKE_PROFIT_MARKET'),
  }
}