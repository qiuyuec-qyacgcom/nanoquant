import { describe, expect, it, vi } from 'vitest'
import { loadParamChanges, rollbackParamChange, rollbackWarning } from './paramChanges'

describe('AI 调参记录', () => {
  it('loads parameter changes and calls rollback', async () => {
    const apiMock = vi
      .fn()
      .mockResolvedValueOnce({
        items: [
          {
            id: 1,
            group: 'strategy',
            key: 'ema_fast',
            old_value: '9',
            new_value: '12',
            reason: 'test',
            ts: 1,
          },
        ],
      })
      .mockResolvedValueOnce({ ok: true })
    const list = await loadParamChanges(apiMock, 20)
    expect(list[0].key).toBe('ema_fast')
    await rollbackParamChange(apiMock, 1)
    expect(apiMock).toHaveBeenCalledWith(
      '/settings/parameter-changes/1/rollback',
      expect.objectContaining({ method: 'POST' }),
    )
  })

  it('returns an empty list when the response has no items', async () => {
    const apiMock = vi.fn().mockResolvedValueOnce({})
    expect(await loadParamChanges(apiMock)).toEqual([])
  })
})

describe('调参记录 UI 辅助', () => {
  it('回滚当前分组且存在未保存修改时给出覆盖警告', () => {
    expect(rollbackWarning(true, true)).toContain('未保存')
    expect(rollbackWarning(false, true)).toContain('确认回滚')
    expect(rollbackWarning(true, false)).toContain('确认回滚')
  })

})