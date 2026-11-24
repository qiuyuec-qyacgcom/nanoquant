import { describe, expect, it } from 'vitest'
import { deleteReason, keepRest, resultCount, scanCounts, skippedText } from './symbolScan'

/**
 * 自动换池结果既要兼容后端数组/数字两种形状，又要在弹窗里区分
 * "保留（其余）/持仓保留/休市保留"，这里把口径钉死，避免视图各写一套。
 */
describe('resultCount', () => {
  it('数组取长度，数字原样返回', () => {
    expect(resultCount(['BTCUSDT', 'ETHUSDT'])).toBe(2)
    expect(resultCount(7)).toBe(7)
  })

  it('null、缺失、非数字一律按 0', () => {
    expect(resultCount(null)).toBe(0)
    expect(resultCount(undefined)).toBe(0)
    expect(resultCount(Number.NaN)).toBe(0)
    expect(resultCount(Number.POSITIVE_INFINITY)).toBe(0)
    expect(resultCount('12')).toBe(0)
    expect(resultCount({ length: 3 })).toBe(0)
  })
})

describe('scanCounts', () => {
  it('完整对象按字段汇总，跳过数合并删除/新增两路', () => {
    const result = {
      scanned: 120,
      candidates: ['BTCUSDT', 'ETHUSDT'],
      keep: ['BTCUSDT'],
      delete: ['AAAUSDT', 'BBBUSDT'],
      delete_skipped: ['CCCUSDT'],
      add_skipped: ['DDDUSDT', 'EEEUSDT'],
    }
    expect(scanCounts(result)).toEqual({
      scanned: 120,
      candidates: 2,
      deleted: 2,
      kept: 1,
      skipped: 3,
    })
  })

  it('null 与缺失字段都降级为 0', () => {
    expect(scanCounts(null)).toEqual({ scanned: 0, candidates: 0, deleted: 0, kept: 0, skipped: 0 })
    expect(scanCounts({ scanned: 10 })).toEqual({ scanned: 10, candidates: 0, deleted: 0, kept: 0, skipped: 0 })
  })

  it('数字形状的 scanned/candidates 也能统计', () => {
    expect(scanCounts({ scanned: 40, candidates: 12, delete: 3, keep: 9 })).toEqual({
      scanned: 40,
      candidates: 12,
      deleted: 3,
      kept: 9,
      skipped: 0,
    })
  })
})

describe('deleteReason', () => {
  const plan = {
    delete_reasons: [
      { symbol: 'AAAUSDT', reason: '流动性不足' },
      ['BBBUSDT', '上市时间过短'],
    ],
  }

  it('兼容 {symbol, reason} 对象与 [symbol, reason] 数组', () => {
    expect(deleteReason(plan, 'AAAUSDT')).toBe('流动性不足')
    expect(deleteReason(plan, 'BBBUSDT')).toBe('上市时间过短')
  })

  it('未命中、无原因或计划缺失时返回空串', () => {
    expect(deleteReason(plan, 'ZZZUSDT')).toBe('')
    expect(deleteReason(null, 'AAAUSDT')).toBe('')
    expect(deleteReason({}, 'AAAUSDT')).toBe('')
    expect(deleteReason({ delete_reasons: [{ symbol: 'AAAUSDT' }] }, 'AAAUSDT')).toBe('')
  })
})

describe('skippedText', () => {
  it('null/undefined 表示仅在执行时检查', () => {
    expect(skippedText(null)).toBe('仅执行时检查')
    expect(skippedText(undefined)).toBe('仅执行时检查')
  })

  it('空数组是明确的无跳过，非空数组列出标的', () => {
    expect(skippedText([])).toBe('无')
    expect(skippedText(['AAAUSDT', 'BBBUSDT'])).toBe('AAAUSDT、BBBUSDT')
  })
})

describe('keepRest', () => {
  it('剔除持仓保留与休市保留，保持原顺序', () => {
    expect(keepRest(['BTCUSDT', 'ETHUSDT', 'SOLUSDT'], ['ETHUSDT'], ['SOLUSDT'])).toEqual(['BTCUSDT'])
  })

  it('不改变未被剔除项的先后顺序', () => {
    expect(keepRest(['C', 'A', 'B'], ['B'], [])).toEqual(['C', 'A'])
  })

  it('keep 内部重复项去重，也容忍脏数据', () => {
    expect(keepRest(['A', 'A', 'B'], null, undefined)).toEqual(['A', 'B'])
    expect(keepRest(null, ['A'], ['B'])).toEqual([])
  })
})