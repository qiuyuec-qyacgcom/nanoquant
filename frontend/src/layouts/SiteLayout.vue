<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useRoute } from 'vue-router'
import { publicConfig, type PublicConfig } from '../api/public'

const cfg = ref<PublicConfig | null>(null)
const error = ref('')
const route = useRoute()

const nav = [
  { path: '/', label: '首页', ico: '⌂' },
  { path: '/charts', label: '行情', ico: '▤' },
  { path: '/insights', label: '观点', ico: '✦' },
  { path: '/about', label: '关于', ico: 'ⓘ' },
]

const title = computed(() => cfg.value?.site_title || 'NanoQuant')
const maintenance = computed(() => cfg.value !== null && !cfg.value.site_enabled)

function isActive(path: string): boolean {
  return path === '/' ? route.path === '/' : route.path.startsWith(path)
}

onMounted(async () => {
  try {
    cfg.value = await publicConfig()
    if (cfg.value?.site_title) document.title = `${cfg.value.site_title} · ${cfg.value.site_tagline}`
  } catch (err: any) {
    error.value = err?.message || '站点信息加载失败'
  }
})
</script>

<template>
  <div class="site">
    <header class="site-header">
      <router-link to="/" class="site-brand">
        <span class="brand-mark">N</span>
        <span class="site-brand-text">
          <b>{{ title }}</b>
          <small>{{ cfg?.site_tagline || 'MICRO CAPITAL QUANT' }}</small>
        </span>
      </router-link>

      <div class="site-actions">
        <span v-if="cfg" class="tag" :class="cfg.mode === 'live' ? 'tag-short' : 'tag-long'">
          {{ cfg.mode === 'live' ? 'LIVE 实盘' : 'DEMO 模拟盘' }}
        </span>
        <router-link to="/admin/dashboard" class="btn btn-sm btn-primary">进入后台</router-link>
      </div>
    </header>

    <div class="site-body">
      <aside class="site-side">
        <div class="side-group">导航</div>
        <router-link
          v-for="item in nav"
          :key="item.path"
          :to="item.path"
          :class="{ active: isActive(item.path) }"
        >
          <span class="side-ico">{{ item.ico }}</span>
          <span>{{ item.label }}</span>
        </router-link>

        <div class="side-group">管理</div>
        <router-link to="/admin/dashboard">
          <span class="side-ico">⚙</span>
          <span>交易后台</span>
        </router-link>

        <div class="side-foot">
          <div v-if="cfg">v{{ cfg.version }} · {{ cfg.mode === 'live' ? '实盘' : '模拟盘' }}</div>
          <div style="margin-top: 4px">数据仅供研究，不构成投资建议</div>
        </div>
      </aside>

      <main class="site-main">
        <div v-if="error" class="note error">
          无法连接服务: {{ error }}。请确认后端已启动（默认 8899 端口）。
        </div>

        <div v-else-if="maintenance" class="panel" style="margin: 40px auto; max-width: 520px">
          <div class="panel-body" style="text-align: center; padding: 32px">
            <div style="font-size: 15px; font-weight: 600; margin-bottom: 8px">公开站点维护中</div>
            <div class="muted" style="font-size: 12px; line-height: 1.8">
              管理员已关闭公开前台，可前往后台继续使用完整功能。
            </div>
            <router-link to="/admin/dashboard" class="btn btn-sm btn-primary" style="margin-top: 16px">进入后台</router-link>
          </div>
        </div>

        <router-view v-else-if="cfg" />

        <div v-else class="empty" style="margin-top: 80px">加载中…</div>
      </main>
    </div>

    <nav class="site-tabbar">
      <router-link v-for="item in nav" :key="item.path" :to="item.path" :class="{ active: isActive(item.path) }">
        <span class="tab-ico">{{ item.ico }}</span>
        <span>{{ item.label }}</span>
      </router-link>
    </nav>

    <footer class="site-footer">
      <div class="row" style="flex-wrap: wrap; gap: 14px">
        <span>© {{ new Date().getFullYear() }} {{ title }}</span>
        <span v-if="cfg">v{{ cfg.version }}</span>
        <span class="spacer" />
        <span>合约杠杆可能造成本金全部损失 · 本站内容仅供研究，不构成投资建议</span>
      </div>
    </footer>
  </div>
</template>
