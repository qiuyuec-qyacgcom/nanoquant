/**
 * 自动换池扫描结果的纯逻辑：口径统一在这里，视图只负责渲染。
 * 后端 ScanPlan / AppliedPlan 字段可能是数组或数字（历史数据），一律用 resultCount 兜底。
 */

/** 扫描结果字段兼容数组与数字两种形状，缺失或非法值一律按 0。 */
export function resultCount(value: unknown): number {
  if (Array.isArray(value)) return value.length
  if (typeof value === 'number' && Number.isFinite(value)) return value
  return 0
}

export interface ScanCounts {
  scanned: number
  candidates: number
  deleted: number
  kept: number
  skipped: number
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' ? (value as Record<string, unknown>) : {}
}

/**
 * 统一统计上次扫描结果：
 * - deleted 对应 delete（dry-run 为计划替换，apply 后为实际替换）；
 * - skipped 为 delete_skipped + add_skipped，dry-run 阶段两者为 null，自然算 0。
 */
export function scanCounts(lastResult: unknown): ScanCounts {
  const result = asRecord(lastResult)
  return {
    scanned: resultCount(result.scanned),
    candidates: resultCount(result.candidates),
    deleted: resultCount(result.delete),
    kept: resultCount(result.keep),
    skipped: resultCount(result.delete_skipped) + resultCount(result.add_skipped),
  }
}

/** 删除原因兼容两种形状：`{symbol, reason}` 对象与 `[symbol, reason]` 数组。 */
export function deleteReason(plan: unknown, symbol: string): string {
  const reasons = asRecord(plan).delete_reasons
  if (!Array.isArray(reasons)) return ''
  for (const entry of reasons) {
    if (Array.isArray(entry)) {
      if (entry[0] === symbol) return String(entry[1] ?? '')
    } else if (entry && typeof entry === 'object' && (entry as Record<string, unknown>).symbol === symbol) {
      return String((entry as Record<string, unknown>).reason ?? '')
    }
  }
  return ''
}

/** 执行后才知道的跳过名单：dry-run 阶段为 null，表示要等真正执行时才二次检查。 */
export function skippedText(value: unknown): string {
  if (value === null || value === undefined) return '仅执行时检查'
  if (Array.isArray(value) && value.length) return value.join('、')
  return '无'
}

/**
 * keep 里剔除已单独展示的持仓保留（held）与休市保留（suspended）：
 * 保持 keep 原有顺序，并去掉重复项。
 */
export function keepRest(keep: unknown, held: unknown, suspended: unknown): string[] {
  const excluded = new Set<string>()
  for (const list of [held, suspended]) {
    if (!Array.isArray(list)) continue
    for (const symbol of list) excluded.add(String(symbol))
  }

  const out: string[] = []
  const seen = new Set<string>()
  if (!Array.isArray(keep)) return out
  for (const symbol of keep) {
    const text = String(symbol)
    if (excluded.has(text) || seen.has(text)) continue
    seen.add(text)
    out.push(text)
  }
  return out
}