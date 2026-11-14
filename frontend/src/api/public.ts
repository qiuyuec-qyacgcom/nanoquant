/** 公开前台接口 (无需登录, 不做 401 跳转) */
const BASE = '/api/v1/public'

export async function pubGet<T = any>(path: string): Promise<T> {
  const res = await fetch(`${BASE}${path}`, { headers: { Accept: 'application/json' } })
  const text = await res.text()
  let payload: any = null
  try {
    payload = text ? JSON.parse(text) : null
  } catch {
    payload = { detail: text.slice(0, 200) }
  }
  if (!res.ok) {
    const detail = payload?.detail
    throw new Error(typeof detail === 'string' ? detail : `HTTP ${res.status}`)
  }
  return payload as T
}

export interface PublicConfig {
  site_enabled: boolean
  site_title: string
  site_tagline: string
  hero_note: string
  show_engine_status: boolean
  show_mantras: boolean
  show_performance: boolean
  mode: string
  version: string
  server_time: number
}

export const publicConfig = () => pubGet<PublicConfig>('/config')
export const publicOverview = () => pubGet('/overview')

/** 标的池 (顺序 = 后台添加顺序) */
export const publicPool = () =>
  pubGet<{ symbols: import('../types/candle').PoolSymbol[]; count: number; empty: boolean; updated_at: number }>('/pool')

/** 单标的 K 线 (池内标的兜底, 用于池子大于批量上限时按需加载) */
export const publicKlines = (symbol: string, interval = '15m', limit = 160) =>
  pubGet<{
    symbol: string
    interval: string
    items: import('../types/candle').Candle[]
    count: number
    last_open_time: number
    next_close_at: number
    server_time: number
    updated_at: number
  }>(
    `/klines?symbol=${encodeURIComponent(symbol)}&interval=${encodeURIComponent(interval)}&limit=${limit}`
  )

/** 池内标的 24h 快照 (价格/涨跌), 给 K 线列表用 */
export const publicQuotes = () =>
  pubGet<{
    symbols: string[]
    tickers: Record<string, { last: number; change_pct: number; high: number; low: number; quote_volume: number; ts: number }>
    updated_at: number
  }>('/quotes')

/** 池内多标的 K 线 (空池返回 empty=true, 不会请求池外合约) */
export const publicKlinesBatch = (interval = '1m', limit = 160) =>
  pubGet<{
    symbols: string[]
    items: Record<string, import('../types/candle').Candle[]>
    tickers: Record<string, { last: number; change_pct: number; high: number; low: number; quote_volume: number; ts: number }>
    errors: Record<string, string>
    interval: string
    empty: boolean
    updated_at: number
  }>(`/klines/batch?interval=${encodeURIComponent(interval)}&limit=${limit}`)
export const publicNews = (limit = 30, source = '') =>
  pubGet(`/news?limit=${limit}${source ? `&source=${encodeURIComponent(source)}` : ''}`)
export const publicMantras = (limit = 8) => pubGet(`/mantras?limit=${limit}`)
export const publicPerformance = () => pubGet('/performance')
