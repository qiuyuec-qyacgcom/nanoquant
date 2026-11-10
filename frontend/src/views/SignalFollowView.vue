<script setup lang="ts">
/**
 * 外部信号跟单（对接 RUNBOOK 的 ao-monitor）。
 * 文件通道 1s 增量读 + HTTP 直推零延迟；开仓走本系统风控/资金/执行层，
 * 与引擎 15s 轮询开仓共用同标的互斥和保证金账本。
 */
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { http } from '../api/client'
import { useAppStore } from '../stores/app'
import { timeStr } from '../utils/format'

interface FollowSettings {
  enabled: boolean
  jsonl_path: string
  poll_sec: number
  ingest_token: string
  leverage_bonus: number
  max_age_sec: number
  max_per_hour: number
  auto_add_symbol: boolean
  strict_events: boolean
  default_sl_pct: number
  dry_run: boolean
}

interface FollowRow {
  id: number
  signal_id: string
  symbol: string
  side: string
  status: string
  reason: string
  leverage: number
  qty: number
  price: number
  margin: number
  mode: string
  created_at: number
}

interface FollowStatus {
  settings: FollowSettings
  ingest_token_set: boolean
  running: boolean
  engine_enabled: boolean
  path: string
  path_exists: boolean
  offset: number
  inflight_margin: number
  last_poll: number
  last_error: string
  counts: Record<string, number>
  items: FollowRow[]
}

const app = useAppStore()
const status = ref<FollowStatus | null>(null)
const events = ref<FollowRow[]>([])
const filter = ref('')
const loading = ref(false)
const saving = ref(false)
const busy = ref(false)

const DEFAULTS: FollowSettings = {
  enabled: false, jsonl_path: '', poll_sec: 1, ingest_token: '',
  leverage_bonus: 10, max_age_sec: 180, max_per_hour: 6,
  auto_add_symbol: true, strict_events: false, default_sl_pct: 1.2, dry_run: true,
}
const form = reactive<FollowSettings>({ ...DEFAULTS })

const STATUS_LABEL: Record<string, string> = {
  opened: '已跟仓', dry_run: '演练', skipped: '跳过', error: '失败',
  duplicate: '重复', processing: '处理中',
}
const COUNT_LABEL: Record<string, string> = {
  opened: '已跟仓', dry_run: '演练', skipped: '跳过', error: '失败',
  duplicate: '重复', processing: '处理中',
}
const numericFields = ['poll_sec', 'leverage_bonus', 'max_age_sec', 'max_per_hour', 'default_sl_pct'] as const

const counts = computed(() => {
  const out: { key: string; label: string; value: number }[] = []
  for (const key of Object.keys(COUNT_LABEL)) {
    out.push({ key, label: COUNT_LABEL[key], value: status.value?.counts?.[key] ?? 0 })
  }
  return out
})

async function load() {
  loading.value = true
  try {
    const [st, ev] = await Promise.all([
      http.get<FollowStatus>('/signal-follow/status'),
      http.get<{ items: FollowRow[] }>(`/signal-follow/events?limit=50${filter.value ? `&status=${filter.value}` : ''}`),
    ])
    status.value = st
    events.value = ev.items || []
  } catch (err: any) {
    app.notify(err?.message || '加载失败', 'error')
  } finally {
    loading.value = false
  }
}

/** 状态接口返回的密钥是掩码，不能原样写回；留空表示保持已配置的密钥 */
function applySettings(s: FollowSettings) {
  Object.assign(form, s, { ingest_token: '' })
}

async function save() {
  saving.value = true
  try {
    const patch: Record<string, any> = { ...form }
    for (const key of numericFields) patch[key] = Number(patch[key])
    if (!patch.ingest_token && status.value?.ingest_token_set) delete patch.ingest_token
    const data = await app.saveGroup('signal_follow', patch)
    applySettings(data)
    app.notify('配置已保存（1 秒内生效，无需重启）', 'ok')
    await load()
  } catch (err: any) {
    app.notify(err?.message || '保存失败', 'error')
  } finally {
    saving.value = false
  }
}

async function runOp(path: string, label: string) {
  busy.value = true
  try {
    const res = await http.post(path)
    app.notify(`${label}：${res.processed !== undefined ? `处理 ${res.processed} 行` : res.ok ? '完成' : res.reason || '完成'}`, 'ok')
    await load()
  } catch (err: any) {
    app.notify(err?.message || `${label}失败`, 'error')
  } finally {
    busy.value = false
  }
}

