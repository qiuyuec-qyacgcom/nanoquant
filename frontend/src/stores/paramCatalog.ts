import { defineStore } from 'pinia'
import { http } from '../api/client'

export const useParamCatalogStore = defineStore('paramCatalog', {
  state: () => ({
    hidden: {} as Record<string, Set<string>>,
    writable: {} as Record<string, Set<string>>,
    kinds: {} as Record<string, Record<string, string>>,
    failed: false,
    loaded: false,
  }),
  actions: {
    async load() {
      try {
        const response = await http.get<{
          groups: Record<string, { hidden?: string[]; writable?: string[]; meta?: Record<string, { kind?: string }> }>
        }>('/settings/catalog')
        const groups = response?.groups || {}
        const hidden: Record<string, Set<string>> = {}
        const writable: Record<string, Set<string>> = {}
        const kinds: Record<string, Record<string, string>> = {}
        for (const [group, config] of Object.entries(groups)) {
          hidden[group] = new Set(config?.hidden || [])
          writable[group] = new Set(config?.writable || [])
          const groupKinds: Record<string, string> = {}
          for (const [key, meta] of Object.entries(config?.meta || {})) {
            if (meta?.kind) groupKinds[key] = meta.kind
          }
          kinds[group] = groupKinds
        }
        this.hidden = hidden
        this.writable = writable
        this.kinds = kinds
        this.failed = false
        this.loaded = true
      } catch {
        this.hidden = {}
        this.writable = {}
        this.kinds = {}
        this.failed = true
        this.loaded = true
      }
    },
    hiddenFor(group: string): Set<string> {
      return this.hidden[group] || new Set<string>()
    },
    kindFor(group: string, key: string): string | undefined {
      return this.kinds[group]?.[key]
    },
  },
})