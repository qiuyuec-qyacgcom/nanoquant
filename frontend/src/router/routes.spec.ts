import { describe, expect, it } from 'vitest'
import type { RouteRecordRaw } from 'vue-router'
import { WORKSPACES, workspaceHome } from '../workspaces'
import { routes } from './routes'

/**
 * 单测只做结构校验，不实例化 router / 不加载 .vue 组件：
 * 真实页面渲染走 Playwright e2e。
 */
const admin = routes.find((route) => route.path === '/admin')
const children: readonly RouteRecordRaw[] = admin?.children || []

function child(segment: string): RouteRecordRaw | undefined {
  return children.find((route) => route.path === segment)
}

function redirectTarget(segment: string, query: Record<string, string> = {}, hash = '') {
  const record = child(segment)
  expect(record, `缺少路由 ${segment}`).toBeTruthy()
  const redirect = record?.redirect
  expect(typeof redirect, `${segment} 必须是重定向`).toBe('function')
  return (redirect as (to: { query: Record<string, string>; hash: string }) => unknown)({
    query,
    hash,
  })
}

describe('后台工作区路由', () => {
  it('每个工作区页签都挂在对应工作区下且路由名一致', () => {
    for (const workspace of WORKSPACES) {
      const record = child(workspace.id)
      expect(record?.meta?.workspace, workspace.id).toBe(workspace.id)
      const home = record?.children?.find((item) => item.path === '')
      expect(home?.redirect, `${workspace.id} 默认落点`).toBe(workspaceHome(workspace))
      for (const tab of workspace.tabs) {
        const segment = tab.path.slice(`${workspace.root}/`.length)
        const matched = record?.children?.find((item) => item.path === segment)
        expect(matched?.name, tab.path).toBe(tab.name)
      }
    }
  })

  it('旧地址兼容跳转保留 query 与 hash', () => {
    expect(redirectTarget('strategy', { group: 'engine' }, '#risk')).toEqual({
      path: '/admin/settings/params',
      query: { group: 'engine' },
      hash: '#risk',
    })
  })

  it('全部旧地址都有工作区落点', () => {
    const expected: Record<string, string> = {
      dashboard: '/admin/overview/dashboard',
      system: '/admin/overview/system',
      onboarding: '/admin/overview/onboarding',
      signals: '/admin/trading/signals',
      symbols: '/admin/trading/symbols',
      strategies: '/admin/research/strategies',
      backtest: '/admin/research/backtest',
      news: '/admin/research/news',
      'ai-tuning': '/admin/ai/tuning',
      'parameter-sets': '/admin/ai/tuning',
      evolution: '/admin/ai/evolution',
      journal: '/admin/ai/journal',
      strategy: '/admin/settings/params',
      backup: '/admin/settings/backup',
      'dead-letters': '/admin/settings/dead-letters',
    }
    for (const [segment, target] of Object.entries(expected)) {
      expect(redirectTarget(segment), segment).toEqual({ path: target, query: {}, hash: '' })
    }
  })
})