function statusClass(s: string) {
  return s === 'opened' ? 'tag-long' : s === 'error' ? 'tag-short' : 'tag-dim'
}

let timer: number | undefined
onMounted(async () => {
  await load()
  if (status.value) applySettings(status.value.settings)
  timer = window.setInterval(load, 5000)
})
onBeforeUnmount(() => window.clearInterval(timer))
</script>

<template>
  <div class="stack" style="gap: 12px">
    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">信号跟单 · 链路状态</span>
        <div class="right">
          <span class="tag" :class="status?.running && status?.settings?.enabled ? 'tag-long' : 'tag-dim'">
            {{ status?.running && status?.settings?.enabled ? '监听中' : '已关闭' }}
          </span>
          <span class="tag" :class="status?.settings?.dry_run ? 'tag-warn' : 'tag-short'">
            {{ status?.settings?.dry_run ? '演练（只记录不下单）' : '实盘跟单' }}
          </span>
          <button class="btn btn-sm" :disabled="loading" @click="load">刷新</button>
          <button class="btn btn-sm" :disabled="busy" @click="runOp('/signal-follow/read-once', '立即读一轮')">立即读一轮</button>
          <button class="btn btn-sm" :disabled="busy" @click="runOp('/signal-follow/reset-cursor', '重置游标')">重置游标</button>
        </div>
      </div>
      <div class="panel-body">
        <div class="hint" style="margin-bottom: 10px">
          上游 <code>ao-monitor</code> 把信号写入 <code>data/&lt;slug&gt;/signals.jsonl</code>：本模块每
          {{ status?.settings?.poll_sec ?? 1 }} 秒增量读本地文件，收到开仓信号即刻跟仓；也可以
          <code>POST /api/v1/signal-follow/ingest</code> 直推，零轮询延迟。首次接入从文件尾部开始，不回放历史。
        </div>
        <div class="kpi-row">
          <div v-for="c in counts" :key="c.key" class="kpi">
            <div class="kpi-label">{{ c.label }}</div>
            <div class="kpi-value mono">{{ c.value }}</div>
          </div>
        </div>
        <div class="grid-info">
          <div><span class="muted">信号文件</span><span class="mono">{{ status?.path || '未发现（请配置路径）' }}</span></div>
          <div><span class="muted">文件状态</span><span :class="status?.path_exists ? 'up' : 'down'">{{ status?.path_exists ? '已接入' : '等待上游写入' }}</span></div>
          <div><span class="muted">游标 / 在途保证金</span><span class="mono">#{{ status?.offset ?? 0 }} / {{ (status?.inflight_margin ?? 0).toFixed(2) }} U</span></div>
          <div><span class="muted">引擎</span><span>{{ status?.engine_enabled ? '已启动' : '未启动（信号会被跳过）' }}</span></div>
          <div><span class="muted">最近轮询</span><span class="mono">{{ timeStr(status?.last_poll ?? 0) }}</span></div>
          <div><span class="muted">最近错误</span><span :class="status?.last_error ? 'down' : ''">{{ status?.last_error || '—' }}</span></div>
        </div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">跟单参数</span>
        <div class="right">
          <button class="btn btn-sm btn-primary" :disabled="saving" @click="save">{{ saving ? '保存中…' : '保存' }}</button>
        </div>
      </div>
      <div class="panel-body">
        <div class="note" style="margin-bottom: 12px">
          开仓杠杆 = 原信号杠杆 + {{ form.leverage_bonus }}x，再受本系统 <code>max_leverage</code> 与交易所上限钳制；
          开仓数量/保证金完全按本系统的风险预算、可用余额与共享保证金账本计算，不复制信号方仓位。
          <strong>关闭「演练」即会真实下单，需人工明确授权。</strong>
        </div>
        <div class="form-grid">
          <label class="check"><input type="checkbox" v-model="form.enabled" /> 启用信号跟单</label>
          <label class="check"><input type="checkbox" v-model="form.auto_add_symbol" /> 池内没有标的时自动添加</label>
          <label class="check"><input type="checkbox" v-model="form.dry_run" /> 演练（只记录不下单）</label>
          <label class="check"><input type="checkbox" v-model="form.strict_events" /> 未知事件严格拒绝</label>

          <label class="field"><span>signals.jsonl 路径 / 频道目录</span>
            <input v-model="form.jsonl_path" placeholder="留空自动发现 ao-monitor/data/*/signals.jsonl" />
          </label>
          <label class="field"><span>本地文件轮询（秒）</span><input type="number" min="1" max="60" v-model.number="form.poll_sec" /></label>
          <label class="field"><span>杠杆加成（x）</span><input type="number" min="0" max="100" v-model.number="form.leverage_bonus" /></label>
          <label class="field"><span>信号有效期（秒）</span><input type="number" min="5" max="3600" v-model.number="form.max_age_sec" /></label>
          <label class="field"><span>每小时跟单上限</span><input type="number" min="1" max="100" v-model.number="form.max_per_hour" /></label>
          <label class="field"><span>缺省止损（%）</span><input type="number" min="0.1" max="20" step="0.1" v-model.number="form.default_sl_pct" /></label>
          <label class="field"><span>直推共享密钥</span>
            <input type="password" v-model="form.ingest_token" :placeholder="status?.ingest_token_set ? '已设置，留空表示保持不变' : '留空=仅后台登录态可直推'" />
          </label>
        </div>
        <div class="hint" style="margin-top: 10px">
          直推示例：<code>curl -H "X-Signal-Token: &lt;密钥&gt;" -H "Content-Type: application/json" -d '{"id":"123","event":"开仓","symbol":"BTCUSDT","side":"long","leverage":"20x"}' /api/v1/signal-follow/ingest</code>
        </div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">跟单流水</span>
        <div class="right">
          <select v-model="filter" @change="load" style="min-width: 130px">
            <option value="">全部状态</option>
            <option v-for="(label, key) in STATUS_LABEL" :key="key" :value="key">{{ label }}</option>
          </select>
        </div>
      </div>
      <div class="panel-body">
        <div v-if="!events.length" class="empty" style="padding: 36px">
          {{ loading ? '加载中…' : '暂无跟单记录' }}
        </div>
        <table v-else class="table">
          <thead>
            <tr><th>时间</th><th>标的 / 方向</th><th>裁决</th><th>杠杆</th><th>数量</th><th>价格</th><th>保证金</th><th>原因</th></tr>
          </thead>
          <tbody>
            <tr v-for="row in events" :key="row.id">
              <td class="mono muted">{{ timeStr(row.created_at) }}</td>
              <td class="mono">{{ row.symbol }} <span :class="row.side === 'long' ? 'up' : 'down'">{{ row.side === 'long' ? '多' : '空' }}</span></td>
              <td><span class="tag" :class="statusClass(row.status)">{{ STATUS_LABEL[row.status] || row.status }}</span></td>
              <td class="mono">{{ row.leverage }}x</td>
              <td class="mono">{{ row.qty || '—' }}</td>
              <td class="mono">{{ row.price || '—' }}</td>
              <td class="mono">{{ row.margin ? `${row.margin.toFixed(2)} U` : '—' }}</td>
              <td class="muted" style="white-space: normal; max-width: 380px">{{ row.reason || '—' }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>
  </div>
</template>

<style scoped>
.kpi-row { display: grid; grid-template-columns: repeat(6, 1fr); gap: 8px; margin-bottom: 12px; }
.kpi { padding: 10px 12px; border: 1px solid rgba(148,170,200,.1); border-radius: 8px; }
.kpi-label { color: var(--muted); font-size: 10.5px; }
.kpi-value { font-size: 18px; margin-top: 4px; }
.grid-info { display: grid; grid-template-columns: repeat(2, 1fr); gap: 8px 20px; font-size: 11.5px; }
.grid-info > div { display: flex; justify-content: space-between; gap: 12px; border-bottom: 1px solid rgba(148,170,200,.07); padding: 6px 0; }
.grid-info > div > span:last-child { text-align: right; word-break: break-all; }
.form-grid { display: grid; grid-template-columns: repeat(2, 1fr); gap: 10px 16px; }
.check { display: flex; align-items: center; gap: 8px; font-size: 12px; }
.table { width: 100%; border-collapse: collapse; font-size: 11.5px; }
.table th, .table td { padding: 7px 10px; text-align: left; border-bottom: 1px solid rgba(148,170,200,.07); }
.table th { color: var(--muted); font-size: 10px; font-weight: 500; white-space: nowrap; }
code { font-size: 11px; color: var(--muted); }
@media (max-width: 900px) { .kpi-row, .form-grid, .grid-info { grid-template-columns: repeat(2, 1fr); } }
</style>
