/** K 线单根数据 (public/klines 系列接口的返回格式). */
export interface Candle {
  /** 开盘时间, 毫秒时间戳 */
  t: number
  o: number
  h: number
  l: number
  c: number
  /** 成交量 (基础币) */
  v: number
}

/** 标的池条目 (public/pool) —— 顺序即后台添加顺序. */
export interface PoolSymbol {
  symbol: string
  name: string
  tier: string
  tier_label: string
  /** 1-based 添加序号 */
  order: number
  added_at: number
}
