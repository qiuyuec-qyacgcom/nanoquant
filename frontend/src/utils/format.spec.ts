import { describe, expect, it } from 'vitest'
import {
  ago,
  duration,
  fmt,
  numClass,
  pct,
  px,
  timeStr,
  usdt,
  EXIT_LABELS,
  CATEGORY_LABELS,
} from './format'

/**
 * 这些函数铺满整个面板，一个单位换算写错就是"看着有数字、实际错十倍"。
 * 重点盯三处：空值降级、量级缩写边界、正负号。
 */
describe('fmt', () => {
  it('把空值与 NaN 统一降级成 --', () => {
    expect(fmt(null)).toBe('--')
    expect(fmt(undefined)).toBe('--')
    expect(fmt(Number.NaN)).toBe('--')
  })

  it('按量级缩写 B / M', () => {
    expect(fmt(1.5e9)).toBe('1.50B')
    expect(fmt(-2.5e6)).toBe('-2.50M')
  })

  it('万级只取整数，避免数字太长挤爆格子', () => {
    expect(fmt(12345)).toBe('12,345')
    expect(fmt(9999)).toBe('9,999.00')
  })

  it('极小值保留 6 位小数', () => {
    expect(fmt(0.0012349)).toBe('0.001235')
    expect(fmt(0)).toBe('0.00')
  })

  it('尊重自定义小数位', () => {
    expect(fmt(1.2345, 4)).toBe('1.2345')
  })
})

describe('px', () => {
  it('价格按量级选择精度', () => {
    expect(px(65432.123)).toBe('65432.12')
    expect(px(1.23456)).toBe('1.235')
    expect(px(0.0234567)).toBe('0.02346')
    expect(px(0.0001234)).toBe('0.0001234')
  })

  it('0 与空值都算缺失', () => {
    expect(px(0)).toBe('--')
    expect(px(null)).toBe('--')
  })
})

describe('pct / usdt 的符号', () => {
  it('正数带 +，负数保留自己的 -', () => {
    expect(pct(1.234)).toBe('+1.23%')
    expect(pct(-1.234)).toBe('-1.23%')
    expect(pct(0)).toBe('0.00%')
  })

  it('空值不显示成 +--', () => {
    expect(pct(null)).toBe('--')
    expect(usdt(undefined)).toBe('--')
  })

  it('金额保留 4 位', () => {
    expect(usdt(12.34567)).toBe('+12.3457')
    expect(usdt(-3.5)).toBe('-3.5000')
  })
})

describe('timeStr', () => {
  it('0 视为没有时间', () => {
    expect(timeStr(0)).toBe('--')
  })

  it('按本地时区格式化并用 padStart 补齐', () => {
    const ts = 1_700_000_000 // 2023-11-14T22:13:20Z
    const d = new Date(ts * 1000)
    const pad = (n: number) => String(n).padStart(2, '0')
    const expected =
      `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ` +
      `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
    expect(timeStr(ts)).toBe(expected)
    expect(timeStr(ts, false)).toBe(expected.slice(0, -3))
  })
})

describe('ago', () => {
  const now = () => Math.floor(Date.now() / 1000)

  it('秒 / 分 / 时 / 天 四档', () => {
    expect(ago(now() - 5)).toBe('5s 前')
    expect(ago(now() - 90)).toBe('1m 前')
    expect(ago(now() - 3700)).toBe('1h 前')
    expect(ago(now() - 90000)).toBe('1d 前')
  })

  it('未来时间不会出现负数', () => {
    expect(ago(now() + 100)).toBe('0s 前')
    expect(ago(0)).toBe('--')
  })
})

describe('duration', () => {
  it('按秒/分/时切换单位', () => {
    expect(duration(45)).toBe('45s')
    expect(duration(125)).toBe('2m5s')
    expect(duration(7500)).toBe('2h5m')
  })

  it('缺失与负数降级', () => {
    expect(duration(0)).toBe('--')
    expect(duration(-1)).toBe('--')
  })
})

describe('numClass', () => {
  it('红涨绿跌的类名约定', () => {
    expect(numClass(0.1)).toBe('up')
    expect(numClass(-0.1)).toBe('down')
    expect(numClass(0)).toBe('flat')
    expect(numClass(null)).toBe('flat')
  })
})

describe('标签映射', () => {
  it('平仓原因都有中文名，新增枚举别漏', () => {
    for (const key of ['stop_loss', 'take_profit', 'roi', 'partial_tp', 'trailing_stop', 'timeout', 'signal_flip', 'manual', 'venue_sync']) {
      expect(EXIT_LABELS[key], `缺少 ${key} 的中文标签`).toBeTruthy()
    }
  })

  it('心法分类都有中文名', () => {
    for (const key of ['entry', 'exit', 'risk', 'timing', 'position', 'market']) {
      expect(CATEGORY_LABELS[key], `缺少 ${key} 的中文标签`).toBeTruthy()
    }
  })
})
