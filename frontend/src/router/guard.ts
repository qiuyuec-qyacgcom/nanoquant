/**
 * 路由守卫与旧地址兼容的纯函数版本：不依赖 window/localStorage，便于单测。
 * 路由实例只负责提供当前路径与登录态。
 */

import type { LocationQuery } from 'vue-router'

/** 出厂密码未改时唯一放行的页面：设置工作区里的参数页（含安全分区）。 */
export const PASSWORD_CHANGE_PATH = '/admin/settings/params'

/** 登录后的默认落点。 */
export const ADMIN_HOME = '/admin/overview/dashboard'

export interface GuardInput {
  /** 目标路径（不含 query/hash） */
  path: string
  /** 完整目标地址，用于登录后回跳 */
  fullPath: string
  hasToken: boolean
  mustChangePassword: boolean
}

export interface GuardRedirect {
  path: string
  query?: Record<string, string>
}

/** 旧地址跳转需要保留的最小字段，避免与 vue-router 的完整 RouteLocation 耦合。 */
export interface LegacyLocation {
  query: LocationQuery
  hash: string
}

/**
 * 旧地址兼容：只换 path，保留 query/hash。
 * `?group=engine` 这类页内定位参数不能因为工作区收敛而丢失。
 */
export function legacyRedirect(path: string) {
  return (to: LegacyLocation) => ({ path, query: to.query, hash: to.hash })
}

/** 返回需要跳转的目标；null 表示放行。 */
export function authRedirect(input: GuardInput): GuardRedirect | null {
  const needsAuth = input.path.startsWith('/admin')
  if (needsAuth && !input.hasToken) {
    return { path: '/login', query: { next: input.fullPath } }
  }
  if (input.path === '/login' && input.hasToken) {
    return { path: ADMIN_HOME }
  }
  // 初始密码未改时，后台其余页面即便打开也全是 403，直接送到改密页。
  if (needsAuth && input.mustChangePassword && input.path !== PASSWORD_CHANGE_PATH) {
    return { path: PASSWORD_CHANGE_PATH }
  }
  return null
}
