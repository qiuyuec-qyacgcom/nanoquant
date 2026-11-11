<script setup lang="ts">
/**
 * 外部信号跟单（对接 RUNBOOK 的 ao-monitor）。
 * 文件通道 1s 增量读 + HTTP 直推零延迟；开仓走本系统风控/资金/执行层，
 * 与引擎 15s 轮询开仓共用同标的互斥和保证金账本。
 */
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { http } from '../api/client'
import { useAppStore } from '../stores/app'
import { ago, fmt, px, timeStr } from '../utils/format'

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

type BoolKey = 'enabled' | 'auto_add_symbol' | 'strict_events' | 'dry_run'
type NumKey = 'poll_sec' | 'leverage_bonus' | 'max_age_sec' | 'max_per_hour' | 'default_sl_pct'

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
  source_kind: string
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

const STATUS_LABELS: Record<string, { text: string; cls: string }> = {
  opened: { text: '已跟仓', cls: 'tag-long' },
  dry_run: { text: '演练', cls: 'tag-blue' },
  skipped: { text: '跳过', cls: 'tag-warn' },
  error: { text: '失败', cls: 'tag-short' },
  duplicate: { text: '重复', cls: 'tag-dim' },
  processing: { text: '处理中', cls: 'tag-ai' },
}

const toggles: { key: BoolKey; label: string; hint: string }[] = [
  { key: 'enabled', label: '启用信号跟单', hint: '关闭后既不读文件也不接收直推' },
  { key: 'auto_add_symbol', label: '标的自动补池', hint: '池内没有时用币安在线合约表校验后加入' },
  { key: 'strict_events', label: '未知事件严格拒绝', hint: '模板变体默认只告警并放行，开启后一律跳过' },
  { key: 'dry_run', label: '演练模式', hint: '只记录意图不下单；关闭即真实跟单' },
]
const numberFields: { key: NumKey; label: string; hint: string; min: number; max: number; step: number; unit: string }[] = [
  { key: 'poll_sec', label: '本地文件轮询', hint: '增量读本地 jsonl，非 Telegram 轮询', min: 1, max: 60, step: 1, unit: '秒' },
  { key: 'max_age_sec', label: '信号有效期', hint: '超时跳过，防重启补开旧单', min: 5, max: 3600, step: 1, unit: '秒' },
  { key: 'leverage_bonus', label: '杠杆加成', hint: '原信号杠杆 +N，仍受系统上限钳制', min: 0, max: 100, step: 1, unit: 'x' },
  { key: 'max_per_hour', label: '每小时跟单上限', hint: '按已成交笔数做频控', min: 1, max: 100, step: 1, unit: '笔' },
  { key: 'default_sl_pct', label: '缺省止损', hint: '信号未给止损时使用', min: 0.1, max: 20, step: 0.1, unit: '%' },
]
const NUM_KEYS: NumKey[] = numberFields.map((f) => f.key)

const CURL_EXAMPLE = `curl -X POST /api/v1/signal-follow/ingest \\
  -H "X-Signal-Token: <密钥>" -H "Content-Type: application/json" \\
  -d '{"id":"123","event":"开仓","symbol":"BTCUSDT","side":"long","leverage":"20x"}'`

const loadError = ref('')
const errorNotified = ref(false)
const ready = computed(() => !!status.value)
const listening = computed(() => !!(status.value?.running && status.value?.settings?.enabled))
const counts = computed<Record<string, number>>(() => {
  const raw = status.value?.counts || {}
  return {
    opened: raw.opened || 0, dry_run: raw.dry_run || 0, skipped: raw.skipped || 0,
    error: raw.error || 0, duplicate: raw.duplicate || 0, processing: raw.processing || 0,
  }
})
const dirty = computed(() => {
  const saved = status.value?.settings
  if (!saved) return false
  const keys = Object.keys(DEFAULTS) as (keyof FollowSettings)[]
  return keys.some((k) => (k === 'ingest_token' ? !!form[k] : form[k] !== saved[k]))
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
    loadError.value = ''
    errorNotified.value = false
  } catch (err: any) {
    // 自动轮询不弹右上角：原因常驻在「链路状态」面板，避免每轮刷新刷屏
    loadError.value = err?.status === 404
      ? '信号跟单接口不可用：请重启后端服务以加载新路由'
      : (err?.message || '加载失败')
  } finally {
    loading.value = false
  }
}

/** 只在「启用信号跟单」开关打开、且链路确实不可用时提醒一次 */
function notifyLinkError() {
  if (!form.enabled || !loadError.value || errorNotified.value) return
  errorNotified.value = true
  app.notify(loadError.value, 'error')
}

