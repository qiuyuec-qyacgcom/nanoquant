import { defineStore } from 'pinia'
import { http, setToken, getToken, getMustChangePassword, setMustChangePassword } from '../api/client'

export const useAuthStore = defineStore('auth', {
  state: () => ({
    token: getToken(),
    username: '',
    mustChangePassword: getMustChangePassword(),
    ready: false,
  }),
  actions: {
    async login(username: string, password: string) {
      const res = await http.post('/auth/login', { username, password })
      this.token = res.token
      this.username = res.username
      this.mustChangePassword = !!res.must_change_password
      setMustChangePassword(this.mustChangePassword)
      setToken(res.token)
      this.ready = true
      return res
    },
    async check() {
      if (!this.token) return false
      try {
        const me = await http.get('/auth/me')
        this.username = me.username
        this.mustChangePassword = !!me.must_change_password
        setMustChangePassword(this.mustChangePassword)
        this.ready = true
        return true
      } catch {
        this.logout()
        return false
      }
    },
    async changePassword(oldPassword: string, newPassword: string) {
      const res = await http.post('/auth/password', { old_password: oldPassword, new_password: newPassword })
      this.token = res.token
      this.mustChangePassword = false
      setMustChangePassword(false)
      setToken(res.token)
    },
    logout() {
      this.token = ''
      this.username = ''
      this.mustChangePassword = false
      setMustChangePassword(false)
      setToken('')
    },
  },
})
