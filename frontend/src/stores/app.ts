import { defineStore } from 'pinia'
import { http } from '../api/client'
import type { EngineStatus, EventItem } from '../types'

export const useAppStore = defineStore('app', {
  state: () => ({
    settings: {} as Record<string, any>,
    defaults: {} as Record<string, any>,
    status: null as EngineStatus | null,
    events: [] as EventItem[],
    toast: { message: '', type: 'info', visible: false },
    loading: false,
  }),
  actions: {
    notify(message: string, type: 'info' | 'ok' | 'error' = 'info') {
      this.toast = { message, type, visible: true }
      window.clearTimeout((this as any)._toastTimer)
      ;(this as any)._toastTimer = window.setTimeout(() => (this.toast.visible = false), 3600)
    },
    async loadSettings() {
      const res = await http.get('/settings')
      this.settings = res.groups
      this.defaults = res.defaults
    },
    async saveGroup(group: string, patch: Record<string, any>) {
      const res = await http.put(`/settings/${group}`, { patch })
      this.settings[group] = res.data
      return res.data
    },
    async resetGroup(group: string) {
      const res = await http.post(`/settings/${group}/reset`)
      this.settings[group] = res.data
      return res.data
    },
    async loadStatus() {
      try {
        this.status = await http.get('/engine/status')
      } catch (err) {
        /* 状态接口失败不阻塞页面 */
      }
    },
    async loadEvents(limit = 60) {
      const res = await http.get(`/system/events?limit=${limit}`)
      this.events = res.items
    },
    async control(payload: { enabled?: boolean; mode?: string; confirm?: string }) {
      const res = await http.post('/engine/control', payload)
      this.status = res.status
      return res
    },
  },
})
