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
const showPassword = ref(false)

async function submit() {
  if (loading.value) return
  loading.value = true
  error.value = ''
  try {
    await auth.login(username.value.trim(), password.value)
    app.notify('登录成功', 'ok')
    router.push((route.query.next as string) || '/admin/overview/dashboard')
  } catch (err: any) {
    error.value = err?.message || '登录失败'
  } finally {
    loading.value = false
  }
}
</script>

<template>
  <div class="login-page">
    <div class="login-shell">
      <aside class="login-aside">
        <div class="login-brand">
          <span class="login-mark">N</span>
          <div>
            <b>NanoQuant</b>
            <small>Modular Studio</small>
          </div>
        </div>
        <div class="login-aside-foot">
          <span class="login-version">v1.1.0</span>
        </div>
      </aside>

      <section class="login-panel">
        <header class="login-head">
          <h1>登录工作台</h1>
          <p>使用管理员账号进入工作区</p>
        </header>

        <form @submit.prevent="submit">
          <label class="login-label" for="login-user">账号</label>
          <input
            id="login-user"
            v-model="username"
            class="login-input mono"
            autocomplete="username"
            spellcheck="false"
          />

          <label class="login-label" for="login-password">密码</label>
          <div class="login-secret">
            <input
              id="login-password"
              v-model="password"
              :type="showPassword ? 'text' : 'password'"
              class="login-input mono"
              autocomplete="current-password"
              placeholder="首次登录密码见启动日志"
            />
            <button
              class="login-eye"
              type="button"
              :aria-label="showPassword ? '隐藏密码' : '显示密码'"
              :aria-pressed="showPassword"
              @click="showPassword = !showPassword"
            >
              <svg v-if="showPassword" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true">
                <path d="M3 3l18 18" />
                <path d="M10.6 5.1A10.9 10.9 0 0 1 12 5c6.5 0 10 7 10 7a17.7 17.7 0 0 1-3.2 4.1" />
                <path d="M6.6 6.6A17.6 17.6 0 0 0 2 12s3.5 7 10 7a10.7 10.7 0 0 0 4.1-.8" />
                <path d="M9.9 9.9a3 3 0 0 0 4.2 4.2" />
              </svg>
              <svg v-else viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" aria-hidden="true">
                <path d="M2 12s3.5-6 10-6 10 6 10 6-3.5 6-10 6S2 12 2 12Z" />
                <circle cx="12" cy="12" r="3" />
              </svg>
            </button>
          </div>

          <div v-if="error" class="login-error">{{ error }}</div>

          <button class="login-submit" type="submit" :disabled="loading">
            <span v-if="loading" class="login-spinner" aria-hidden="true" />
            <span>{{ loading ? '正在验证' : '进入工作台' }}</span>
            <svg v-if="!loading" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
              <path d="M5 12h14M13 6l6 6-6 6" />
            </svg>
          </button>
        </form>

        <footer class="login-foot">
          <router-link to="/">返回公开站点</router-link>
          <span>首次登录需修改密码</span>
        </footer>
      </section>
    </div>
  </div>
</template>

<style scoped>
.login-page {
  display: grid;
  place-items: center;
  min-height: 100vh;
  padding: 24px;
  background: var(--bg);
  color: var(--text);
}

.login-shell {
  display: grid;
  grid-template-columns: 232px minmax(0, 1fr);
  width: min(820px, 100%);
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 16px;
  box-shadow: var(--shadow-2);
  overflow: hidden;
}

.login-aside {
  display: flex;
  flex-direction: column;
  justify-content: space-between;
  gap: 20px;
  padding: 24px;
  background: var(--panel-2);
  border-right: 1px solid var(--line);
}

.login-brand {
  display: flex;
  align-items: center;
  gap: 10px;
}
.login-mark {
  display: grid;
  place-items: center;
  width: 32px;
  height: 32px;
  border-radius: var(--r-md);
  background: var(--accent);
  color: var(--on-accent);
  font-family: var(--mono);
  font-size: 15px;
  font-weight: 600;
}
.login-brand b {
  display: block;
  font-size: 14px;
  font-weight: 600;
  letter-spacing: -0.01em;
}
.login-brand small {
  display: block;
  margin-top: 2px;
  color: var(--muted);
  font-family: var(--mono);
  font-size: 9px;
  letter-spacing: 0.12em;
  text-transform: uppercase;
}

