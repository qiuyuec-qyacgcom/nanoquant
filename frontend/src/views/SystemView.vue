<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { useSilentRefresh } from '../composables/silentRefresh'
import { http } from '../api/client'
import EventFeed from '../components/EventFeed.vue'
import { useAppStore } from '../stores/app'
import type { EventItem } from '../types'
import { visibleEvents } from '../utils/events'
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
const core = ref<any>(null)
const testing = ref(false)
const klines = ref<any>(null)

const filteredEvents = computed(() => {
  const byLevel = levelFilter.value
    ? events.value.filter((e) => e.level === levelFilter.value)
    : events.value
  return visibleEvents(byLevel)
})

/** 执行内核状态：已连接 / 重连中 / 未连接。默认必选组件，未连接即异常。 */
const execTag = computed(() => {
  if (!core.value) return { text: '读取中…', cls: 'tag-dim' }
  if (core.value.connected) return { text: '已连接', cls: 'tag-long' }
  if (core.value.state === 'Reconnecting') return { text: '重连中', cls: 'tag-warn' }
  return { text: '未连接', cls: 'tag-short' }
})

/** K 线流状态：没订阅时上游本来就不连，标「空闲」而不是「未连接」 */
const klineTag = computed(() => {
  if (!klines.value) return { text: '读取中…', cls: 'tag-dim' }
  if (klines.value.connected) return { text: '已连接', cls: 'tag-long' }
  const subs = (klines.value.subscriptions || []).length
  if (subs > 0) return { text: '未连接', cls: 'tag-warn' }
  return { text: '空闲', cls: 'tag-dim' }
})

/** 耗时：没有样本就显示「—」，不显示 0ms */
function msText(value: unknown): string {
  return value === null || value === undefined ? '—' : `${value}ms`
}

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
  const [i, e, l, a, ar, sc, k] = await Promise.allSettled([
    http.get('/system/info'),
    http.get('/system/events?limit=120'),
    http.get(`/system/logs?lines=300${keyword.value ? '&keyword=' + encodeURIComponent(keyword.value) : ''}`),
    http.get('/system/alerts/logs?limit=10'),
    http.get('/system/archives'),
    http.get('/system/exec-core'),
    http.get('/kline/status'),
  ])
  if (i.status === 'fulfilled') info.value = i.value
  if (e.status === 'fulfilled') events.value = e.value.items
  if (l.status === 'fulfilled') logs.value = l.value.lines
  if (a.status === 'fulfilled') alerts.value = a.value.items || []
  if (ar.status === 'fulfilled') archives.value = ar.value.items || []
  if (sc.status === 'fulfilled') core.value = sc.value
  if (k.status === 'fulfilled') klines.value = k.value
}

async function selfTestCore() {
  testing.value = true
  try {
    const res = await http.post('/system/exec-core/self-test', {})
    if (res.outcome === 'ok') {
      // 这里只验证 health 协议能应答；逐字节签名比对要用 ops verify-exec-core
      app.notify(
        res.connected ? '执行内核端点可达（' + res.endpoint + '）' : res.detail || '自检通过',
        'ok'
      )
    } else if (res.outcome === 'skipped') {
      app.notify(res.detail || '自检跳过（未配置交易所 Secret）', 'info')
    } else {
      app.notify(res.detail || '自检失败', 'error')
    }
    core.value = await http.get('/system/exec-core')
  } catch (err: any) {
    app.notify(err?.message || '自检失败', 'error')
  } finally {
    testing.value = false
  }
}

async function resetCore() {
  try {
    core.value = await http.post('/system/exec-core/reset', {})
    app.notify('比对计数已清零', 'ok')
  } catch (err: any) {
    app.notify(err?.message || '清零失败', 'error')
  }
}

