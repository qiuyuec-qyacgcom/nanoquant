<script setup lang="ts">
import { ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useAuthStore } from '../stores/auth'
import { useAppStore } from '../stores/app'

const auth = useAuthStore()
const app = useAppStore()
const router = useRouter()
const route = useRoute()

const username = ref('admin')
const password = ref('')
const loading = ref(false)
const error = ref('')

async function submit() {
  if (loading.value) return
  loading.value = true
  error.value = ''
  try {
    await auth.login(username.value.trim(), password.value)
    app.notify('登录成功', 'ok')
    router.push((route.query.next as string) || '/admin/dashboard')
  } catch (err: any) {
    error.value = err?.message || '登录失败'
  } finally {
    loading.value = false
  }
}
</script>

<template>
  <div class="login-wrap">
    <div class="login-card">
      <div class="row" style="gap: 11px; margin-bottom: 22px">
        <div class="brand-mark" style="width: 38px; height: 38px; font-size: 18px">N</div>
        <div>
          <div style="font-size: 18px; font-weight: 700; letter-spacing: 0.5px">NanoQuant</div>
          <div class="brand-sub">micro capital quant terminal</div>
        </div>
      </div>

      <form class="stack" @submit.prevent="submit">
        <div class="field">
          <span class="label">管理员账号</span>
          <input v-model="username" class="input" autocomplete="username" />
        </div>
        <div class="field">
          <span class="label">密码</span>
          <input v-model="password" type="password" class="input" autocomplete="current-password" placeholder="首次登录密码见启动日志" />
        </div>
        <div v-if="error" class="note error">{{ error }}</div>
        <button class="btn btn-primary" type="submit" :disabled="loading" style="justify-content: center; padding: 10px">
          {{ loading ? '验证中…' : '进入后台' }}
        </button>
      </form>

      <div style="margin-top: 14px">
        <router-link to="/" class="hint">← 返回前台首页</router-link>
      </div>

      <div class="hint" style="margin-top: 18px; line-height: 1.8">
        管理员账号 <span class="mono">admin</span>，初始密码在首次启动时随机生成，
        见控制台或 <span class="mono">logs/nanoquant.log</span>（安装时也可指定）。<br />
        首次登录后必须改密；忘记密码用 <span class="mono">python scripts/reset_password.py</span> 重置。<br />
        系统默认以模拟盘模式运行，接入交易所密钥并显式切换后才会真实下单。
      </div>
    </div>
  </div>
</template>
