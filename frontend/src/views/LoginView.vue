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
  <div class="login-page">
    <section class="login-visual">
      <div class="login-brand">
        <div class="brand-mark">N</div>
        <div>
          <div class="login-brand-name">NanoQuant</div>
          <div class="login-brand-sub">systematic micro capital</div>
        </div>
      </div>

      <div class="login-quote">
        <div class="eyebrow">PRIVATE TERMINAL / 2026</div>
        <h1>先活下来，<br />再谈<em>收益</em>。</h1>
        <p>策略只提出候选，风控决定是否允许下单。每一次进出、每一次参数变更，都写入同一本审计账本。</p>
      </div>

      <div class="login-stats">
        <div><b>5–30m</b><span>主执行周期</span></div>
        <div><b>2.41</b><span>Sharpe / 90D</span></div>
        <div><b>0600</b><span>UDS / 执行侧车</span></div>
      </div>
    </section>

    <section class="login-panel">
      <div class="login-form">
        <div class="eyebrow">SECURE ACCESS</div>
        <h2>登录交易终端</h2>
        <p class="login-lead">仅限授权操作员。所有操作会记录到审计日志。</p>

        <form @submit.prevent="submit">
          <label class="login-label" for="login-user">账号</label>
          <input id="login-user" v-model="username" class="login-input mono" autocomplete="username" />

          <label class="login-label" for="login-password">密码</label>
          <input
            id="login-password"
            v-model="password"
            type="password"
            class="login-input mono"
            autocomplete="current-password"
            placeholder="首次登录密码见启动日志"
          />

          <div v-if="error" class="login-error">{{ error }}</div>
          <button class="login-submit" type="submit" :disabled="loading">
            {{ loading ? '验证中…' : '进入终端' }} <span>→</span>
          </button>
        </form>

        <div class="login-hint">
          首次登录或重置后，系统会要求修改默认密码。不要在公共网络保存凭证。
        </div>
        <div class="login-security"><i></i> TLS · 会话令牌 · 登录失败锁定</div>
        <div class="login-links">
          <router-link to="/">← 返回公开站点</router-link>
          <span>v1.0.0</span>
        </div>
      </div>
    </section>
  </div>
</template>

<style scoped>
.login-page {
  min-height: 100vh;
  display: grid;
  grid-template-columns: 57% 43%;
  background: #07080a;
  color: var(--text);
  overflow: hidden;
}

.login-visual {
  position: relative;
  padding: 46px 54px;
  background:
    radial-gradient(circle at 78% 20%, rgba(240, 185, 11, 0.2), transparent 32%),
    radial-gradient(circle at 20% 80%, rgba(240, 185, 11, 0.06), transparent 35%),
    #08090b;
  overflow: hidden;
}

.login-visual::before {
  content: '';
  position: absolute;
  inset: 0;
  background-image: linear-gradient(rgba(240, 185, 11, 0.045) 1px, transparent 1px),
    linear-gradient(90deg, rgba(240, 185, 11, 0.045) 1px, transparent 1px);
  background-size: 52px 52px;
}

.login-visual::after {
  content: '';
  position: absolute;
  right: -260px;
  bottom: -280px;
  width: 620px;
  height: 620px;
  border: 1px solid rgba(240, 185, 11, 0.12);
  border-radius: 50%;
  box-shadow: 0 0 0 70px rgba(240, 185, 11, 0.025), 0 0 0 140px rgba(240, 185, 11, 0.018);
}

.login-brand,
.login-quote,
.login-stats {
  position: relative;
  z-index: 1;
}

.login-brand {
  display: flex;
  align-items: center;
  gap: 11px;
}

.login-brand .brand-mark {
  width: 30px;
  height: 30px;
  font-size: 14px;
}

.login-brand-name {
  font-family: var(--serif);
  font-size: 18px;
}

.login-brand-sub {
  color: #7e8189;
  font-family: var(--mono);
  font-size: 8px;
  letter-spacing: 0.15em;
  text-transform: uppercase;
  margin-top: 2px;
}

.login-quote {
  margin-top: 150px;
  max-width: 560px;
}

.login-quote .eyebrow {
  color: var(--accent);
  font-size: 10px;
  letter-spacing: 0.18em;
}

.login-quote h1 {
  margin: 17px 0 0;
  font-family: var(--serif);
  font-size: 48px;
  line-height: 1.08;
  font-weight: 400;
  letter-spacing: -0.045em;
}

