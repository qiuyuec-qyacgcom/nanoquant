import { describe, expect, it, vi } from 'vitest'
import { loadParamChanges, rollbackParamChange } from './paramChanges'

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