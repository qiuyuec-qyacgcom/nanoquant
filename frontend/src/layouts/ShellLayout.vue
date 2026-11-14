<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { useAppStore } from '../stores/app'
import { useAuthStore } from '../stores/auth'
import { wsUrl, getToken } from '../api/client'
import { fmt, timeStr } from '../utils/format'

const app = useAppStore()
const auth = useAuthStore()
const route = useRoute()
const router = useRouter()

const nav = [
  { group: '交易', items: [
    { path: '/admin/dashboard', label: '总览', ico: '◈' },
    { path: '/admin/onboarding', label: '开仓检查', ico: '⇥' },
    { path: '/admin/signals', label: '信号审计', ico: '▥' },
    { path: '/admin/signal-follow', label: '信号跟单', ico: '⇄' },
    { path: '/admin/strategies', label: 'AI 策略', ico: '⚑' },
    { path: '/admin/symbols', label: '标的池', ico: '☰' },
    { path: '/admin/backtest', label: '回测', ico: '◇' },
  ]},
  { group: '智能', items: [
    { path: '/admin/news', label: '资讯情绪', ico: '✦' },
    { path: '/admin/ai', label: 'AI 模型', ico: '◉' },
    { path: '/admin/evolution', label: '自进化心法', ico: '✧' },
    { path: '/admin/journal', label: '决策经验', ico: '◈' },
  ]},
  { group: '运维', items: [
    { path: '/admin/strategy', label: '参数配置', ico: '⚙' },
    { path: '/admin/backup', label: '备份还原', ico: '⛁' },
    { path: '/admin/dead-letters', label: '落库补偿', ico: '⟳' },
    { path: '/admin/system', label: '系统诊断', ico: '⌁' },
  ]},
]

const title = computed(() => (route.meta.title as string) || 'NanoQuant')
const status = computed(() => app.status)
const equity = computed(() => status.value?.account?.equity ?? 0)
const pnl = computed(() => status.value?.account?.unrealized ?? 0)

/** 移动端抽屉：切换路由后自动收起，避免遮挡内容 */
const navOpen = ref(false)
watch(
  () => route.path,
  () => {
    navOpen.value = false
  },
)
// 抽屉展开时锁定背景滚动，收起/卸载时恢复
watch(navOpen, (open) => {
  document.body.style.overflow = open ? 'hidden' : ''
})

let timer: number | undefined
let ws: WebSocket | null = null
let wsRetry: number | undefined

async function refresh() {
  await app.loadStatus()
}

async function toggleEngine() {
  if (!status.value) return
  try {
    const next = !status.value.enabled
    if (next) {
      const mode = status.value.mode
      if (!confirm(`确认启动引擎? 当前交易环境: ${mode === 'live' ? '实盘' : '模拟盘'}`)) return
    }
    await app.control({ enabled: next })
    app.notify(next ? '引擎已启动' : '引擎已停止', 'ok')
  } catch (err: any) {
    app.notify(err.message || '操作失败', 'error')
  }
}

function connectWs() {
  try {
    ws = new WebSocket(wsUrl())
    ws.onopen = () => {
      // 连接后首帧鉴权，token 不进 URL（避免进 access log / 浏览器历史）
      ws?.send(JSON.stringify({ type: 'auth', token: getToken() }))
    }
    ws.onmessage = (evt) => {
      try {
        const payload = JSON.parse(evt.data)
        if (payload.type === 'event') {
          const e = payload.data
          if (e.category === 'trade') app.notify(e.message, 'ok')
          if (e.level === 'error') app.notify(e.message, 'error')
          refresh()
        }
      } catch { /* ignore */ }
    }
    ws.onclose = () => {
      wsRetry = window.setTimeout(connectWs, 6000)
    }
  } catch {
    wsRetry = window.setTimeout(connectWs, 8000)
  }
}

function logout() {
  auth.logout()
  router.push('/login')
}

onMounted(async () => {
  await auth.check()
  if (!Object.keys(app.settings).length) {
    app.loadSettings().catch(() => undefined)
  }
  await refresh()
  // 隐藏标签页不轮询：多开几个后台 tab 就是成倍请求，还会打交易所状态聚合
  timer = window.setInterval(() => {
    if (!document.hidden) refresh()
  }, 5000)
  connectWs()
})

