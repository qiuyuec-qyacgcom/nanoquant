import { describe, expect, it } from 'vitest'
import { WORKSPACES, workspaceById, workspaceByPath, workspaceHome } from './workspaces'

describe('workspaces', () => {
  it('后台收敛为 5 个工作区，页签路径唯一且归属正确', () => {
    expect(WORKSPACES.map((workspace) => workspace.id)).toEqual([
      'overview',
      'trading',
      'research',
      'ai',
      'settings',
    ])
    const paths = WORKSPACES.flatMap((workspace) => workspace.tabs.map((tab) => tab.path))
    const names = WORKSPACES.flatMap((workspace) => workspace.tabs.map((tab) => tab.name))
    expect(new Set(paths).size).toBe(paths.length)
    expect(new Set(names).size).toBe(names.length)
    for (const workspace of WORKSPACES) {
      expect(workspace.tabs.length).toBeGreaterThan(0)
      for (const tab of workspace.tabs) {
        expect(tab.path.startsWith(`${workspace.root}/`)).toBe(true)
      }
    }
  })

  it('按路径定位工作区，工作区根路径也算命中', () => {
    expect(workspaceByPath('/admin/ai/tuning')?.id).toBe('ai')
    expect(workspaceByPath('/admin/ai')?.id).toBe('ai')
    expect(workspaceByPath('/admin/trading/signals')?.id).toBe('trading')
    expect(workspaceByPath('/admin/settings/params')?.id).toBe('settings')
    expect(workspaceByPath('/admin')).toBeUndefined()
    expect(workspaceByPath('/login')).toBeUndefined()
  })

  it('默认落点取第一个页签', () => {
    expect(workspaceHome(workspaceById('overview')!)).toBe('/admin/overview/dashboard')
    expect(workspaceHome(workspaceById('ai')!)).toBe('/admin/ai/models')
    expect(workspaceHome(workspaceById('settings')!)).toBe('/admin/settings/params')
  })
})
