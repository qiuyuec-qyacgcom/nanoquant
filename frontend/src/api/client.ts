const BASE = '/api/v1'
const TOKEN_KEY = 'nq_token'

export function getToken(): string {
  return localStorage.getItem(TOKEN_KEY) || ''
}

export function setToken(token: string) {
  if (token) localStorage.setItem(TOKEN_KEY, token)
  else localStorage.removeItem(TOKEN_KEY)
}

export class ApiError extends Error {
  status: number
  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

function detailOf(payload: any): string {
  const detail = payload?.detail
  if (!detail) return '请求失败'
  if (typeof detail === 'string') return detail
  if (Array.isArray(detail)) {
    return detail
      .map((d: any) => {
        const loc = Array.isArray(d.loc) ? d.loc.slice(1).join('.') : ''
        return `${loc ? loc + ': ' : ''}${d.msg || ''}`
      })
      .join('; ')
  }
  return JSON.stringify(detail)
}

export async function api<T = any>(path: string, init: RequestInit = {}): Promise<T> {
  const headers: Record<string, string> = { 'Content-Type': 'application/json', ...(init.headers as any) }
  const token = getToken()
  if (token) headers.Authorization = `Bearer ${token}`
  let res: Response
  try {
    res = await fetch(`${BASE}${path}`, { ...init, headers })
  } catch (err: any) {
    throw new ApiError(0, `网络错误: ${err?.message || '无法连接后端'}`)
  }
  if (res.status === 401) {
    setToken('')
    if (!location.pathname.startsWith('/login')) location.hash = '#/login'
    throw new ApiError(401, '登录已失效')
  }
  const text = await res.text()
  const payload = text ? safeJson(text) : null
  if (!res.ok) throw new ApiError(res.status, detailOf(payload))
  return payload as T
}

function safeJson(text: string) {
  try {
    return JSON.parse(text)
  } catch {
    return { detail: text.slice(0, 200) }
  }
}

export const http = {
  get: <T = any>(path: string) => api<T>(path),
  post: <T = any>(path: string, body?: any) =>
    api<T>(path, { method: 'POST', body: body === undefined ? undefined : JSON.stringify(body) }),
  put: <T = any>(path: string, body?: any) => api<T>(path, { method: 'PUT', body: JSON.stringify(body ?? {}) }),
  patch: <T = any>(path: string, body?: any) => api<T>(path, { method: 'PATCH', body: JSON.stringify(body ?? {}) }),
  del: <T = any>(path: string, body?: any) =>
    api<T>(path, { method: 'DELETE', body: body === undefined ? undefined : JSON.stringify(body) }),
}

export function wsUrl(): string {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws'
  return `${proto}://${location.host}/api/v1/ws?token=${encodeURIComponent(getToken())}`
}
