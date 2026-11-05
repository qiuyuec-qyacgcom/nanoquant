<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { http } from '../api/client'
import Modal from '../components/Modal.vue'
import { useAppStore } from '../stores/app'
import type { NewsItem } from '../types'
import { ago, numClass, timeStr } from '../utils/format'

interface SourceStat {
  id: string
  label: string
  enabled: boolean
  ok: boolean
  count: number
  error: string
  latency_ms: number
  needs_proxy: boolean
}

interface DiagRow {
  source: string
  label: string
  enabled: boolean
  ok: boolean
  count: number
  latency_ms: number
  error: string
  url: string
  hint: string
}

const app = useAppStore()
const items = ref<NewsItem[]>([])
const overview = ref<any>(null)
const sources = ref<SourceStat[]>([])
const activeSource = ref('')
const loading = ref(false)
const aiResult = ref<any>(null)
const aiBusy = ref(false)
const diag = ref<any>(null)
const diagOpen = ref(false)
const checking = ref(false)

const configured = computed(() => sources.value.filter((s) => s.enabled))
const enabledCount = computed(() => configured.value.filter((s) => s.ok).length)
const failedSources = computed(() => configured.value.filter((s) => !s.ok && s.error))
const filtered = computed(() => (activeSource.value ? items.value.filter((i) => i.source === activeSource.value) : items.value))
const sentimentPct = computed(() => Math.round(((overview.value?.sentiment_index ?? 0) + 1) * 50))
const sentimentLabel = computed(() => {
  const v = overview.value?.sentiment_index ?? 0
  if (v > 0.15) return '偏多'
  if (v < -0.15) return '偏空'
  return '中性'
})

async function load() {
  loading.value = true
  try {
    const [list, ov] = await Promise.all([http.get('/news?limit=100'), http.get('/news/overview')])
    items.value = list.items
    sources.value = list.sources
    overview.value = ov
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    loading.value = false
  }
}

async function refresh() {
  loading.value = true
  try {
    const res = await http.post('/news/refresh')
    if (res.skipped) {
      app.notify('刷新过于频繁, 已跳过', 'info')
    } else if (res.failed?.length) {
      app.notify(`新增 ${res.inserted} 条; ${res.failed.length} 个源失败, 可点「源检测」查看原因`, 'info')
    } else {
      app.notify(`刷新完成, 新增 ${res.inserted} 条`, 'ok')
    }
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    loading.value = false
  }
}

async function checkSources() {
  diagOpen.value = true
  checking.value = true
  diag.value = null
  try {
    diag.value = await http.get('/news/diagnostics')
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    checking.value = false
  }
}

async function aiSummary() {
  aiBusy.value = true
  aiResult.value = null
  try {
    aiResult.value = await http.post('/news/ai-summary')
    if (!aiResult.value.ok) app.notify(aiResult.value.error || 'AI 综述失败', 'error')
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    aiBusy.value = false
  }
}

let timer: number | undefined
onMounted(async () => {
  await load()
  timer = window.setInterval(load, 60000)
})
onBeforeUnmount(() => window.clearInterval(timer))
</script>