/** 状态接口返回的密钥是掩码，不能原样写回；留空表示保持已配置的密钥 */
function applySettings(s: FollowSettings) {
  Object.assign(form, s, { ingest_token: '' })
}

async function toggle(key: BoolKey) {
  const enabling = key === 'enabled' && !form.enabled
  form[key] = !form[key]
  if (enabling) {
    await load()
    notifyLinkError()
  }
}

async function save() {
  saving.value = true
  try {
    const patch: Record<string, any> = { ...form }
    for (const key of NUM_KEYS) patch[key] = Number(patch[key])
    if (!patch.ingest_token && status.value?.ingest_token_set) delete patch.ingest_token
    const data = await app.saveGroup('signal_follow', patch)
    applySettings(data)
    app.notify('配置已保存并生效（无需重启）', 'ok')
    await load()
    notifyLinkError()
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
    const detail = res.processed !== undefined
      ? `处理 ${res.processed} 行`
      : res.ok ? '完成' : (res.reason || '完成')
    app.notify(`${label}：${detail}`, 'ok')
    await load()
  } catch (err: any) {
    app.notify(err?.message || `${label}失败`, 'error')
  } finally {
    busy.value = false
  }
}

function tagOf(s: string) {
  return (STATUS_LABELS[s] || { text: s, cls: 'tag-dim' })
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
  <div class="stack">
    <div class="panel">
      <div class="panel-body" style="padding: 16px 20px">
        <div class="row" style="gap: 10px; flex-wrap: wrap">
          <span class="panel-title" style="font-size: 14px">外部信号跟单</span>
          <span class="tag tag-dim">ao-monitor</span>
          <span class="tag tag-dim">开仓即跟</span>
          <span class="tag tag-dim">+{{ form.leverage_bonus }}x 杠杆</span>
          <span class="tag tag-dim">系统资金</span>
          <div class="spacer" />
          <span class="dot" :class="!ready ? 'warn' : listening ? 'on' : 'off'" />
          <span class="muted" style="font-size: 11px">{{ !ready ? '状态读取中' : listening ? '监听中' : '未启用' }}</span>
          <span v-if="ready" class="tag" :class="status?.settings?.dry_run ? 'tag-warn' : 'tag-short'">
            {{ status?.settings?.dry_run ? '演练 · 只记录' : '实盘跟单' }}
          </span>
          <span v-else class="tag tag-dim">状态接口不可用</span>
        </div>
        <div class="hint" style="margin-top: 8px">
          信号源填本地 <code>signals.jsonl</code> 路径，或直接填 <code>http(s)</code> 频道链接（后端每 {{ form.poll_sec }} 秒拉取）。
          也可 <code>POST /api/v1/signal-follow/ingest</code> 直推，零轮询延迟。
          开仓走本系统风控与资金，和引擎轮询开仓共用同标的互斥与保证金账本。
        </div>
      </div>
    </div>

    <div class="kpi-grid">
      <div class="kpi">
        <div class="kpi-label">已跟仓</div>
        <div class="kpi-value up">{{ counts.opened }}</div>
        <div class="kpi-foot">{{ !ready ? '等待状态接口' : '演练 ' + counts.dry_run + ' 笔 · 失败 ' + counts.error + ' 笔' }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">跳过 / 重复</div>
        <div class="kpi-value">{{ counts.skipped + counts.duplicate }}</div>
        <div class="kpi-foot">含过期、风控闸门、非开仓事件</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">信号源</div>
        <div class="kpi-value" :class="!ready ? 'flat' : status?.path_exists ? 'up' : 'down'" style="font-size: 17px">
          {{ !ready ? '--' : status?.path_exists ? '已接入' : '未接入' }}
        </div>
        <div class="kpi-foot ellipsis" :title="status?.path || ''">
          {{ !ready ? '等待状态接口' : (status?.path || '请配置信号源路径 / 频道链接') }}
        </div>
      </div>
      <div class="kpi">
        <div class="kpi-label">在途保证金</div>
        <div class="kpi-value">{{ fmt(status?.inflight_margin ?? 0, 2) }} <span class="muted" style="font-size: 14px">USDT</span></div>
        <div class="kpi-foot">游标 #{{ status?.offset ?? 0 }} · 轮询 {{ ago(status?.last_poll ?? 0) }}</div>
      </div>
    </div>

    <div class="grid main-grid">
      <div class="panel">
        <div class="panel-hd">
          <div class="stack" style="gap: 2px">
            <span class="panel-title">跟单参数</span>
            <span class="muted" style="font-size: 10.5px">热更新 · 保存后无需重启</span>
          </div>
          <div class="right">
            <span v-if="dirty" class="tag tag-warn">未保存</span>
            <button class="btn btn-sm" :disabled="!status" @click="status && applySettings(status.settings)">还原</button>
            <button class="btn btn-sm btn-primary" :disabled="saving" @click="save">
              {{ saving ? '保存中…' : '保存并生效' }}
            </button>
          </div>
        </div>

        <div class="panel-body stack" style="gap: 14px">
          <div class="note warn">
            开仓杠杆 = 原信号杠杆 + {{ form.leverage_bonus }}x，再受本系统 <code>max_leverage</code> 与交易所上限钳制；
            数量、保证金、止损止盈全部按本系统风险预算与可用余额计算，不复制信号方仓位。
            <b>关闭「演练模式」即会真实下单，需人工明确授权。</b>
          </div>

          <div>
            <div class="section-hd">
              <span class="section-title">接入开关</span>
              <span class="section-hint">信号到达即刻处理，不等待引擎 15s 轮询</span>
            </div>
            <div class="grid g2" style="gap: 10px">
              <div v-for="t in toggles" :key="t.key" class="panel" style="background: rgba(10, 18, 32, 0.6)">
                <div class="panel-body" style="padding: 10px 12px">
                  <div class="row" style="gap: 10px">
                    <div class="stack" style="gap: 3px; flex: 1; min-width: 0">
                      <span style="font-size: 12px; font-weight: 600">{{ t.label }}</span>
                      <span class="hint">{{ t.hint }}</span>
                    </div>
                    <div class="switch" :class="{ on: form[t.key] }" @click="toggle(t.key)" />
                  </div>
                </div>
              </div>
            </div>
          </div>

          <div>
            <div class="section-hd">
              <span class="section-title">参数</span>
              <span class="section-hint">生效范围：文件通道与 HTTP 直推共用</span>
            </div>
            <div class="grid g2" style="gap: 10px 12px">
              <div v-for="f in numberFields" :key="f.key" class="field">
                <span class="label">{{ f.label }} <span class="muted">({{ f.unit }})</span></span>
                <input
                  v-model.number="form[f.key]" type="number" class="input mono"
                  :min="f.min" :max="f.max" :step="f.step"
                />
                <span class="hint">{{ f.hint }}</span>
              </div>
              <div class="hint" style="align-self: center">参数越界会被后端拒绝并给出具体字段提示。</div>
            </div>
          </div>

          <div>
            <div class="section-hd">
              <span class="section-title">信号源与直推</span>
              <span class="section-hint">频道不写死在代码里</span>
            </div>
            <div class="stack" style="gap: 10px">
              <div class="field">
                <span class="label">信号源路径 / 频道链接</span>
                <input v-model="form.jsonl_path" class="input mono" placeholder="本地 signals.jsonl 路径，或 http(s) 频道链接" />
                <span class="hint">本地路径支持文件或频道目录；填 <code>http(s)</code> 链接则后端定时拉取（支持 JSON 数组 / 含 signals 字段的对象 / JSONL 文本）；首次接入不回放历史。</span>
              </div>
              <div class="field">
                <span class="label">直推共享密钥</span>
                <input
                  v-model="form.ingest_token" type="password" class="input mono" autocomplete="off"
                  :placeholder="status?.ingest_token_set ? '已设置，留空表示保持不变' : '留空 = 直推接口仅接受后台登录态'"
                />
                <span class="hint">状态接口只返回掩码；不改动密钥时请保持输入框为空。</span>
              </div>
              <div class="code">{{ CURL_EXAMPLE }}</div>
            </div>
          </div>
        </div>
      </div>

      <aside class="panel" style="align-self: start">
        <div class="panel-hd">
          <span class="panel-title">链路状态</span>
          <div class="right">
            <span class="tag" :class="!ready ? 'tag-dim' : status?.path_exists ? 'tag-long' : 'tag-warn'">
              {{ !ready ? '状态未读取' : status?.path_exists ? (status?.source_kind === 'url' ? '链接已接入' : '文件已接入') : '等待上游写入' }}
            </span>
          </div>
        </div>
        <div class="panel-body stack" style="gap: 10px">
          <div class="row">
            <span class="muted" style="font-size: 11.5px">监听协程</span>
            <div class="spacer" />
            <span class="dot" :class="!ready ? 'warn' : status?.running ? 'on' : 'off'" />
            <span style="font-size: 11.5px">{{ !ready ? '--' : status?.running ? '运行中' : '未运行' }}</span>
          </div>
          <div class="row">
            <span class="muted" style="font-size: 11.5px">交易引擎</span>
            <div class="spacer" />
            <span v-if="ready" class="tag" :class="status?.engine_enabled ? 'tag-long' : 'tag-warn'">
              {{ status?.engine_enabled ? '已启动' : '未启动' }}
            </span>
            <span v-else class="muted mono">--</span>
          </div>
          <div class="row">
            <span class="muted" style="font-size: 11.5px">最近轮询</span>
            <div class="spacer" />
            <span class="mono" style="font-size: 11.5px">{{ timeStr(status?.last_poll ?? 0) }}</span>
          </div>
          <div class="row">
            <span class="muted" style="font-size: 11.5px">在途预留</span>
            <div class="spacer" />
            <span class="mono" style="font-size: 11.5px">{{ fmt(status?.inflight_margin ?? 0, 2) }} USDT</span>
          </div>
          <div v-if="ready && !status?.engine_enabled" class="hint">引擎未启动时信号会被跳过，请先在总览启动引擎。</div>

          <div class="divider" />

          <div class="field">
            <span class="label">信号源</span>
            <div class="code" style="font-size: 10.5px; max-height: 84px">
              {{ !ready ? '状态接口不可用' : (status?.path || '未发现 signals.jsonl') }}
            </div>
          </div>
          <div v-if="loadError" class="note error">
            {{ loadError }}
            <div class="hint">原因只在此处常驻展示，不会随自动刷新反复弹出；打开「启用信号跟单」开关时提醒一次。</div>
          </div>
          <div v-if="status?.last_error" class="note error">
            最近错误：{{ status.last_error }}
          </div>

          <div class="row" style="gap: 6px; flex-wrap: wrap">
            <button class="btn btn-sm" :disabled="busy" @click="runOp('/signal-follow/read-once', '立即读一轮')">立即读一轮</button>
            <button class="btn btn-sm" :disabled="busy" @click="runOp('/signal-follow/reset-cursor', '重置游标')">重置游标</button>
            <button class="btn btn-sm" :disabled="loading" @click="load">刷新</button>
          </div>
          <div class="hint">切换频道或清理积压旧信号时用「重置游标」：本地文件移到尾部、频道链接清空去重索引，均不回放历史、不删数据。</div>
        </div>
      </aside>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">跟单流水</span>
        <div class="right">
          <span class="muted" style="font-size: 10.5px">{{ events.length }} 条 · 时间倒序</span>
          <select v-model="filter" class="input" style="width: 112px" @change="load">
            <option value="">全部状态</option>
            <option v-for="(v, k) in STATUS_LABELS" :key="k" :value="k">{{ v.text }}</option>
          </select>
          <button class="btn btn-sm" :disabled="loading" @click="load">筛选</button>
        </div>
      </div>
      <div class="panel-body flush">
        <div v-if="!events.length" class="empty">{{ loading ? '加载中…' : '暂无跟单记录' }}</div>
        <table v-else class="table">
          <thead>
            <tr>
              <th style="width: 118px">时间</th>
              <th style="width: 112px">标的</th>
              <th style="width: 44px">方向</th>
              <th style="width: 72px">裁决</th>
              <th style="width: 56px">杠杆</th>
              <th style="width: 92px">数量</th>
              <th style="width: 96px">价格</th>
              <th style="width: 100px">保证金</th>
              <th>原因</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in events" :key="row.id">
              <td class="mono muted">{{ timeStr(row.created_at) }}</td>
              <td class="mono">{{ row.symbol }}</td>
              <td :class="row.side === 'long' ? 'up' : 'down'">{{ row.side === 'long' ? '多' : '空' }}</td>
              <td><span class="tag" :class="tagOf(row.status).cls">{{ tagOf(row.status).text }}</span></td>
              <td class="mono">{{ row.leverage ? row.leverage + 'x' : '—' }}</td>
              <td class="mono">{{ row.qty ? fmt(row.qty, 4) : '—' }}</td>
              <td class="mono">{{ row.price ? px(row.price) : '—' }}</td>
              <td class="mono">{{ row.margin ? fmt(row.margin, 2) + ' U' : '—' }}</td>
              <td class="muted" style="white-space: normal; min-width: 220px">{{ row.reason || '—' }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>
  </div>
</template>

<style scoped>
.main-grid {
  grid-template-columns: minmax(0, 1fr) 320px;
  gap: 12px;
}
.section-hd {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  margin-bottom: 8px;
}
.section-hint {
  font-size: 10.5px;
  color: var(--muted);
}
code {
  color: var(--text-dim);
  font-family: var(--mono);
  font-size: 10.5px;
}
@media (max-width: 960px) {
  .main-grid {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>