onBeforeUnmount(() => {
  window.clearInterval(timer)
  window.clearTimeout(wsRetry)
  ws?.close()
  document.body.style.overflow = ''
})
</script>

<template>
  <div class="shell" :class="{ 'nav-open': navOpen }">
    <div v-if="navOpen" class="drawer-mask" @click="navOpen = false" />
    <aside class="sidebar">
      <button class="drawer-close" type="button" aria-label="关闭菜单" @click="navOpen = false">×</button>
      <div class="brand">
        <div class="brand-mark">N</div>
        <div>
          <div class="brand-name">NanoQuant</div>
          <div class="brand-sub">micro capital</div>
        </div>
      </div>
      <nav class="nav">
        <template v-for="g in nav" :key="g.group">
          <div class="nav-group">{{ g.group }}</div>
          <router-link
            v-for="item in g.items"
            :key="item.path"
            :to="item.path"
            class="nav-item"
            :class="{ active: route.path === item.path }"
          >
            <span class="ico">{{ item.ico }}</span>
            <span>{{ item.label }}</span>
          </router-link>
        </template>
      </nav>
      <router-link to="/" class="nav-item" style="margin: 6px 10px 0">
        <span class="ico">⌂</span>
        <span>返回前台</span>
      </router-link>

      <div class="sidebar-foot">
        <div class="row" style="gap: 6px">
          <span class="dot" :class="status?.running ? (status?.enabled ? 'on' : 'off') : 'err'" />
          <span>{{ status?.running ? (status?.enabled ? '引擎运行中' : '引擎待命') : '引擎未运行' }}</span>
        </div>
        <div class="mono" style="margin-top: 6px; font-size: 10px">
          v1.0.0 · {{ status?.mode === 'live' ? '实盘' : '模拟盘' }} · {{ status?.last_cycle_ms || 0 }}ms
        </div>
      </div>
    </aside>

    <div class="main">
      <header class="topbar">
        <button class="nav-toggle" type="button" aria-label="打开菜单" @click="navOpen = true">☰</button>
        <div>
          <div class="page-title">{{ title }}</div>
          <div class="page-sub">小资金 · 币安 U 本位永续 · 超短线</div>
        </div>
        <div class="topbar-right">
          <span class="tag" :class="status?.mode === 'live' ? 'tag-short' : 'tag-long'">
            {{ status?.mode === 'live' ? 'LIVE 实盘' : 'DEMO 模拟盘' }}
          </span>
          <div class="field hide-sm" style="align-items: flex-end; gap: 2px">
            <span class="mono" style="font-size: 14px">{{ fmt(equity, 4) }} USDT</span>
            <span class="mono" style="font-size: 10.5px" :class="pnl >= 0 ? 'up' : 'down'">
              浮盈亏 {{ pnl >= 0 ? '+' : '' }}{{ fmt(pnl, 4) }}
            </span>
          </div>
          <button class="btn" :class="status?.enabled ? 'btn-danger' : 'btn-primary'" :disabled="!status" @click="toggleEngine">
            {{ status?.enabled ? '停止引擎' : '启动引擎' }}
          </button>
          <button class="btn btn-sm btn-ghost" title="退出登录" @click="logout">退出</button>
        </div>
      </header>

      <main class="content">
        <div v-if="auth.mustChangePassword" class="note error" style="margin-bottom: 12px">
          当前仍是初始密码，后台除改密外的操作都已被拒绝。
          <router-link to="/admin/strategy" style="text-decoration: underline">去「参数配置 → 安全」修改密码</router-link>
        </div>
        <div v-if="status?.last_error" class="note error" style="margin-bottom: 12px">
          引擎最近错误: {{ status.last_error }}
        </div>
        <div v-if="status?.halted" class="note warn" style="margin-bottom: 12px">
          风控提示: {{ status.halted }}
        </div>
        <router-view />
        <footer class="muted" style="margin-top: 26px; font-size: 10.5px; text-align: center">
          NanoQuant · 仅用于研究与自担风险的交易 · 合约杠杆可能造成本金全部损失 · 最后心跳
          {{ timeStr(status?.last_cycle_ts || 0) }}
        </footer>
      </main>
    </div>
  </div>
</template>
