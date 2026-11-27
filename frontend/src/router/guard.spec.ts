import { describe, expect, it } from 'vitest'
import {
  ADMIN_HOME,
  PASSWORD_CHANGE_PATH,
  authRedirect,
  legacyRedirect,
  type GuardInput,
} from './guard'

function input(patch: Partial<GuardInput>): GuardInput {
  return {
    path: '/admin/overview/dashboard',
    fullPath: '/admin/overview/dashboard',
    hasToken: true,
    mustChangePassword: false,
    ...patch,
  }
}

describe('authRedirect', () => {
  it('未登录访问后台时带上回跳地址去登录', () => {
    expect(
      authRedirect(
        input({ path: '/admin/trading/signals', fullPath: '/admin/trading/signals?x=1', hasToken: false }),
      ),
    ).toEqual({ path: '/login', query: { next: '/admin/trading/signals?x=1' } })
  })

  it('已登录访问登录页时回到默认工作区', () => {
    expect(authRedirect(input({ path: '/login', fullPath: '/login' }))).toEqual({ path: ADMIN_HOME })
  })

  it('出厂密码未改时只放行参数页', () => {
    expect(authRedirect(input({ mustChangePassword: true }))).toEqual({ path: PASSWORD_CHANGE_PATH })
    expect(
      authRedirect(input({ path: PASSWORD_CHANGE_PATH, mustChangePassword: true })),
    ).toBeNull()
  })

  it('公开页面与正常后台访问都放行', () => {
    expect(authRedirect(input({ path: '/', hasToken: false }))).toBeNull()
    expect(authRedirect(input({ path: '/admin/ai/tuning' }))).toBeNull()
  })
})

describe('legacyRedirect', () => {
  it('只换路径，保留页内定位参数与 hash', () => {
    const redirect = legacyRedirect('/admin/settings/params')
    expect(redirect({ query: { group: 'engine' }, hash: '#risk' })).toEqual({
      path: '/admin/settings/params',
      query: { group: 'engine' },
      hash: '#risk',
    })
  })
})
