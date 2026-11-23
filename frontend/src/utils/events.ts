import type { EventItem } from '../types'

const HIDDEN_RECONCILE_PREFIXES = ['订单对账不一致：']

export function isReconcileNoise(event: Pick<EventItem, 'message'>): boolean {
  const message = event.message.trimStart()
  return HIDDEN_RECONCILE_PREFIXES.some((prefix) => message.startsWith(prefix))
}

export function visibleEvents(events: EventItem[]): EventItem[] {
  return events.filter((event) => !isReconcileNoise(event))
}