<script setup lang="ts">
/**
 * 参数配置中心
 * ---------------------------------------------------------------
 * 排版原则:
 * - 交易环境 (模拟盘/实盘) 是唯一开关, 固定在最上方, 不混在分组参数里;
 * - 左侧分组导航 + 右侧分区表单, 每区只放同语义的字段;
 * - 字段按语义选择控件 (开关/分段/数字+单位/标签/文本), 并给出口径说明;
 * - 顶部保留「未保存」提示与保存/恢复默认, 避免改完不知道有没有生效。
 */
import { computed, nextTick, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { http } from '../api/client'
import EnvironmentSwitch from '../components/EnvironmentSwitch.vue'
import { useAppStore } from '../stores/app'
import { useAuthStore } from '../stores/auth'
import { useParamCatalogStore } from '../stores/paramCatalog'

import DataTab from './settings/DataTab.vue'
import EngineTab from './settings/EngineTab.vue'
import RiskTab from './settings/RiskTab.vue'
import SecurityTab from './settings/SecurityTab.vue'
import SettingsAudit from './settings/SettingsAudit.vue'
import { BASE_META, GROUP_META, PLAIN_HINTS } from './settings/meta'
import SettingsSection from './settings/SettingsSection.vue'
import { SETTINGS_GROUPS, SETTINGS_SECTIONS, visibleSettingKeys } from './settingsCatalog'
import type { FieldMeta, Widget } from './settings/types'
import { normalizeNumericPayload } from '../utils/paramSets'


interface Row extends FieldMeta {
  key: string
  value: any
  widget: Widget
}

const groups = SETTINGS_GROUPS






const SECTIONS = SETTINGS_SECTIONS

const SKIP: Record<string, string[]> = {
  engine: ['mode', 'enabled'],
}

// 只读/敏感字段: 不渲染、不回写; 否则「保存」会用页面加载时的旧密文覆盖刚更新的密钥
const PROTECTED = new Set([
  'api_key_enc',
  'api_secret_enc',
  'demo_api_key_enc',
  'demo_api_secret_enc',
  'live_api_key_enc',
  'live_api_secret_enc',
  'password_hash',
  'last_run_ts',
  'token_version',
])

const app = useAppStore()
const paramCatalog = useParamCatalogStore()
const auth = useAuthStore()
const route = useRoute()
const router = useRouter()

const current = ref('engine')
const draft = ref<Record<string, any>>({})
const baseline = ref<Record<string, any>>({})
const saving = ref(false)
const credentials = ref<any>(null)
const credForms = reactive({
  demo: { api_key: '', api_secret: '' },
  live: { api_key: '', api_secret: '' },
})
const proxyForm = ref('')
const credSaving = ref<'' | 'demo' | 'live'>('')
const pwd = reactive({ old_password: '', new_password: '' })

const engineBusy = ref(false)


const currentGroup = computed(() => groups.find((g) => g.id === current.value))
const dirty = computed(() => JSON.stringify(draft.value) !== JSON.stringify(baseline.value))
const engineRunning = computed(() => !!app.status?.enabled)
const engineLive = computed(() => app.status?.mode === 'live')

function clone<T>(v: T): T {
  return JSON.parse(JSON.stringify(v ?? {}))
}

function metaOf(group: string, key: string): FieldMeta {
  const base = GROUP_META[group]?.[key] || BASE_META[key] || { label: key }
  const plain = PLAIN_HINTS[key]
  return plain ? { ...base, hint: plain } : base
}

function widgetOf(value: any, meta: FieldMeta): Widget {
  if (meta.widget) return meta.widget
  if (typeof value === 'boolean') return 'switch'
  if (typeof value === 'number') return 'number'
  if (Array.isArray(value)) return value.some((v) => typeof v === 'object') ? 'json' : 'tags'
  if (value && typeof value === 'object') return 'json'
  return 'text'
}

function rowOf(group: string, key: string, value: any): Row {
  const meta = metaOf(group, key)
  return { key, value, widget: widgetOf(value, meta), label: meta.label, hint: meta.hint || '', unit: meta.unit || '', options: meta.options, min: meta.min, max: meta.max, step: meta.step }
}

function skippedKeys(): Set<string> {
  return skipOf(current.value)
}

// ------------------------------------------------------------ 参数检索
/**
 * 参数分散在 10 个分组、上百个字段里，先让用户猜"在哪一组"是最大的摩擦。
 * 这里做三件事：
 * 1) 跨全部分组的关键词检索（同时搜中文名、字段名、说明、分区名），点击直接跳转并高亮；
 * 2) 「常用直达」——最高频的十来个参数一键定位，不必记分组；
 * 3) 「只看已改」——保存前确认到底动了哪几项。
 */
const query = ref('')
const hitsOpen = ref(false)
const focusKey = ref('')
const onlyDirty = ref(false)
const searchInput = ref<HTMLInputElement | null>(null)

interface Hit {
  group: string
  groupLabel: string
  section: string
  key: string
  label: string
  hint: string
  value: any
}

/** 常用直达：出现频率最高的参数，跨分组一键定位 */
const QUICK_LINKS: { label: string; group: string; key: string }[] = [
  { label: '交易间隔', group: 'engine', key: 'cycle_interval_sec' },
  { label: '下单方式', group: 'engine', key: 'order_type' },
  { label: '最大持仓', group: 'engine', key: 'max_positions' },

  { label: 'K 线周期', group: 'strategy', key: 'timeframe' },
  { label: '单仓保证金中枢', group: 'risk', key: 'margin_pct' },
  { label: '最大杠杆', group: 'risk', key: 'max_leverage' },
  { label: '本地杠杆基准', group: 'risk', key: 'local_leverage_base' },
  { label: '日亏上限', group: 'risk', key: 'daily_loss_limit_pct' },
  { label: '最大回撤', group: 'risk', key: 'max_drawdown_pct' },

  { label: '最低开仓盈亏比', group: 'strategy', key: 'min_entry_rr' },
  { label: '手续费率', group: 'risk', key: 'fee_rate_taker' },
]

function sectionMapOf(group: string): Map<string, string> {
  const m = new Map<string, string>()
  for (const def of SECTIONS[group] || []) for (const k of def.fields) m.set(k, def.title)
  return m
}

function skipOf(group: string): Set<string> {
  return new Set([...PROTECTED, ...(SKIP[group] || []), ...paramCatalog.hiddenFor(group), 'symbols'])
}

/** 每个分组有多少可编辑参数（导航上显示，避免"点进去才知有没有"） */
const groupCounts = computed<Record<string, number>>(() => {
  const out: Record<string, number> = {}
  for (const g of groups) {
    const cfg = (app.settings as Record<string, any>)[g.id] || {}
    const skip = skipOf(g.id)
    out[g.id] = Object.keys(cfg).filter((k) => !skip.has(k)).length
  }
  return out
})

const searchHits = computed<Hit[]>(() => {
  const q = query.value.trim().toLowerCase()
  if (!q) return []
  const out: Hit[] = []
  for (const g of groups) {
    const cfg = (app.settings as Record<string, any>)[g.id] || {}
    const skip = skipOf(g.id)
    const secs = sectionMapOf(g.id)
    for (const [key, value] of Object.entries(cfg)) {
      if (skip.has(key)) continue
      const meta = metaOf(g.id, key)
      const hay = [meta.label, key, meta.hint || '', PLAIN_HINTS[key] || '', secs.get(key) || '']
        .join(' ')
        .toLowerCase()
      if (!hay.includes(q)) continue
      out.push({
        group: g.id,
        groupLabel: g.label,
        section: secs.get(key) || '其他参数',
        key,
        label: meta.label,
        hint: meta.hint || PLAIN_HINTS[key] || '',
        value,
      })
    }
  }
  return out.slice(0, 30)
})

function preview(v: any): string {
  if (typeof v === 'boolean') return v ? '开' : '关'
  if (Array.isArray(v)) return v.length ? `${v.length} 项` : '空'
  if (v && typeof v === 'object') return '…'
  const s = String(v ?? '')
  return s.length > 18 ? `${s.slice(0, 18)}…` : s
}

function clearSearch() {
  query.value = ''
  hitsOpen.value = false
}

/** 跳到某个参数：必要时切分组、滚动到视野中央并闪一下 */
async function jumpTo(group: string, key: string) {
  // 首屏还在拉配置时点「常用直达」会没有任何反应：先等配置到位再跳
  if (!Object.keys(app.settings || {}).length) await app.loadSettings()
  if (group !== current.value) {
    if (dirty.value && !confirm('当前分组有未保存的修改，切换后会被丢弃，继续？')) return
    loadGroup(group)
  }
  onlyDirty.value = false
  clearSearch()
  focusKey.value = key
  await nextTick()
  const el = document.getElementById(`fld-${group}-${key}`)
  if (!el) return
  el.scrollIntoView({ block: 'center', behavior: 'smooth' })
  window.setTimeout(() => {
    if (focusKey.value === key) focusKey.value = ''
  }, 2200)
}

/** 当前分组里相对"已保存值"有哪些改动 */
const changedKeys = computed(() => {
  const skip = skippedKeys()
  return Object.keys(draft.value || {}).filter(
    (k) => !skip.has(k) && JSON.stringify(draft.value[k]) !== JSON.stringify((baseline.value as any)[k]),
  )
})

function onHotkey(e: KeyboardEvent) {
  const tag = (e.target as HTMLElement)?.tagName
  const typing = tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT'
  if ((e.key === '/' && !typing) || ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k')) {
    e.preventDefault()
    searchInput.value?.focus()
    hitsOpen.value = true
  }
  if (e.key === 'Escape') clearSearch()
}

/** 只看改动：长表单保存前核对到底动了哪几项 */
function toggleOnlyDirty() {
  onlyDirty.value = !onlyDirty.value
}

const currentSections = computed(() => {
  const skip = skippedKeys()
  const keep = onlyDirty.value ? new Set(changedKeys.value) : null
  const allowed = new Set(
    visibleSettingKeys(current.value, Object.keys(draft.value || {}), skip, paramCatalog.failed),
  )
  const rows = new Map<string, Row>()
  for (const [key, value] of Object.entries(draft.value || {})) {
    if (!allowed.has(key)) continue
    if (keep && !keep.has(key)) continue
    rows.set(key, rowOf(current.value, key, value))
  }
  const out: { title: string; hint: string; rows: Row[] }[] = []
  for (const def of SECTIONS[current.value] || []) {
    const list = def.fields.filter((k) => rows.has(k)).map((k) => rows.get(k)!)
    if (!list.length && (def.fields.length > 0 || onlyDirty.value)) continue
    list.forEach((r) => rows.delete(r.key))
    out.push({ title: def.title, hint: def.hint || '', rows: list })
  }

  return out
})


// ---------------------------------------------------- 交易模式（双向逐仓）
const tradeMode = ref<any>(null)
const tradeModeError = ref('')
const isolatedOnly = computed(() => app.settings.engine?.isolated === true)
const isolatedBusy = ref(false)

async function loadTradeMode() {
  try {
    tradeMode.value = await http.get('/engine/trade-mode', 8000)
    tradeModeError.value = ''
  } catch (err: any) {
    tradeMode.value = null
    tradeModeError.value = err?.message || '交易模式读取失败'
  }
}

async function toggleIsolated() {
  if (isolatedBusy.value) return
  const enabled = !isolatedOnly.value
  isolatedBusy.value = true
  try {
    await app.saveGroup('engine', { isolated: enabled })
    if (current.value === 'engine') {
      // 开关是独立保存的；必须同步表单草稿，否则随后「保存并生效」会用旧值覆盖。
      draft.value = { ...draft.value, isolated: enabled }
      baseline.value = { ...baseline.value, isolated: enabled }

    }
    if (enabled && tradeMode.value?.can_switch) {
      const res = await http.post(
        '/engine/trade-mode',
        { hedge: Boolean(tradeMode.value?.hedge_mode), isolated: true },
        15000,
      )
      const firstFail = (res.results || []).find((item: any) => !item.ok)
      app.notify(
        res.ok ? '已开启全局逐仓并完成切换' : firstFail?.detail || '逐仓开关已保存，部分标的切换失败',
        res.ok ? 'ok' : 'error',
      )
      await loadTradeMode()
    } else if (enabled) {
      app.notify('已开启全局逐仓开单；现有仓位保持原模式，新开仓会自动切逐仓', 'info')
    } else {
      app.notify('已关闭全局逐仓开单', 'ok')
    }
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    isolatedBusy.value = false
  }
}

function loadGroup(id: string) {
  current.value = id
  draft.value = clone(app.settings[id] || {})
  baseline.value = clone(app.settings[id] || {})
  onlyDirty.value = false
  focusKey.value = ''

  if (id === 'engine') loadTradeMode()
  if (id === 'risk') loadUnprotected()
  loadAudit()
}

function onGroupClick(id: string) {
  if (!dirty.value || id === current.value) {
    loadGroup(id)
    return
  }
  if (confirm('当前分组有未保存的修改，切换后会被丢弃，继续？')) loadGroup(id)
}

async function save() {
  saving.value = true
  try {
    const payload: Record<string, any> = { ...draft.value }
    for (const key of PROTECTED) delete payload[key]
    // 交易环境的 mode/enabled 由顶部开关与引擎控制接口管理, 不随表单回写
    if (current.value === 'engine') {
      delete payload.mode
      delete payload.enabled
    }
    const normalized = normalizeNumericPayload(
      current.value,
      payload,
      baseline.value,
      (group, key) => paramCatalog.kindFor(group, key),
    )
    if (!normalized.ok) {
      const meta = metaOf(current.value, normalized.key)
      app.notify(`${meta.label || normalized.key} 需要填写数字`, 'error')
      return
    }
    const data = await app.saveGroup(current.value, normalized.value)
    draft.value = clone(data)
    baseline.value = clone(data)

    onlyDirty.value = false
    app.notify('配置已保存并热生效', 'ok')
    if (current.value === 'engine' || current.value === 'data') await app.loadStatus()
    await loadAudit()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    saving.value = false
  }
}

async function reset() {
  if (!confirm('恢复当前分组为默认值？')) return
  try {
    const data = await app.resetGroup(current.value)
    draft.value = clone(data)
    baseline.value = clone(data)

    onlyDirty.value = false
    if (current.value === 'engine' || current.value === 'data') await app.loadStatus()
    app.notify('已恢复默认值', 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

/** 放弃本次改动：退回上次保存的值，不用重新加载页面 */
function discard() {
  draft.value = clone(baseline.value)

  onlyDirty.value = false
  app.notify('已放弃未保存的修改', 'ok')
}

// ---------------------------------------------------- 日亏基准重置
/**
 * 充值 / 重置模拟盘资金 / 换账户之后必须做一次：旧基准还在的时候，系统会拿新
 * 资金和旧基准比，算出 -99% 的假日内亏损并熔断（这个坑已经踩过两次）。
 */
const anchorBusy = ref(false)

async function resetAnchors() {
  if (!confirm('把日亏基准与回撤峰值重置为当前权益？\n只在充值、重置模拟盘资金或换账户后使用。')) return
  anchorBusy.value = true
  try {
    const res = await http.post('/engine/reset-anchors')
    await app.loadStatus()
    app.notify(
      res.cleared_halt ? `基准已对齐到 ${res.equity}；已解除「${res.cleared_halt}」` : `基准已对齐到 ${res.equity}`,
      'ok',
    )
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    anchorBusy.value = false
  }
}

/** 当前分组的最近变更（谁改的、改前改后） */
const auditRows = ref<any[]>([])

async function loadAudit() {
  try {
    const res = await http.get(`/settings/audit?group=${current.value}&limit=6`)
    auditRows.value = res.items || []
  } catch {
    auditRows.value = []
  }
}

function tsText(ts: number): string {
  if (!ts) return '-'
  const d = new Date(ts * 1000)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

function fieldLabel(key: string): string {
  return metaOf(current.value, key).label || key
}

function shortValue(v: any): string {
  if (v === null || v === undefined || v === '') return '空'
  if (typeof v === 'boolean') return v ? '开' : '关'
  if (Array.isArray(v)) return v.length ? `${v.length} 项` : '空'
  if (typeof v === 'object') return '…'
  const s = String(v)
  return s.length > 14 ? `${s.slice(0, 14)}…` : s
}

// ---------------------------------------------------- 无保护单仓位
/** 手动开的仓常常没挂止损，系统只能显示 sl=0。这里列出并支持一键按 ATR 补挂。 */
const unprotected = ref<{ id: number; symbol: string }[]>([])
const protectBusy = ref(false)

async function loadUnprotected() {
  try {
    const res = await http.get('/engine/positions?limit=50')
    unprotected.value = (res.items || [])
      .filter((p: any) => !p.sl_price && !p.tp_price)
      .map((p: any) => ({ id: p.id, symbol: p.symbol }))
  } catch {
    unprotected.value = []
  }
}

async function protectAll() {
  if (!unprotected.value.length) return
  if (!confirm(`按 ATR 给 ${unprotected.value.length} 个无保护单仓位补挂止损/止盈？`)) return
  protectBusy.value = true
  let ok = 0
  let fail = 0
  for (const p of unprotected.value) {
    try {
      await http.post(`/engine/positions/${p.id}/protect`)
      ok += 1
    } catch {
      fail += 1
    }
  }
  protectBusy.value = false
  await loadUnprotected()
  await app.loadStatus()
  app.notify(`已补挂 ${ok} 个${fail ? `，失败 ${fail} 个` : ''}`, fail ? 'error' : 'ok')
}

// ---------------------------------------------------- 紧急全平
/**
 * 事故处置只该有一个动作：停引擎 + 撤挂单 + 市价平掉当前环境全部仓位。
 * 后端要求确认短语 FLATTEN（与切实盘的 LIVE 同款约定），避免误触清仓。
 */
const flattenBusy = ref(false)

async function flattenAll() {
  const phrase = prompt(
    '紧急全平：将停止引擎、撤销全部挂单，并以市价平掉当前交易环境的全部仓位。\n\n输入 FLATTEN 确认：',
  )
  if (phrase === null) return
  if (phrase.trim().toUpperCase() !== 'FLATTEN') {
    app.notify('确认短语不正确，已取消', 'error')
    return
  }
  flattenBusy.value = true
  try {
    const res = await http.post('/engine/flatten', { confirm: 'FLATTEN', stop_engine: true })
    await app.loadStatus()
    const closed = (res.closed || []).length
    const failed = (res.failed || []).length
    app.notify(
      failed ? `已平 ${closed} 笔，${failed} 笔失败，请手动处理` : `已平 ${closed} 笔，引擎已停止`,
      failed ? 'error' : 'ok',
    )
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    flattenBusy.value = false
  }
}

async function toggleEngine() {
  if (engineBusy.value) return
  const target = !engineRunning.value
  if (target && !confirm('启动引擎后将按当前交易环境运行，确认启动？')) return
  engineBusy.value = true
  try {
    await app.control({ enabled: target })
    app.notify(target ? '引擎已启动' : '引擎已停止', 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    engineBusy.value = false
  }
}

async function onEnvChanged() {
  await app.loadStatus()
}


async function loadCredentials() {
  try {
    credentials.value = await http.get('/settings/credentials/exchange')
    proxyForm.value = credentials.value?.proxy || ''
  } catch {
    credentials.value = null
  }
}

async function saveCredentials(env: 'demo' | 'live') {
  const form = credForms[env]
  if (!form.api_key && !form.api_secret) {
    app.notify('请至少填写 API Key 与 Secret', 'error')
    return
  }
  credSaving.value = env
  try {
    const payload: any = { env }
    if (form.api_key) payload.api_key = form.api_key
    if (form.api_secret) payload.api_secret = form.api_secret
    if (proxyForm.value !== '') payload.proxy = proxyForm.value
    await http.post('/settings/credentials/exchange', payload)
    form.api_key = ''
    form.api_secret = ''
    await Promise.all([loadCredentials(), app.loadSettings()])
    app.notify(`${env === 'demo' ? '模拟盘（测试网）' : '实盘（主网）'}凭据已保存`, 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    credSaving.value = ''
  }
}

async function clearCredentials(env: 'demo' | 'live') {
  const label = env === 'demo' ? '模拟盘（测试网）' : '实盘（主网）'
  if (!confirm(`清空${label}的 API 密钥？`)) return
  try {
    await http.del(`/settings/credentials/exchange?env=${env}`)
    await Promise.all([loadCredentials(), app.loadSettings()])
    app.notify('已清空', 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

async function changePassword() {
  if (!pwd.old_password || pwd.new_password.length < 6) {
    app.notify('请填写原密码，新密码至少 6 位', 'error')
    return
  }
  try {
    // 被"出厂密码"拦到这里的人, 改完就该放回原路, 而不是留在参数页
    const forced = auth.mustChangePassword
    await auth.changePassword(pwd.old_password, pwd.new_password)
    pwd.old_password = ''
    pwd.new_password = ''
    await app.loadSettings()
    app.notify('密码已更新', 'ok')
    if (forced) router.push('/admin/overview/dashboard')
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}


watch(current, (id) => {
  draft.value = clone(app.settings[id] || {})
  baseline.value = clone(app.settings[id] || {})

})

onMounted(async () => {
  window.addEventListener('keydown', onHotkey)
  if (!Object.keys(app.settings).length) await app.loadSettings()
  await Promise.all([app.loadStatus(), paramCatalog.load()])
  const wanted = String(route.query.group || '')
  const group = groups.some((g) => g.id === wanted) ? wanted : 'engine'
  loadGroup(group)
  await loadCredentials()
  // 支持 ?group=risk&key=max_drawdown_pct 直达某个参数（文档与提示可以精确指路）
  const key = String(route.query.key || '')
  if (key) await jumpTo(group, key)
})

onUnmounted(() => {
  window.removeEventListener('keydown', onHotkey)
})
</script>

<template>
  <div class="stack">
    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">交易环境</span>
        <div class="right">
          <span class="tag" :class="engineLive ? 'tag-short' : 'tag-long'">{{ engineLive ? '实盘' : '模拟盘' }}</span>
          <span class="dot" :class="engineRunning ? 'on' : 'off'" />
          <span class="muted" style="font-size: 11px">{{ engineRunning ? '引擎已启动' : '引擎未启动' }}</span>
          <button class="btn btn-sm" :disabled="engineBusy" @click="toggleEngine">
            {{ engineBusy ? '…' : engineRunning ? '停止引擎' : '启动引擎' }}
          </button>
          <button class="btn btn-sm btn-danger" :disabled="flattenBusy" @click="flattenAll" title="停引擎 + 撤单 + 市价平掉全部仓位">
            {{ flattenBusy ? '全平中…' : '紧急全平' }}
          </button>
        </div>
      </div>
      <div class="panel-body stack" style="gap: 10px">
        <EnvironmentSwitch @changed="onEnvChanged" />
        <div class="hint">
          模拟盘对接<b>币安测试网</b>，实盘对接<b>币安主网</b>，两者都需要各自配置 API 凭证并真实下单。
          切换后只管理当前环境的仓位，另一环境的仓位会保留但暂停管理。
        </div>
      </div>
    </div>

    <!-- 参数检索：跨分组搜索 + 常用直达。参数上百个，先解决"找不到" -->
    <div class="panel">
      <div class="panel-body stack" style="gap: 10px">
        <div class="search-row">
          <input
            ref="searchInput"
            v-model="query"
            class="input search-input"
            placeholder="搜索参数：杠杆 / 止损 / 周期 / 手续费 / 熔断 …"
            @focus="hitsOpen = true"
            @input="hitsOpen = true"
            @keyup.enter="searchHits.length && jumpTo(searchHits[0].group, searchHits[0].key)"
          />
          <span class="search-kbd" title="按 / 或 Ctrl+K 聚焦搜索">/</span>
          <button v-if="query" class="btn btn-sm btn-ghost" @click="clearSearch">清空</button>
        </div>

        <div class="quick-row">
          <span class="quick-label">常用直达</span>
          <button
            v-for="q in QUICK_LINKS"
            :key="q.group + q.key"
            type="button"
            class="chip-btn"
            @click="jumpTo(q.group, q.key)"
          >
            {{ q.label }}
          </button>
        </div>

        <div v-if="query && hitsOpen" class="search-results">
          <div v-if="!searchHits.length" class="search-empty">
            没有匹配「{{ query }}」的参数。试试：杠杆 / 止损 / 周期 / 手续费 / 熔断
          </div>
          <button
            v-for="h in searchHits"
            :key="h.group + h.key"
            type="button"
            class="search-hit"
            @click="jumpTo(h.group, h.key)"
          >
            <span class="hit-where">{{ h.groupLabel }} · {{ h.section }}</span>
            <span class="hit-label">{{ h.label }}</span>
            <span class="hit-hint">{{ h.hint }}</span>
            <span class="hit-value">{{ preview(h.value) }}</span>
          </button>
          <div v-if="searchHits.length" class="search-foot">
            {{ searchHits.length }} 个结果 · 点击跳到该参数并高亮（Esc 关闭）
          </div>
        </div>
      </div>
    </div>

    <div class="settings-layout">
      <aside class="settings-nav">
        <button
          v-for="g in groups"
          :key="g.id"
          type="button"
          class="nav-item"
          :class="{ on: current === g.id }"
          @click="onGroupClick(g.id)"
        >
          <span class="nav-name">
            {{ g.label }}
            <span class="nav-count">{{ groupCounts[g.id] ?? 0 }}</span>
          </span>
          <span class="nav-hint">{{ g.hint }}</span>
        </button>
      </aside>

      <div class="stack">
        <div class="panel">
          <div class="panel-hd">
            <div class="stack" style="gap: 2px">
              <span class="panel-title">{{ currentGroup?.label }}参数</span>
              <span class="muted" style="font-size: 10.5px">{{ currentGroup?.hint }}</span>
            </div>
            <div class="right" style="flex-wrap: wrap">
              <button
                v-if="changedKeys.length"
                class="btn btn-sm btn-ghost"
                :class="{ 'btn-primary': onlyDirty }"
                :title="onlyDirty ? '显示本组全部参数' : '只显示本组改动过的参数'"
                @click="toggleOnlyDirty"
              >
                {{ onlyDirty ? '显示全部' : `只看改动 ${changedKeys.length}` }}
              </button>
              <span v-if="dirty" class="tag tag-warn">未保存</span>
              <button class="btn btn-sm" @click="reset">恢复默认</button>
              <button class="btn btn-sm btn-primary" :disabled="saving" @click="save">
                {{ saving ? '保存中…' : '保存并生效' }}
              </button>
            </div>
          </div>

          <div class="panel-body stack" style="gap: 16px">
            <!-- 引擎: 交易模式（双向持仓 + 逐仓） -->
            <EngineTab
              v-if="current === 'engine'"
              :trade-mode="tradeMode"
              :trade-mode-error="tradeModeError"
              :isolated-only="isolatedOnly"
              :isolated-busy="isolatedBusy"
              @toggle="toggleIsolated"
            />

            <!-- 数据源: 两套环境凭证 (模拟盘=测试网 / 实盘=主网) -->
            <DataTab
              v-if="current === 'data'"
              :credentials="credentials"
              :cred-forms="credForms"
              :proxy-form="proxyForm"
              :cred-saving="credSaving"
              @update:proxy-form="proxyForm = $event"
              @save="saveCredentials"
              @clear="clearCredentials"
            />

            <!-- 安全: 改密 -->
            <SecurityTab v-if="current === 'security'" :pwd="pwd" @change="changePassword" />

            <div v-if="current === 'strategy'" class="note">
              这里是<b>机器真正执行的策略参数</b>：模式、周期、杠杆区间、准入闸门、止损止盈，改完直接影响下单。
              想改「AI 怎么分析」的文本指令，去
              <router-link to="/admin/research/strategies">全局策略</router-link>。
            </div>

            <!-- 风控: 日亏基准（充值/重置资金后必须对齐，否则会算出假 -99%） -->
            <RiskTab
              v-if="current === 'risk'"
              :app="app"
              :unprotected="unprotected"
              :anchor-busy="anchorBusy"
              :protect-busy="protectBusy"
              @reset-anchors="resetAnchors"
              @protect-all="protectAll"
            />

            <SettingsSection
              v-for="sec in currentSections"
              :key="sec.title"
              :section="sec"
              :draft="draft"
              :changed-keys="changedKeys"
              :focus-key="focusKey"
              :group-id="current"
              @update="(key, value) => draft[key] = value"
            />


            <SettingsAudit
              v-if="auditRows.length"
              :rows="auditRows"
              :field-label="fieldLabel"
              :ts-text="tsText"
              :short-value="shortValue"
            />

            <div v-if="!currentSections.length && current !== 'engine' && current !== 'data' && current !== 'security'" class="empty">
               该分组暂无可编辑字段
            </div>
          </div>
        </div>
      </div>
    </div>

    <!-- 有未保存改动时才出现的悬浮保存条：长表单滚到下面也能直接保存 -->
    <div v-if="dirty" class="save-bar">
      <span class="save-bar-text">
        {{ currentGroup?.label }} · {{ changedKeys.length }} 项未保存
      </span>
      <button class="btn btn-sm btn-ghost" @click="discard">放弃修改</button>
      <button class="btn btn-sm" @click="reset">恢复默认</button>
      <button class="btn btn-sm btn-primary" :disabled="saving" @click="save">
        {{ saving ? '保存中…' : '保存并生效' }}
      </button>
    </div>
  </div>
</template>

<style scoped>
.settings-layout {
  display: grid;
  grid-template-columns: 208px minmax(0, 1fr);
  gap: 14px;
  align-items: start;
}
.settings-nav {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 8px;
  background: var(--panel);
  border: 1px solid var(--line);
  border-radius: var(--radius);
  position: sticky;
  top: 70px;
}
.nav-item {
  display: flex;
  flex-direction: column;
  gap: 2px;
  width: 100%;
  text-align: left;
  padding: 8px 10px;
  border: 1px solid transparent;
  border-radius: 8px;
  background: transparent;
  color: var(--text-dim);
  cursor: pointer;
  transition: all 0.15s ease;
  font-family: var(--sans);
}
.nav-item:hover {
  background: rgba(59, 130, 246, 0.07);
  color: var(--text);
}
.nav-item.on {
  background: rgba(59, 130, 246, 0.14);
  border-color: rgba(59, 130, 246, 0.35);
  color: var(--text);
}
.nav-name {
  font-size: 13px;
  font-weight: 600;
}
.nav-hint {
  font-size: 10px;
  color: var(--muted);
}
.section + .section {
  border-top: 1px solid var(--line-soft);
  padding-top: 14px;
}


@media (max-width: 980px) {
  .settings-layout {
    grid-template-columns: 1fr;
  }
  .settings-nav {
    position: static;
    flex-direction: row;
    overflow-x: auto;
  }
  .nav-item {
    width: auto;
    white-space: nowrap;
  }
  .nav-hint {
    display: none;
  }

}


/* ------------------------------------------------ 参数检索 / 常用直达 */
.search-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.search-input {
  flex: 1;
  min-width: 0;
}
.search-kbd {
  flex: 0 0 auto;
  width: 20px;
  height: 20px;
  display: grid;
  place-items: center;
  font-family: var(--mono);
  font-size: 11px;
  color: var(--muted);
  border: 1px solid var(--line);
  border-radius: 5px;
  background: var(--panel-2);
}

.quick-row {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
}
.quick-label {
  font-size: 10.5px;
  color: var(--muted);
  margin-right: 2px;
}
.chip-btn {
  padding: 3px 9px;
  font-size: 11px;
  font-family: var(--sans);
  color: var(--text-dim);
  background: transparent;
  border: 1px solid var(--line);
  border-radius: 999px;
  cursor: pointer;
  transition: all 0.15s ease;
}
.chip-btn:hover {
  color: var(--text);
  border-color: rgba(59, 130, 246, 0.5);
  background: rgba(59, 130, 246, 0.08);
}

.search-results {
  display: flex;
  flex-direction: column;
  gap: 1px;
  max-height: 300px;
  overflow: auto;
  padding-top: 8px;
  border-top: 1px solid var(--line);
}
.search-empty {
  padding: 12px 8px;
  font-size: 11.5px;
  color: var(--muted);
}
.search-hit {
  display: grid;
  grid-template-columns: 132px 140px minmax(0, 1fr) auto;
  gap: 10px;
  align-items: baseline;
  padding: 7px 8px;
  border: none;
  border-radius: 7px;
  background: transparent;
  color: inherit;
  text-align: left;
  cursor: pointer;
  font-family: var(--sans);
}
.search-hit:hover {
  background: rgba(59, 130, 246, 0.1);
}
.hit-where {
  font-size: 10.5px;
  color: var(--muted);
}
.hit-label {
  font-size: 12px;
  color: var(--text);
  font-weight: 500;
}
.hit-hint {
  font-size: 10.5px;
  color: var(--text-dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.hit-value {
  font-family: var(--mono);
  font-size: 10.5px;
  color: var(--accent);
}
.search-foot {
  padding: 7px 8px 2px;
  font-size: 10.5px;
  color: var(--muted);
}

/* 导航上的字段数：点进去之前就知道这一组有多少项 */
.nav-count {
  margin-left: 4px;
  padding: 0 4px;
  font-size: 9.5px;
  font-family: var(--mono);
  color: var(--muted);
  border: 1px solid var(--line);
  border-radius: 4px;
}
.nav-item.on .nav-count {
  color: var(--accent);
  border-color: rgba(59, 130, 246, 0.4);
}


/* 未保存时悬浮的保存条：长表单滚到最下面也不用回去找按钮 */
.save-bar {
  position: fixed;
  right: 22px;
  bottom: 18px;
  z-index: 30;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 10px 8px 14px;
  background: var(--panel-3);
  border: 1px solid rgba(59, 130, 246, 0.4);
  border-radius: 10px;
  box-shadow: 0 10px 30px rgba(0, 0, 0, 0.45);
}
.save-bar-text {
  font-size: 11px;
  color: var(--text-dim);
  white-space: nowrap;
}

@media (max-width: 760px) {
  .search-hit {
    grid-template-columns: 1fr auto;
    row-gap: 2px;
  }
  .hit-where,
  .hit-hint {
    display: none;
  }
  .save-bar {
    left: 12px;
    right: 12px;
    bottom: 12px;
    justify-content: space-between;
  }
}
</style>
