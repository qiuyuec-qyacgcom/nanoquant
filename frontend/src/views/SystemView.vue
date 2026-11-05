<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { http } from '../api/client'
import EventFeed from '../components/EventFeed.vue'
import { useAppStore } from '../stores/app'
import type { EventItem } from '../types'
import { timeStr } from '../utils/format'

const app = useAppStore()
const diag = ref<any>(null)
const info = ref<any>(null)
const events = ref<EventItem[]>([])
const alerts = ref<any[]>([])
const archives = ref<any[]>([])
const archivePreview = ref<{ name: string; rows: any[] } | null>(null)
const archiveText = computed(() =>
  (archivePreview.value?.rows || [])
    .map((row: any) => JSON.stringify(row))
    .join('\n')
)

async function previewArchive(name: string) {
  try {
    const res = await http.get(`/system/archives/${encodeURIComponent(name)}?limit=20`)
    archivePreview.value = { name: res.name, rows: res.rows || [] }
  } catch (err: any) {
    app.notify(err?.message || '归档读取失败', 'error')
  }
}
const logs = ref<string[]>([])
const keyword = ref('')
const levelFilter = ref('')
const checking = ref(false)

const filteredEvents = computed(() =>
  levelFilter.value ? events.value.filter((e) => e.level === levelFilter.value) : events.value
)

async function runDiagnostics() {
  checking.value = true
  try {
    diag.value = await http.get('/system/diagnostics')
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    checking.value = false
  }
}

async function load() {
  const [i, e, l, a, ar] = await Promise.allSettled([
    http.get('/system/info'),
    http.get('/system/events?limit=120'),
    http.get(`/system/logs?lines=300${keyword.value ? '&keyword=' + encodeURIComponent(keyword.value) : ''}`),
    http.get('/system/alerts/logs?limit=10'),
    http.get('/system/archives'),
  ])
  if (i.status === 'fulfilled') info.value = i.value
  if (e.status === 'fulfilled') events.value = e.value.items
  if (l.status === 'fulfilled') logs.value = l.value.lines
  if (a.status === 'fulfilled') alerts.value = a.value.items || []
  if (ar.status === 'fulfilled') archives.value = ar.value.items || []
}

