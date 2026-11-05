export function fmt(value: number | null | undefined, digits = 2): string {
  if (value === null || value === undefined || Number.isNaN(value)) return '--'
  const abs = Math.abs(value)
  if (abs >= 1e9) return (value / 1e9).toFixed(2) + 'B'
  if (abs >= 1e6) return (value / 1e6).toFixed(2) + 'M'
  if (abs >= 1e4) return value.toLocaleString('en-US', { maximumFractionDigits: 0 })
  if (abs > 0 && abs < 0.01) return value.toFixed(6)
  return value.toLocaleString('en-US', { minimumFractionDigits: digits, maximumFractionDigits: digits })
}

export function px(value: number | null | undefined): string {
  if (!value) return '--'
  const abs = Math.abs(value)
  if (abs >= 1000) return value.toFixed(2)
  if (abs >= 1) return value.toFixed(3)
  if (abs >= 0.01) return value.toFixed(5)
  return value.toFixed(7)
}

export function pct(value: number | null | undefined, digits = 2): string {
  if (value === null || value === undefined || Number.isNaN(value)) return '--'
  return `${value > 0 ? '+' : ''}${value.toFixed(digits)}%`
}

export function usdt(value: number | null | undefined, digits = 4): string {
  if (value === null || value === undefined || Number.isNaN(value)) return '--'
  return `${value > 0 ? '+' : ''}${value.toFixed(digits)}`
}

export function timeStr(ts: number, withSeconds = true): string {
  if (!ts) return '--'
  const d = new Date(ts * 1000)
  const pad = (n: number) => String(n).padStart(2, '0')
  const base = `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
  return withSeconds ? `${base}:${pad(d.getSeconds())}` : base
}

export function ago(ts: number): string {
  if (!ts) return '--'
  const diff = Math.max(0, Math.floor(Date.now() / 1000 - ts))
  if (diff < 60) return `${diff}s 前`
  if (diff < 3600) return `${Math.floor(diff / 60)}m 前`
  if (diff < 86400) return `${Math.floor(diff / 3600)}h 前`
  return `${Math.floor(diff / 86400)}d 前`
}

export function duration(seconds: number): string {
  if (!seconds || seconds < 0) return '--'
  if (seconds < 60) return `${Math.floor(seconds)}s`
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m${Math.floor(seconds % 60)}s`
  return `${Math.floor(seconds / 3600)}h${Math.floor((seconds % 3600) / 60)}m`
}

export function numClass(value: number | null | undefined): string {
  if (!value) return 'flat'
  return value > 0 ? 'up' : 'down'
}

export const EXIT_LABELS: Record<string, string> = {
  stop_loss: '止损',
  take_profit: '止盈',
  roi: '阶梯止盈',
  partial_tp: '部分止盈',
  trailing_stop: '移动止损',
  timeout: '超时',
  signal_flip: '信号反转',
  manual: '手动',
  venue_sync: '交易所同步',
}

export const CATEGORY_LABELS: Record<string, string> = {
  entry: '入场',
  exit: '出场',
  risk: '风控',
  timing: '时机',
  position: '仓位',
  market: '市场结构',
}