.login-quote h1 em {
  color: var(--accent);
  font-style: italic;
}

.login-quote p {
  max-width: 440px;
  margin: 19px 0 0;
  color: #8f929a;
  font-size: 11px;
  line-height: 1.9;
}

.login-stats {
  position: absolute;
  left: 54px;
  right: 54px;
  bottom: 46px;
  display: flex;
  border-top: 1px solid rgba(255, 255, 255, 0.1);
}

.login-stats > div {
  flex: 1;
  padding: 14px 0 0 18px;
  border-left: 1px solid rgba(255, 255, 255, 0.08);
}

.login-stats > div:first-child {
  border-left: 0;
  padding-left: 0;
}

.login-stats b {
  display: block;
  font-family: var(--mono);
  font-size: 17px;
  font-weight: 500;
}

.login-stats span {
  display: block;
  margin-top: 5px;
  color: #656870;
  font-family: var(--mono);
  font-size: 8px;
  letter-spacing: 0.12em;
  text-transform: uppercase;
}

.login-panel {
  position: relative;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 60px 80px;
  background: #f1ede4;
  color: #151619;
}

.login-panel::before {
  content: '';
  position: absolute;
  left: 0;
  top: 0;
  bottom: 0;
  width: 1px;
  background: linear-gradient(180deg, transparent, var(--accent), transparent);
  opacity: 0.55;
}

.login-form {
  width: 100%;
  max-width: 370px;
  margin-top: -30px;
}

.login-form .eyebrow {
  color: #9a7a1d;
  font-size: 10px;
  letter-spacing: 0.18em;
}

.login-form h2 {
  margin: 13px 0 0;
  font-family: var(--serif);
  font-size: 34px;
  font-weight: 400;
  letter-spacing: -0.04em;
}

.login-lead {
  color: #7c7870;
  font-size: 11px;
  margin: 8px 0 0;
}

.login-form form {
  display: grid;
  gap: 6px;
  margin-top: 30px;
}

.login-label {
  color: #4f4c47;
  font-size: 10px;
  margin-top: 8px;
}

.login-input {
  width: 100%;
  height: 42px;
  padding: 0 13px;
  border: 1px solid rgba(17, 17, 17, 0.16);
  border-radius: 2px;
  background: rgba(255, 255, 255, 0.35);
  color: #17181b;
  font-size: 11px;
  outline: none;
}

.login-input:focus {
  border-color: var(--accent);
}

.login-error {
  margin-top: 10px;
  padding: 8px 10px;
  border: 1px solid rgba(214, 69, 69, 0.28);
  background: rgba(214, 69, 69, 0.06);
  color: #a32929;
  font-size: 11px;
}

.login-submit {
  width: 100%;
  height: 43px;
  margin-top: 10px;
  border: 0;
  border-radius: 2px;
  background: #111;
  color: #f6f1e5;
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 0.02em;
}

.login-submit span {
  color: var(--accent);
  margin-left: 8px;
}

.login-submit:disabled {
  opacity: 0.6;
}

.login-hint {
  color: #8e897f;
  font-size: 9px;
  line-height: 1.7;
  margin-top: 14px;
}

.login-security {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-top: 22px;
  padding-top: 14px;
  border-top: 1px solid rgba(17, 17, 17, 0.1);
  color: #8c867c;
  font-family: var(--mono);
  font-size: 8px;
  letter-spacing: 0.04em;
}

.login-security i {
  width: 5px;
  height: 5px;
  border-radius: 50%;
  background: var(--up);
}

.login-links {
  display: flex;
  justify-content: space-between;
  margin-top: 42px;
  color: #9b958a;
  font-family: var(--mono);
  font-size: 8px;
  letter-spacing: 0.05em;
}

.login-links a {
  color: #7c5a00;
}

@media (max-width: 900px) {
  .login-page {
    grid-template-columns: 1fr;
    min-height: 100vh;
  }

  .login-visual {
    min-height: 260px;
    padding: 24px 24px 20px;
  }

  .login-quote {
    margin-top: 34px;
  }

  .login-quote h1 {
    font-size: 34px;
  }

  .login-quote p {
    font-size: 10px;
  }

  .login-stats {
    display: none;
  }

  .login-panel {
    padding: 34px 24px 44px;
  }

  .login-form {
    margin-top: 0;
  }
}
</style>