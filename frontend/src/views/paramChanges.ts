export interface ParamChange {
  id: number
  ts: number
  group: string
  key: string
  old_value: string | null
  new_value: string
  reason: string
  decision_id?: number
  mode?: string
  source?: string
  effect?: { trades?: number; pnl?: number }
}

export type ApiCall = (path: string, init?: RequestInit) => Promise<any>

export async function loadParamChanges(api: ApiCall, limit = 20): Promise<ParamChange[]> {
  const payload = await api(`/settings/parameter-changes?limit=${limit}`)
  return Array.isArray(payload?.items) ? payload.items : []
}

export async function rollbackParamChange(api: ApiCall, id: number): Promise<any> {
  return api(`/settings/parameter-changes/${id}/rollback`, { method: 'POST' })
}
export function rollbackWarning(dirty: boolean, sameGroup: boolean): string {
  if (sameGroup && dirty) return '当前分组有未保存的修改，回滚会覆盖它们。继续？'
  return '确认回滚这次 AI 调参？'
}