.login-aside-foot {
  display: flex;
  align-items: center;
  gap: 8px;
}
.login-version {
  color: var(--dim);
  font-family: var(--mono);
  font-size: 10px;
}

.login-panel {
  min-width: 0;
  padding: 30px 32px 26px;
}

.login-head h1 {
  margin: 0;
  font-size: 20px;
  font-weight: 600;
  letter-spacing: -0.02em;
}
.login-head p {
  margin: 4px 0 0;
  color: var(--muted);
  font-size: 11.5px;
}

.login-label {
  display: block;
  margin: 16px 0 6px;
  color: var(--muted);
  font-size: 11px;
}

.login-input {
  width: 100%;
  height: 42px;
  padding: 0 12px;
  background: var(--panel-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  color: var(--text);
  font-size: 12px;
  outline: none;
  transition: border-color var(--dur-1) var(--ease), box-shadow var(--dur-1) var(--ease);
}
.login-input::placeholder { color: var(--dim); }
.login-input:hover { border-color: var(--line-strong); }
.login-input:focus {
  border-color: var(--accent);
  box-shadow: var(--focus-ring);
}

.login-secret { position: relative; }
.login-secret .login-input { padding-right: 46px; }
.login-eye {
  position: absolute;
  top: 50%;
  right: 6px;
  transform: translateY(-50%);
  display: grid;
  place-items: center;
  width: 32px;
  height: 32px;
  border-radius: var(--r-sm);
  color: var(--muted);
  cursor: pointer;
}
.login-eye svg { width: 16px; height: 16px; }
.login-eye:hover { color: var(--text); background: var(--panel-3); }
.login-eye:focus-visible { outline: none; box-shadow: var(--focus-ring); }

.login-error {
  margin-top: 12px;
  padding: 9px 11px;
  border: 1px solid var(--down);
  border-radius: var(--r-sm);
  background: var(--down-soft);
  color: var(--down);
  font-size: 11px;
}

.login-submit {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 8px;
  width: 100%;
  height: 44px;
  margin-top: 18px;
  border-radius: var(--r-sm);
  background: var(--accent);
  color: var(--on-accent);
  font-size: 12.5px;
  font-weight: 600;
  cursor: pointer;
  transition: filter var(--dur-1) var(--ease), transform var(--dur-1) var(--ease);
}
.login-submit:hover:not(:disabled) { filter: brightness(1.06); }
.login-submit:active:not(:disabled) { transform: translateY(1px); }
.login-submit:focus-visible { outline: none; box-shadow: var(--focus-ring); }
.login-submit:disabled { opacity: 0.65; cursor: wait; }
.login-submit svg { width: 16px; height: 16px; }

.login-spinner {
  width: 14px;
  height: 14px;
  border: 2px solid currentColor;
  border-right-color: transparent;
  border-radius: 50%;
  animation: login-spin 0.7s linear infinite;
}
@keyframes login-spin {
  to { transform: rotate(360deg); }
}

.login-foot {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  margin-top: 20px;
  padding-top: 14px;
  border-top: 1px solid var(--line);
  color: var(--muted);
  font-size: 10.5px;
}
.login-foot a { color: var(--muted); }
.login-foot a:hover { color: var(--text); }

@media (max-width: 720px) {
  .login-page { padding: 16px; }
  .login-shell { grid-template-columns: 1fr; }
  .login-aside {
    flex-direction: row;
    align-items: center;
    padding: 16px 18px;
    border-right: 0;
    border-bottom: 1px solid var(--line);
  }
  .login-aside-foot { gap: 12px; }
  .login-panel { padding: 22px 20px 20px; }
  .login-head h1 { font-size: 18px; }
}

@media (prefers-reduced-motion: reduce) {
  .login-submit { transition: none; }
  .login-spinner { animation: none; }
}
</style>
