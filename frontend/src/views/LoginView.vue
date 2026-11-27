<script setup lang="ts">
import { ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useAuthStore } from '../stores/auth'
import { useAppStore } from '../stores/app'
import { useTheme } from '../composables/theme'

const auth = useAuthStore()
const app = useAppStore()
const router = useRouter()
const route = useRoute()
const { theme, toggle: toggleTheme } = useTheme()

const username = ref('admin')
const password = ref('')
const loading = ref(false)
const error = ref('')
const showPassword = ref(false)

const flow = [
  { k: '01', title: '信号', desc: '量化评分' },
  { k: '02', title: '风控', desc: '硬闸门' },
  { k: '03', title: 'AI 复核', desc: '同意 / 否决' },
  { k: '04', title: '执行', desc: '下单与保护' },
]

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
  <div class="gate">
    <header class="gate-head">
      <div class="gate-brand">
        <span class="gate-mark">N</span>
        <div>
          <b>NanoQuant</b>
          <small>MODULAR STUDIO</small>
        </div>
      </div>
      <button class="btn btn-sm btn-ghost" type="button" @click="toggleTheme">
        {{ theme === 'dark' ? '浅色' : '深色' }}
      </button>
    </header>

    <main class="gate-main">
      <section class="gate-copy">
        <span class="eyebrow">PRIVATE WORKSPACE / 2026</span>
        <h1>把每一次决策<br />放在<em>同一张桌面</em>上。</h1>
        <p class="gate-lead">
          信号、风控、AI 复核与执行共用一套数据与审计链路。
          每一笔开仓、每一次拒绝、每一个参数变更，都能回到同一处查证。
        </p>

        <div class="bento">
          <article class="bcard wide">
            <div class="bcard-head">
              <span>执行链路</span>
              <span class="tag tag-dim">实时</span>
            </div>
            <div class="flow">
              <div v-for="step in flow" :key="step.k" class="flow-step">
                <b>{{ step.k }}</b>
                <span>{{ step.title }}</span>
                <small>{{ step.desc }}</small>
              </div>
            </div>
          </article>

          <article class="bcard">
            <span class="bcard-k">环境</span>
            <b class="bcard-v">DEMO</b>
            <span class="bcard-s">模拟盘 · 币安测试网</span>
          </article>
          <article class="bcard">
            <span class="bcard-k">审计</span>
            <b class="bcard-v">全链路</b>
            <span class="bcard-s">决策 ID · 可追溯</span>
          </article>
          <article class="bcard">
            <span class="bcard-k">访问</span>
            <b class="bcard-v">会话令牌</b>
            <span class="bcard-s">失败锁定 · 审计</span>
          </article>
          <article class="bcard">
            <span class="bcard-k">AI 复核</span>
            <b class="bcard-v">预算内</b>
            <span class="bcard-s">超时自动降级</span>
          </article>
        </div>
      </section>

      <section class="gate-auth">
        <div class="auth-card">
          <div class="auth-head">
            <span class="auth-mark">N</span>
            <div>
              <h2>登录工作台</h2>
              <p>仅限授权操作员</p>
            </div>
          </div>

          <form @submit.prevent="submit">
            <label class="auth-label" for="login-user">账号</label>
            <input
              id="login-user"
              v-model="username"
              class="auth-input mono"
              autocomplete="username"
              spellcheck="false"
            />

            <label class="auth-label" for="login-password">密码</label>
            <div class="auth-secret">
              <input
                id="login-password"
                v-model="password"
                :type="showPassword ? 'text' : 'password'"
                class="auth-input mono"
                autocomplete="current-password"
                placeholder="首次登录密码见启动日志"
              />
              <button class="auth-eye" type="button" @click="showPassword = !showPassword">
                {{ showPassword ? '隐藏' : '显示' }}
              </button>
            </div>

            <div v-if="error" class="auth-error">{{ error }}</div>

            <button class="auth-submit" type="submit" :disabled="loading">
              {{ loading ? '验证中…' : '进入工作台' }}
              <span aria-hidden="true">→</span>
            </button>
          </form>

          <div class="auth-meta">
            <span>首次登录需修改密码</span>
            <span>v1.1.0</span>
          </div>
          <router-link to="/" class="auth-back">← 返回公开站点</router-link>
        </div>
        <p class="auth-foot">TLS · 会话令牌 · 登录失败锁定 · 全部操作写入审计日志</p>
      </section>
    </main>
  </div>
</template>

<style scoped>
.gate {
  position: relative;
  display: flex;
  flex-direction: column;
  min-height: 100vh;
  max-width: 1280px;
  margin: 0 auto;
  padding: 24px 32px 28px;
  color: var(--text);
  overflow: hidden;
}

.gate::before {
  content: '';
  position: absolute;
  inset: -240px 40% auto -240px;
  width: 620px;
  height: 620px;
  border-radius: 50%;
  background: radial-gradient(circle, color-mix(in srgb, var(--accent) 10%, transparent), transparent 68%);
  pointer-events: none;
}

.gate-head {
  position: relative;
  z-index: 1;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
}

.gate-brand {
  display: flex;
  align-items: center;
  gap: 10px;
}
.gate-mark,
.auth-mark {
  display: grid;
  place-items: center;
  width: 30px;
  height: 30px;
  border-radius: var(--r-md);
  background: var(--accent);
  color: var(--on-accent);
  font-family: var(--mono);
  font-size: 14px;
  font-weight: 600;
}
.gate-brand b {
  display: block;
  font-size: 15px;
  font-weight: 600;
  letter-spacing: -0.01em;
}
.gate-brand small {
  display: block;
  color: var(--muted);
  font-family: var(--mono);
  font-size: 9px;
  letter-spacing: 0.16em;
}

