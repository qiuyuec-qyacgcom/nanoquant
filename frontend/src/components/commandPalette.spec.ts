import { describe, expect, it } from 'vitest'
import { filterCommands, type CommandItem } from './commandPalette'

const items: CommandItem[] = [
  { label: '总览', path: '/admin/overview/dashboard' },
  { label: '参数配置', path: '/admin/settings/params' },
  { label: '系统诊断', path: '/admin/overview/system' },
]

describe('filterCommands', () => {
  it('matches Chinese labels and lowercases paths', () => {
    expect(filterCommands(items, '参数')).toEqual([items[1]])
    expect(filterCommands(items, 'DASHBOARD')).toEqual([items[0]])
    expect(filterCommands(items, 'admin/settings')).toEqual([items[1]])
    expect(filterCommands(items, '不存在')).toEqual([])
  })

  it('returns a copy for an empty query', () => {
    const result = filterCommands(items, '  ')
    expect(result).toEqual(items)
    expect(result).not.toBe(items)
  })
})