<template>
  <div class="stack">
    <div class="kpi-grid">
      <div class="kpi">
        <div class="kpi-label">24H 资讯</div>
        <div class="kpi-value">{{ overview?.total_24h ?? 0 }}</div>
        <div class="kpi-foot">最近刷新 {{ ago(overview?.last_refresh || 0) }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">情绪指数</div>
        <div class="kpi-value" :class="numClass(overview?.sentiment_index)">{{ (overview?.sentiment_index ?? 0).toFixed(3) }}</div>
        <div class="kpi-foot">{{ sentimentLabel }} · 偏多 {{ overview?.positive ?? 0 }} / 偏空 {{ overview?.negative ?? 0 }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">情绪分布</div>
        <div class="bar-track" style="margin-top: 12px">
          <div class="bar-fill" :style="{ width: sentimentPct + '%' }" />
        </div>
        <div class="kpi-foot">0 空 ← → 100 多</div>
      </div>
      <div class="kpi" style="cursor: pointer" @click="checkSources">
        <div class="kpi-label">数据源健康</div>
        <div class="kpi-value">
          {{ enabledCount }} <span class="muted" style="font-size: 14px">/ {{ configured.length }}</span>
        </div>
        <div class="kpi-foot">
          <span v-if="failedSources.length" class="down">{{ failedSources.length }} 个不可达, 点击检测</span>
          <span v-else-if="configured.length" class="up">全部正常</span>
          <span v-else class="muted">未启用任何源</span>
        </div>
      </div>
    </div>

    <div v-if="failedSources.length" class="note" style="border-left-color: #f0a92a">
      以下源抓取失败:
      <b v-for="s in failedSources" :key="s.id" style="margin: 0 6px">{{ s.label }}</b>
      —— {{ failedSources[0].error }}
      <a href="#/admin/strategy?group=news" style="margin-left: 6px">去配置代理</a>
      <span class="muted">（「参数配置 → 资讯 → proxy」，留空则复用数据源代理）</span>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">市场资讯</span>
        <div class="right">
          <button class="btn btn-sm" :disabled="checking" @click="checkSources">源检测</button>
          <button class="btn btn-sm" :disabled="loading" @click="refresh">刷新</button>
          <button class="btn btn-sm btn-primary" :disabled="aiBusy" @click="aiSummary">{{ aiBusy ? '分析中…' : 'AI 综述' }}</button>
        </div>
      </div>
      <div class="panel-body" style="padding-bottom: 0">
        <div class="row row-wrap" style="gap: 6px">
          <button class="btn btn-xs" :class="{ 'btn-primary': !activeSource }" @click="activeSource = ''">全部</button>
          <button
            v-for="s in configured"
            :key="s.id"
            class="btn btn-xs"
            :class="{ 'btn-primary': activeSource === s.id, 'btn-danger': !s.ok && activeSource !== s.id }"
            :title="s.error || `${s.count} 条 · ${s.latency_ms}ms`"
            @click="activeSource = s.id"
          >
            {{ s.label }}
            <span v-if="!s.ok" style="opacity: 0.8">!</span>
          </button>
        </div>

        <div v-if="aiResult" class="note" style="margin-top: 12px">
          <div v-if="!aiResult.ok" style="color: #ff9aa3">{{ aiResult.error }}</div>
          <template v-else>
            <div style="font-size: 12.5px; line-height: 1.8">{{ aiResult.summary?.summary }}</div>
            <div class="row" style="margin-top: 6px; gap: 6px">
              <span class="tag" :class="numClass(aiResult.summary?.sentiment)">{{ aiResult.summary?.sentiment }}</span>
              <span v-for="h in aiResult.summary?.hot || []" :key="h" class="tag tag-ai">{{ h }}</span>
            </div>
            <div class="hint" style="margin-top: 6px">风险提示: {{ aiResult.summary?.risk }}</div>
          </template>
        </div>
      </div>
      <div class="panel-body">
        <div v-if="loading && !items.length" class="empty">加载中…</div>
        <div v-else-if="!filtered.length" class="empty">暂无资讯, 点击「刷新」抓取; 若全部失败请点「源检测」</div>
        <div v-else>
          <div v-for="item in filtered" :key="item.id" class="list-item">
            <div class="row" style="align-items: flex-start; gap: 10px">
              <span
                class="tag"
                :class="item.sentiment > 0.15 ? 'tag-long' : item.sentiment < -0.15 ? 'tag-short' : 'tag-dim'"
                style="flex: 0 0 auto; margin-top: 2px"
              >
                {{ item.sentiment > 0 ? '+' : '' }}{{ item.sentiment.toFixed(2) }}
              </span>
              <div style="flex: 1; min-width: 0">
                <a v-if="item.url" :href="item.url" target="_blank" rel="noreferrer" class="item-title">{{ item.title }}</a>
                <div v-else class="item-title">{{ item.title }}</div>
                <div v-if="item.summary" class="muted" style="font-size: 11px; margin-top: 3px; line-height: 1.6">{{ item.summary }}</div>
                <div class="item-meta">
                  <span class="tag tag-dim">{{ item.source_label }}</span>
                  <span>{{ timeStr(item.ts) }}</span>
                  <span v-for="sym in item.symbols" :key="sym" class="tag tag-blue">{{ sym }}</span>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>

    <Modal v-if="diagOpen" title="资讯源检测" wide @close="diagOpen = false">
      <div v-if="checking" class="empty">正在逐个探测 (每个源最多 13 秒)…</div>
      <div v-else-if="diag" class="stack">
        <div class="row">
          <span class="tag" :class="diag.ok ? 'tag-long' : 'tag-short'">
            {{ diag.ok_count }}/{{ diag.total }} 个源正常
          </span>
          <span class="tag tag-dim">代理: {{ diag.proxy }}</span>
          <span class="tag tag-dim">超时 {{ diag.timeout_sec }}s</span>
          <div class="spacer" />
          <button class="btn btn-sm" :disabled="checking" @click="checkSources">重新检测</button>
        </div>
        <div v-if="diag.suggestion" class="note" style="border-left-color: #f04b5a">{{ diag.suggestion }}</div>
        <table class="table">
          <thead>
            <tr><th>资讯源</th><th>状态</th><th>耗时</th><th>条数</th><th>失败原因 / 建议</th></tr>
          </thead>
          <tbody>
            <tr v-for="r in diag.rows" :key="r.source">
              <td>
                {{ r.label }}
                <div class="muted mono" style="font-size: 10px">{{ r.source }}</div>
              </td>
              <td>
                <span v-if="!r.enabled" class="tag tag-dim">未启用</span>
                <span v-else class="tag" :class="r.ok ? 'tag-long' : 'tag-short'">{{ r.ok ? '正常' : '失败' }}</span>
              </td>
              <td class="mono">{{ r.latency_ms ? r.latency_ms + 'ms' : '-' }}</td>
              <td class="mono">{{ r.enabled ? r.count : '-' }}</td>
              <td style="white-space: normal; max-width: 420px">
                <span class="muted">{{ r.error || '—' }}</span>
                <div v-if="r.hint" class="hint">{{ r.hint }}</div>
                <div v-if="r.url" class="mono muted ellipsis" style="font-size: 10px">{{ r.url }}</div>
              </td>
            </tr>
          </tbody>
        </table>
        <div class="hint">
          币安广场 / 币安公告 / CoinDesk 需要能访问 `www.binance.com`、`www.coindesk.com`；
          若服务器在受限网络，请配置 HTTP 代理（参数配置 → 资讯 → proxy，或数据源 → proxy 全局复用）。
        </div>
      </div>
    </Modal>
  </div>
</template>
