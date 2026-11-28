import { beforeEach, describe, expect, it, vi } from 'vitest'
import { createPinia, setActivePinia } from 'pinia'

vi.mock('../api/client', () => ({ http: { get: vi.fn() } }))

import { http } from '../api/client'
import { useParamCatalogStore } from './paramCatalog'

const get = vi.mocked(http.get)

describe('paramCatalog', () => {
  beforeEach(() => {
    setActivePinia(createPinia())
    get.mockReset()
  })

  it('loads hidden fields from the catalog', async () => {
    get.mockResolvedValueOnce({ groups: { strategy: { hidden: ['ema_fast'] } } })
    const store = useParamCatalogStore()
    await store.load()
    expect(store.hiddenFor('strategy').has('ema_fast')).toBe(true)
  })

  it('keeps field kinds so numeric strings can be normalized before save', async () => {
    get.mockResolvedValueOnce({
      groups: { risk: { meta: { min_leverage: { kind: 'number' }, proxy: { kind: 'string' } } } },
    })
    const store = useParamCatalogStore()
    await store.load()
    expect(store.kindFor('risk', 'min_leverage')).toBe('number')
    expect(store.kindFor('risk', 'proxy')).toBe('string')
    expect(store.kindFor('risk', 'missing')).toBeUndefined()
  })

  it('keeps hidden empty and marks failure when the request fails', async () => {
    get.mockRejectedValueOnce(new Error('offline'))
    const store = useParamCatalogStore()
    await store.load()
    expect(store.hiddenFor('strategy').size).toBe(0)
    expect(store.failed).toBe(true)
  })
})