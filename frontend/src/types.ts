export interface Account {
  wallet: number
  available: number
  unrealized: number
  margin_used: number
  equity: number
  mode?: string
  source?: string
  error?: string
}

export interface Position {
  id: number
  symbol: string
  partial_tp?: boolean
  partial_taken_at?: number
  side: 'long' | 'short'
  qty: number
  entry_price: number
  mark_price: number
  leverage: number
  margin: number
  sl_price: number
  tp_price: number
  peak_price?: number
  opened_at: number
  closed_at?: number
  exit_price?: number
  exit_reason?: string
  realized_pnl?: number
  unrealized?: number
  unrealized_source?: 'exchange' | 'estimated'
  venue_ts?: number
  notional?: number
  roi_pct?: number
  liquidation_price?: number
  liquidation_estimated?: boolean
  ai_confidence?: number
  ai_reason?: string
  signal_score?: number
  mode: string
  status: string
  trade_id?: number
}

export interface EngineStatus {
  enabled: boolean
  /** demo=模拟盘(币安测试网), live=实盘(币安主网) */
  mode: 'demo' | 'live'
  env?: string
  env_label?: string
  running: boolean
  heartbeat_age: number
  uptime: number
  last_cycle_ts: number
  last_cycle_ms: number
  last_error: string
  halted: string
  account: Account
  positions: Position[]
  stats: {
    trades: number
    win_rate: number
    realized_pnl: number
    consecutive_losses: number
    cooldowns: Record<string, { until: number; reason: string; remaining: number }>
  }
}

export interface Ticker {
  symbol: string
  last: number
  change_pct: number
  high: number
  low: number
  quote_volume: number
  bid: number
  ask: number
  spread_pct: number
  ts: number
  source: string
}

export interface NewsItem {
  id: number
  ts: number
  source: string
  source_label: string
  title: string
  url: string
  summary: string
  sentiment: number
  symbols: string[]
}

export interface Mantra {
  id: number
  title: string
  rule: string
  tags: string[]
  category: string
  source: string
  confidence: number
  health: number
  hits: number
  wins: number
  losses: number
  enabled: boolean
  version: number
  superseded_by: number
  created_at: number
  updated_at: number
  expires_at: number
  expired: boolean
  evidence: Record<string, any>
}

export interface AiProvider {
  id: number
  name: string
  protocol: string
  base_url: string
  model: string
  temperature: number
  max_tokens: number
  timeout_sec: number
  json_mode: boolean
  enabled: boolean
  priority: number
  role: string
  api_key_masked: string
  has_key: boolean
  extra: Record<string, any>
}

export interface Trade {
  id: number
  symbol: string
  side: string
  qty: number
  entry_price: number
  exit_price: number
  leverage: number
  notional: number
  pnl: number
  pnl_pct: number
  roi_pct: number
  fees: number
  opened_at: number
  closed_at: number
  hold_seconds: number
  exit_reason: string
  ai_confidence: number
  ai_reason: string
  signal_score: number
  mode: string
  reviewed: boolean
  review_note: string
  mantra_ids: number[]
}

export interface Signal {
  id: number
  ts: number
  symbol: string
  direction: string
  score: number
  features: Record<string, any>
  ai_action: string
  ai_confidence: number
  ai_reason: string
  decision: string
  reject_reason: string
}

export interface EventItem {
  id: number
  ts: number
  level: string
  category: string
  message: string
  payload: Record<string, any>
}