.gate-main {
  position: relative;
  z-index: 1;
  flex: 1;
  display: grid;
  grid-template-columns: minmax(0, 1.15fr) minmax(360px, 0.85fr);
  align-items: center;
  gap: 48px;
  padding: 40px 0 24px;
}

.gate-copy { min-width: 0; }
.eyebrow {
  color: var(--accent);
  font-family: var(--mono);
  font-size: 10px;
  letter-spacing: 0.16em;
}
.gate-copy h1 {
  margin: 14px 0 0;
  font-size: clamp(30px, 3.4vw, 44px);
  line-height: 1.16;
  font-weight: 600;
  letter-spacing: -0.035em;
}
.gate-copy h1 em {
  font-style: normal;
  color: var(--accent);
}
.gate-lead {
  max-width: 52ch;
  margin: 16px 0 0;
  color: var(--muted);
  font-size: 13px;
  line-height: 1.8;
}

.bento {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 10px;
  margin-top: 26px;
}
.bcard {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
  padding: 14px 16px;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--r-lg);
}
.bcard.wide { grid-column: span 2; }
.bcard-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  margin-bottom: 10px;
  font-size: 12px;
  font-weight: 500;
}
.bcard-k {
  color: var(--muted);
  font-size: 11px;
}
.bcard-v {
  font-family: var(--mono);
  font-size: 16px;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
}
.bcard-s {
  color: var(--muted);
  font-size: 10.5px;
}

.flow {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 8px;
}
.flow-step {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding-left: 12px;
  border-left: 1px solid var(--line);
}
.flow-step:first-child { border-left: 0; padding-left: 0; }
.flow-step b {
  color: var(--accent);
  font-family: var(--mono);
  font-size: 10px;
  font-weight: 500;
}
.flow-step span { font-size: 12px; font-weight: 500; }
.flow-step small { color: var(--muted); font-size: 10px; }

.gate-auth { min-width: 0; }

.auth-card {
  padding: 24px;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: 16px;
  box-shadow: var(--shadow-2);
}
.auth-head {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-bottom: 20px;
}
.auth-head h2 {
  margin: 0;
  font-size: 18px;
  font-weight: 600;
  letter-spacing: -0.02em;
}
.auth-head p {
  margin: 2px 0 0;
  color: var(--muted);
  font-size: 11px;
}

.auth-label {
  display: block;
  margin: 14px 0 6px;
  color: var(--muted);
  font-size: 11px;
}
.auth-input {
  width: 100%;
  height: 40px;
  padding: 0 12px;
  background: var(--panel-2);
  border: 1px solid var(--line);
  border-radius: var(--r-sm);
  color: var(--text);
  font-size: 12px;
  outline: none;
}
.auth-input::placeholder { color: var(--dim); }
.auth-input:focus {
  border-color: var(--accent);
  box-shadow: var(--focus-ring);
}

.auth-secret { position: relative; }
.auth-secret .auth-input { padding-right: 58px; }
.auth-eye {
  position: absolute;
  top: 50%;
  right: 6px;
  transform: translateY(-50%);
  padding: 5px 8px;
  border-radius: 6px;
  color: var(--muted);
  font-size: 10px;
}
.auth-eye:hover { color: var(--text); background: var(--panel-3); }

.auth-error {
  margin-top: 12px;
  padding: 9px 11px;
  border: 1px solid var(--down);
  border-radius: var(--r-sm);
  background: var(--down-soft);
  color: var(--down);
  font-size: 11px;
}

.auth-submit {
  display: flex;
  align-items: center;
  justify-content: space-between;
  width: 100%;
  height: 42px;
  margin-top: 18px;
  padding: 0 14px;
  border-radius: var(--r-sm);
  background: var(--accent);
  color: var(--on-accent);
  font-size: 12px;
  font-weight: 600;
  transition: filter var(--dur-1) var(--ease);
}
.auth-submit:hover:not(:disabled) { filter: brightness(1.05); }
.auth-submit:disabled { opacity: 0.6; cursor: wait; }
.auth-submit span { font-family: var(--mono); }

.auth-meta {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  margin-top: 16px;
  padding-top: 14px;
  border-top: 1px solid var(--line);
  color: var(--muted);
  font-family: var(--mono);
  font-size: 10px;
}
.auth-back {
  display: inline-block;
  margin-top: 14px;
  color: var(--muted);
  font-size: 11px;
}
.auth-back:hover { color: var(--text); }
.auth-foot {
  margin: 12px 0 0;
  color: var(--dim);
  font-size: 10px;
  text-align: center;
}

@media (max-width: 980px) {
  .gate-main {
    grid-template-columns: 1fr;
    gap: 28px;
    padding-top: 24px;
  }
  .gate-auth { order: -1; }
  .gate-copy h1 { font-size: 30px; }
}

@media (max-width: 620px) {
  .gate { padding: 16px; }
  .gate-main { padding-top: 18px; }
  .gate-copy h1 { font-size: 26px; }
  .bento { grid-template-columns: 1fr; }
  .bcard.wide { grid-column: auto; }
  .flow { grid-template-columns: repeat(2, minmax(0, 1fr)); }
  .flow-step:nth-child(3) { border-left: 0; padding-left: 0; }
  .auth-card { padding: 20px; }
  .gate-lead { font-size: 12px; }
}

@media (prefers-reduced-motion: reduce) {
  .auth-submit { transition: none; }
}
</style>