async function loadLogs() {
  try {
    const res = await http.get(`/system/logs?lines=300${keyword.value ? '&keyword=' + encodeURIComponent(keyword.value) : ''}`)
    logs.value = res.lines
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

onMounted(async () => {
  // 诊断涉及外部网络探测, 与基础信息并行, 避免页面长时间空白
  await Promise.allSettled([runDiagnostics(), load()])
})

useSilentRefresh(load, 15000)
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
        <span class="panel-title">执行内核（exec-core）</span>
        <div class="right">
          <span class="tag" :class="execTag.cls">{{ execTag.text }}</span>
          <router-link to="/admin/settings/params?group=system" class="btn btn-xs">配置</router-link>
          <button class="btn btn-xs" :disabled="testing" @click="selfTestCore">
            {{ testing ? '自检中…' : '自检' }}
          </button>
          <button class="btn btn-xs" @click="resetCore">清零</button>
        </div>
      </div>
      <div class="panel-body stack" style="gap: 10px">
        <div class="muted" style="font-size: 11px; line-height: 1.7">
          {{ core?.verdict || '读取中…' }}
        </div>
        <div class="hint">
          默认执行内核，主服务启动需完成握手；端点通过环境变量配置：
          <code>EXEC_TCP=host:port</code> 或 <code>EXEC_SOCKET=/path/to.sock</code>
          （Windows 可执行 <code>scripts\start-core.ps1</code> 拉起内核）。
        </div>
        <table v-if="core" class="table">
          <tbody>
            <tr>
              <td class="muted">通道</td>
              <td class="mono" style="white-space: normal">
                {{ core.target || '未配置' }}
                <span class="muted">（{{ core.connected ? '已连接' : core.state === 'Reconnecting' ? '重连中' : '未连接' }}{{ core.algo ? ' · ' + core.algo : '' }}）</span>
                <span v-if="core.last_error" class="down"> · {{ core.last_error }}</span>
              </td>
            </tr>
            <tr>
              <td class="muted">自检</td>
              <td class="mono">
                共 {{ core.counts?.checks ?? 0 }} 次 · 成功 <span class="up">{{ core.counts?.ok ?? 0 }}</span> ·
                失败 <span class="down">{{ core.counts?.errors ?? 0 }}</span> ·
                跳过 {{ core.counts?.skipped ?? 0 }}
              </td>
            </tr>
            <tr>
              <td class="muted">往返耗时</td>
              <td class="mono">
                <template v-if="core.connected">
                  最近 {{ msText(core.latency_ms?.last) }} · p50 {{ msText(core.latency_ms?.p50) }} ·
                  p95 {{ msText(core.latency_ms?.p95) }} · p99 {{ msText(core.latency_ms?.p99) }}
                  <span class="muted">（探活一帧 health 的往返）</span>
                </template>
                <template v-else>
                  —<span class="muted">（内核未连接，暂无探活样本）</span>
                </template>
              </td>
            </tr>
            <tr>
              <td class="muted">下单分段延迟</td>
              <td class="mono" style="white-space: normal">
                <div v-for="seg in ['total','plan','sign','http','wait','protect','persist']" :key="seg">
                  <span class="muted">{{ seg }}</span>
                  <template v-if="core.segments?.[seg]?.count">
                    count {{ core.segments[seg].count }} · p50 {{ msText(core.segments[seg].p50) }} ·
                    p95 {{ msText(core.segments[seg].p95) }} · p99 {{ msText(core.segments[seg].p99) }} ·
                    max {{ msText(core.segments[seg].max) }}
                  </template>
                  <template v-else>—<span class="muted">（暂无样本）</span></template>
                </div>
              </td>
            </tr>
            <tr v-if="core.dropped">
              <td class="muted">上报丢弃</td>
              <td class="mono down">{{ core.dropped }} 条（队列满自动丢弃，不影响下单）</td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">K 线流</span>
        <div class="right">
          <span class="tag" :class="klineTag.cls">{{ klineTag.text }}</span>
        </div>
      </div>
      <div class="panel-body">
        <table v-if="klines" class="table">
          <tbody>
            <tr>
              <td class="muted">上游</td>
              <td class="mono">
                {{ klines.connected ? '已连接' : '未连接' }}
                <span class="muted">{{ klines.env ? ' · ' + klines.env : '' }}</span>
              </td>
            </tr>
            <tr>
              <td class="muted">订阅</td>
              <td class="mono">
                {{ (klines.subscriptions || []).length }} 路 · 客户端 {{ klines.clients ?? 0 }} 个
              </td>
            </tr>
            <tr>
              <td class="muted">最后消息</td>
              <td class="mono">
                {{
                  klines.last_message_age === null || klines.last_message_age === undefined
                    ? '—'
                    : klines.last_message_age + 's 前'
                }}
              </td>
            </tr>
          </tbody>
        </table>
        <div v-else class="muted">读取中…</div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">最近告警</span>
        <div class="right">
          <span class="tag tag-dim">{{ alerts.length }}</span>
          <router-link to="/admin/settings/params?group=system" class="btn btn-xs">配置告警</router-link>
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
              background: var(--surface-input);
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