async function loadLogs() {
  try {
    const res = await http.get(`/system/logs?lines=300${keyword.value ? '&keyword=' + encodeURIComponent(keyword.value) : ''}`)
    logs.value = res.lines
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

let timer: number | undefined
onMounted(async () => {
  // 诊断涉及外部网络探测, 与基础信息并行, 避免页面长时间空白
  await Promise.allSettled([runDiagnostics(), load()])
  timer = window.setInterval(load, 15000)
})
onBeforeUnmount(() => window.clearInterval(timer))
</script>

<template>
  <div class="stack">
    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">一键诊断</span>
        <div class="right">
          <span v-if="diag" class="tag" :class="diag.ok ? 'tag-long' : 'tag-warn'">{{ diag.ok ? '全部正常' : '存在异常项' }}</span>
          <button class="btn btn-sm btn-primary" :disabled="checking" @click="runDiagnostics">{{ checking ? '诊断中…' : '重新诊断' }}</button>
        </div>
      </div>
      <div class="panel-body">
        <div class="grid g3" style="gap: 10px">
          <div v-for="c in diag?.checks || []" :key="c.name" class="panel" style="background: rgba(10,18,32,0.6)">
            <div class="panel-body" style="padding: 12px">
              <div class="row">
                <span class="dot" :class="c.ok ? 'on' : 'err'" />
                <span style="font-weight: 600; font-size: 12px">{{ c.name }}</span>
                <div class="spacer" />
                <span v-if="c.latency_ms" class="mono muted" style="font-size: 10px">{{ c.latency_ms }}ms</span>
              </div>
              <div class="muted" style="font-size: 11px; line-height: 1.6; margin-top: 6px; white-space: normal">{{ c.detail }}</div>
            </div>
          </div>
          <div v-if="!diag" class="empty">诊断中…</div>
        </div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">最近告警</span>
        <div class="right">
          <span class="tag tag-dim">{{ alerts.length }}</span>
          <router-link to="/admin/strategy?group=system" class="btn btn-xs">配置告警</router-link>
        </div>
      </div>
      <div class="panel-body" style="padding: 0">
        <div v-if="!alerts.length" class="empty" style="padding: 28px">
          还没有发送记录。启用告警后，warn/error 事件会自动推送并在这里留痕。
        </div>
        <table v-else class="table">
          <thead>
            <tr>
              <th>时间</th>
              <th>级别</th>
              <th>类别</th>
              <th>内容</th>
              <th>通道</th>
              <th>结果</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in alerts" :key="row.id">
              <td class="mono muted">{{ timeStr(row.ts) }}</td>
              <td>
                <span class="tag" :class="row.level === 'error' ? 'tag-short' : row.level === 'warn' ? 'tag-warn' : 'tag-dim'">
                  {{ row.level }}
                </span>
              </td>
              <td class="muted">{{ row.category }}</td>
              <td style="white-space: normal; max-width: 520px">{{ row.message }}</td>
              <td class="mono muted">{{ row.channels || '—' }}</td>
              <td class="mono" :class="row.ok ? 'up' : 'down'" :title="row.error">
                {{ row.ok ? '已送达' : '失败' }}
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">历史归档</span>
        <div class="right">
          <span class="tag tag-dim">{{ archives.length }}</span>
          <span class="muted" style="font-size: 10.5px">过期数据先写 gzip 再删除</span>
        </div>
      </div>
      <div class="panel-body" style="padding: 0">
        <div v-if="!archives.length" class="empty" style="padding: 28px">
          还没有归档文件。清理任务触发后，过期信号/事件会先落到 data/archive/。
        </div>
        <table v-else class="table">
          <thead>
            <tr><th>文件</th><th>类型</th><th>日期</th><th>大小</th><th></th></tr>
          </thead>
          <tbody>
            <tr v-for="row in archives" :key="row.name">
              <td class="mono">{{ row.name }}</td>
              <td class="muted">{{ row.kind }}</td>
              <td class="mono muted">{{ row.date }}</td>
              <td class="mono">{{ (row.size / 1024).toFixed(1) }} KB</td>
              <td><button class="btn btn-xs" @click="previewArchive(row.name)">预览</button></td>
            </tr>
          </tbody>
        </table>
        <div v-if="archivePreview" class="panel-body stack" style="gap: 8px; border-top: 1px solid rgba(148,170,200,0.08)">
          <div class="row" style="gap: 8px">
            <span class="mono" style="font-size: 11px">{{ archivePreview.name }}</span>
            <span class="tag tag-dim">{{ archivePreview.rows.length }} 行</span>
            <div class="spacer" />
            <button class="btn btn-xs" @click="archivePreview = null">关闭</button>
          </div>
          <pre
            class="mono"
            style="
              max-height: 220px;
              overflow: auto;
              margin: 0;
              padding: 8px 10px;
              font-size: 10.5px;
              line-height: 1.5;
              white-space: pre-wrap;
              word-break: break-all;
              background: rgba(8, 14, 26, 0.6);
              border-radius: 6px;
              color: var(--muted);
            "
          >{{ archiveText }}</pre>
        </div>
      </div>
    </div>

    <div class="grid g2">
      <div class="panel">
        <div class="panel-hd"><span class="panel-title">运行环境</span></div>
        <div class="panel-body">
          <table class="table">
            <tbody>
              <tr><td class="muted" style="width: 120px">版本</td><td class="mono">{{ info?.app }} v{{ info?.version }}</td></tr>
              <tr><td class="muted">Python</td><td class="mono">{{ info?.python }}</td></tr>
              <tr><td class="muted">平台</td><td class="mono" style="white-space: normal">{{ info?.platform }}</td></tr>
              <tr><td class="muted">启动时间</td><td class="mono">{{ timeStr(info?.started_at || 0) }}</td></tr>
              <tr><td class="muted">运行时长</td><td class="mono">{{ Math.floor((info?.uptime || 0) / 60) }} 分钟</td></tr>
              <tr><td class="muted">数据目录</td><td class="mono muted" style="white-space: normal">{{ info?.paths?.data }}</td></tr>
              <tr><td class="muted">交易环境</td><td class="mono">{{ info?.engine?.mode === 'live' ? '实盘 · 币安主网' : '模拟盘 · 币安测试网' }}</td></tr>
            </tbody>
          </table>
        </div>
      </div>

      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">运行事件</span>
          <div class="right">
            <select v-model="levelFilter" class="select" style="width: 110px; padding: 4px 8px">
              <option value="">全部级别</option>
              <option value="info">info</option>
              <option value="warn">warn</option>
              <option value="error">error</option>
            </select>
            <span class="tag tag-dim">{{ filteredEvents.length }}</span>
          </div>
        </div>
        <div class="panel-body"><EventFeed :events="filteredEvents" height="300px" /></div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">服务日志</span>
        <div class="right">
          <input v-model="keyword" class="input" placeholder="关键字过滤" style="width: 160px; padding: 5px 9px" @keyup.enter="loadLogs" />
          <button class="btn btn-sm" @click="loadLogs">刷新</button>
        </div>
      </div>
      <div class="panel-body">
        <pre class="code" style="max-height: 420px">{{ (logs || []).join('\n') || '暂无日志' }}</pre>
      </div>
    </div>
  </div>
</template>
