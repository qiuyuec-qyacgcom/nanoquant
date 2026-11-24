<script setup lang="ts">
/**
 * 参数集治理：展示各 regime 的参数集、证据摘要与守卫状态，
 * 并允许人工启用 approved/manual 集或回滚当前 active 集。
 * 所有操作走后端同一事务与审计，页面只做确认与刷新。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { api, http } from '../api/client'
import { useSilentRefresh } from '../composables/silentRefresh'
import { ago, timeStr } from '../utils/format'
import { loadParamChanges, rollbackParamChange, rollbackWarning, type ParamChange } from './paramChanges'
import {
  canManuallyActivate,
  canRollback as canRollbackSet,
  evidenceSummary,
  formatPercent,
  gateTag,
  regimeLabel,
  statusTag,
  type EvidenceLike,
} from '../utils/paramSets'
import Modal from '../components/Modal.vue'

interface SetItem {
  id: number
  name: string
  regime: string
  timeframe: string
  version: number
  status: string
  source: string
  evidence: EvidenceLike & Record<string, any>
  parent_id: number | null
  created_at: number
  activated_at: number | null
}

interface ActiveEntry {
  regime: string
  set_id: number | null
  name: string | null
  timeframe: string | null
}

interface GuardEntry {
  set_id?: number
  last_candidate_at?: number
  started_at?: number
  rollback_done?: boolean
}

interface CurrentView {
  market_regime: string | null
  active: ActiveEntry[]
  guard: Record<string, GuardEntry>
  auto_apply_since: number
}

const REGIME_ORDER = ['trend', 'range', 'chaos']

const sets = ref<SetItem[]>([])
const current = ref<CurrentView | null>(null)
const loading = ref(false)
const busyId = ref(0)
const error = ref('')
const notice = ref('')
const evidenceTarget = ref<SetItem | null>(null)
const paramChanges = ref<ParamChange[]>([])
const paramChangesBusy = ref(false)
const paramChangesError = ref('')
let paramChangesTimer: number | null = null

const grouped = computed(() => {
  const map = new Map<string, SetItem[]>()
  for (const item of sets.value) {
    if (!map.has(item.regime)) map.set(item.regime, [])
    map.get(item.regime)!.push(item)
  }
  const known = REGIME_ORDER.filter((regime) => map.has(regime))
  const extra = [...map.keys()].filter((regime) => !REGIME_ORDER.includes(regime)).sort()
  return [...known, ...extra].map((regime) => ({
    regime,
    items: map.get(regime)!.slice().sort((a, b) => b.id - a.id),
  }))
})

const activeEntries = computed(() =>
  (current.value?.active || []).filter((entry) => entry.set_id !== null)
)
const guardRows = computed(() =>
  Object.entries(current.value?.guard || {}).map(([regime, guard]) => ({ regime, ...guard }))
)
const autoApplySinceText = computed(() => {
  const since = current.value?.auto_apply_since || 0
  return since > 0 ? timeStr(since, false) : '未设置'
})
const marketRegimeText = computed(() => regimeLabel(current.value?.market_regime))

function canActivate(item: SetItem): boolean {
  return canManuallyActivate(item.status, item.regime, current.value?.market_regime)
}

function isActivatableStatus(item: SetItem): boolean {
  return item.status === 'approved' || item.status === 'manual'
}

function canRollback(item: SetItem): boolean {
  return canRollbackSet(item.status, item.regime, current.value?.market_regime)
}

function guardAge(entry: GuardEntry): string {
  return entry.last_candidate_at ? ago(entry.last_candidate_at) : '--'
}

async function load(silent = false) {
  if (!silent) loading.value = true
  try {
    const [list, view] = await Promise.all([
      http.get('/ai-tuning'),
      http.get('/ai-tuning/current'),
    ])
    sets.value = Array.isArray(list?.items) ? list.items : []
    current.value = view || null
  } catch (err: any) {
    if (!silent) error.value = err?.message || '加载参数集失败'
  } finally {
    loading.value = false
  }
}

async function act(item: SetItem, action: 'activate' | 'rollback') {
  const tip =
    action === 'activate'
      ? `确认启用参数集「${item.name}」？仅当前 regime 可手动启用，启用后将立即物化到当前 strategy。`
      : `确认将 ${regimeLabel(item.regime)} 的当前集回滚到上一套？若该集没有上一套会失败。`
  if (!confirm(tip)) return
  busyId.value = item.id
  error.value = ''
  notice.value = ''
  try {
    await http.post(`/ai-tuning/${item.id}/${action}`)
    notice.value = action === 'activate' ? '已启用，参数已物化。' : '已回滚到上一套。'
    await load(true)
  } catch (err: any) {
    error.value = err?.message || '操作失败'
  } finally {
    busyId.value = 0
  }
}

function openEvidence(item: SetItem) {
  evidenceTarget.value = item
}

function displayParamValue(raw: string | null | undefined): string {
  if (raw === null || raw === undefined || raw === '') return '—'
  try {
    return JSON.stringify(JSON.parse(raw))
  } catch {
    return raw
  }
}

async function refreshParamChanges() {
  try {
    paramChanges.value = await loadParamChanges(api, 20)
    paramChangesError.value = ''
  } catch (err: any) {
    paramChangesError.value = err?.message || '加载失败'
  }
}

async function rollbackChange(id: number) {
  if (!confirm(rollbackWarning(false, false))) return
  paramChangesBusy.value = true
  try {
    await rollbackParamChange(api, id)
    await Promise.all([refreshParamChanges(), load(true)])
    notice.value = '已回滚该次 AI 调参。'
  } catch (err: any) {
    error.value = err?.message || '回滚失败'
  } finally {
    paramChangesBusy.value = false
  }
}

function numberOf(value: unknown): number | undefined {
  return typeof value === 'number' ? value : undefined
}

onMounted(() => {
  void load()
  void refreshParamChanges()
  if (paramChangesTimer === null) {
    paramChangesTimer = window.setInterval(() => void refreshParamChanges(), 60_000)
  }
})
onUnmounted(() => {
  if (paramChangesTimer !== null) {
    window.clearInterval(paramChangesTimer)
    paramChangesTimer = null
  }
})
useSilentRefresh(() => load(true), 30000)
</script>

<template>
  <div class="stack" style="gap: 12px">
    <div class="panel">
      <div class="panel-body" style="padding: 16px 20px">
        <div class="row" style="gap: 10px; flex-wrap: wrap">
          <span class="panel-title" style="font-size: 14px">AI 调参</span>
          <span class="tag tag-dim">当前 regime {{ marketRegimeText }}</span>
          <span v-for="entry in activeEntries" :key="entry.regime" class="tag tag-long">
            {{ regimeLabel(entry.regime) }} #{{ entry.set_id }} {{ entry.name }}
          </span>
          <div class="spacer" />
          <span class="muted" style="font-size: 11px">每 30s 静默刷新 · 手动操作需确认</span>
        </div>
        <div class="hint" style="margin-top: 8px">
          AI 每周期自动调整隐藏参数。系统按 regime 保存候选快照，approve 后通常由引擎自动启用；
                  这里提供人工兜底：当前 regime 的 approved/manual 集可立即启用，当前 regime 的 active 集可回滚到上一套。
                  跨 regime 的启用与回滚由自动切换处理。
        </div>
        <div v-if="notice" class="hint" style="margin-top: 6px; color: var(--long)">{{ notice }}</div>
        <div v-if="error" class="hint" style="margin-top: 6px; color: var(--warn)">{{ error }}</div>
      </div>
    </div>

    <div class="kpi-grid">
      <div class="kpi">
        <div class="kpi-label">当前 regime</div>
        <div class="kpi-value">{{ marketRegimeText }}</div>
        <div class="kpi-foot">取自引擎实跑市场状态</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">active 集</div>
        <div class="kpi-value">{{ activeEntries.length }}</div>
        <div class="kpi-foot">trend / range / chaos</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">自动启用起点</div>
        <div class="kpi-value" style="font-size: 15px">{{ autoApplySinceText }}</div>
        <div class="kpi-foot">更早的候选不参与自动启用</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">无候选守卫</div>
        <div class="kpi-value">{{ guardRows.length }}</div>
        <div class="kpi-foot">已记录计时的 regime 数</div>
      </div>
    </div>

    <div class="panel" v-if="guardRows.length">
      <div class="panel-hd"><span class="panel-title">守卫状态</span></div>
      <div class="panel-body" style="padding: 10px 16px">
        <div class="row" style="gap: 14px; flex-wrap: wrap; font-size: 11px">
          <span v-for="row in guardRows" :key="row.regime" class="tag tag-dim">
            {{ regimeLabel(row.regime) }} · 集 #{{ row.set_id ?? '--' }} · 上次候选 {{ guardAge(row) }}
            <span v-if="row.rollback_done"> · 已回滚过</span>
          </span>
        </div>
      </div>
    </div>

    <div v-if="loading" class="empty" style="padding: 40px">加载中…</div>
    <div v-else-if="!grouped.length" class="empty" style="padding: 40px">暂无调参候选</div>

    <div v-for="group in grouped" :key="group.regime" class="panel">
      <div class="panel-hd">
        <span class="panel-title">{{ regimeLabel(group.regime) }}</span>
        <span class="muted" style="font-size: 11px">{{ group.items.length }} 套参数</span>
      </div>
      <div class="panel-body" style="padding: 0">
        <table class="table">
          <thead>
            <tr>
              <th style="width: 150px">名称 / 版本</th>
              <th style="width: 64px">状态</th>
              <th style="width: 64px">来源</th>
              <th style="width: 56px">周期</th>
              <th>证据摘要</th>
              <th style="width: 80px">门</th>
              <th style="width: 150px">操作</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="item in group.items" :key="item.id">
              <td>
                <div class="mono">{{ item.name }}</div>
                <div class="muted" style="font-size: 10px">
                  #{{ item.id }} · v{{ item.version }}
                  <span v-if="item.activated_at"> · {{ timeStr(item.activated_at, false) }}</span>
                </div>
              </td>
              <td><span class="tag" :class="statusTag(item.status).cls">{{ statusTag(item.status).text }}</span></td>
              <td class="mono muted">{{ item.source }}</td>
              <td class="mono">{{ item.timeframe }}</td>
              <td class="muted" style="font-size: 10.5px">
                {{ item.evidence ? evidenceSummary(item.evidence) : '暂无证据' }}
                <div v-if="item.evidence?.relative_advantage_pp != null" style="font-size: 9.5px">
                  相对 active {{ formatPercent(numberOf(item.evidence.relative_advantage_pp)) }}
                </div>
              </td>
              <td>
                <span class="tag" :class="gateTag(item.evidence || {}).cls">{{ gateTag(item.evidence || {}).text }}</span>
              </td>
              <td>
                <div class="row" style="gap: 6px; flex-wrap: wrap">
                  <button class="btn btn-sm btn-ghost" @click="openEvidence(item)">证据</button>
                  <button
                    v-if="isActivatableStatus(item)"
                    class="btn btn-sm"
                    :disabled="busyId === item.id || !canActivate(item)"
                    :title="canActivate(item) ? '' : `仅当前 regime（${marketRegimeText}）可手动启用；其他 regime 由自动切换处理`"
                    @click="act(item, 'activate')"
                  >
                    {{ busyId === item.id ? '处理中…' : '启用' }}
                  </button>
                  <button
                    v-else-if="item.status === 'active'"
                    class="btn btn-sm"
                    :disabled="busyId === item.id || !canRollback(item)"
                    :title="canRollback(item) ? '' : `仅当前 regime（${marketRegimeText}）的 active 集可回滚；其他 regime 由自动切换处理`"
                    @click="act(item, 'rollback')"
                  >
                    {{ busyId === item.id ? '处理中…' : '回滚' }}
                  </button>
                </div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <Modal
      v-if="evidenceTarget"
      :title="`证据详情 · ${evidenceTarget.name}`"
      wide
      @close="evidenceTarget = null"
    >
      <div class="stack" style="gap: 10px; font-size: 11.5px">
        <div class="row" style="gap: 8px; flex-wrap: wrap">
          <span class="tag" :class="statusTag(evidenceTarget.status).cls">
            {{ statusTag(evidenceTarget.status).text }}
          </span>
          <span class="tag tag-dim">{{ regimeLabel(evidenceTarget.regime) }}</span>
          <span class="tag tag-dim">{{ evidenceTarget.timeframe }}</span>
          <span class="tag" :class="gateTag(evidenceTarget.evidence || {}).cls">
            {{ gateTag(evidenceTarget.evidence || {}).text }}
          </span>
        </div>
        <div class="hint">{{ evidenceSummary(evidenceTarget.evidence || {}) }}</div>
        <div v-if="evidenceTarget.evidence?.gate?.reasons?.length" class="hint" style="color: var(--warn)">
          未通过原因：
          <span v-for="reason in evidenceTarget.evidence.gate.reasons" :key="reason" class="mono" style="display: block">
            · {{ reason }}
          </span>
        </div>
        <div class="muted">原始证据</div>
        <pre class="evidence-json">{{ JSON.stringify(evidenceTarget.evidence, null, 2) }}</pre>
      </div>
    </Modal>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">AI 调参记录</span>
        <span class="muted" style="font-size: 11px">最近 20 条 · 每分钟自动刷新</span>
      </div>
      <div class="panel-body" style="padding: 10px 16px">
        <div v-if="paramChangesError" class="hint" style="color: var(--warn)">
          调参记录加载失败：{{ paramChangesError }}
        </div>
        <div v-else-if="!paramChanges.length" class="empty" style="padding: 20px">暂无 AI 调参记录</div>
        <div v-else class="change-table">
          <div class="change-row change-head">
            <span>时间</span>
            <span>字段</span>
            <span>旧值 → 新值</span>
            <span>原因</span>
            <span>效果</span>
            <span>操作</span>
          </div>
          <div v-for="row in paramChanges" :key="row.id" class="change-row">
            <span class="muted">{{ timeStr(row.ts, false) }}</span>
            <span class="mono">{{ row.group }}.{{ row.key }}</span>
            <span class="mono">
              <s>{{ displayParamValue(row.old_value) }}</s>
              →
              {{ displayParamValue(row.new_value) }}
            </span>
            <span>{{ row.reason }}</span>
            <span class="muted">{{ row.effect?.trades ?? 0 }} 笔 · {{ (row.effect?.pnl ?? 0).toFixed(2) }} USDT</span>
            <button class="btn btn-xs" :disabled="paramChangesBusy" @click="rollbackChange(row.id)">回滚</button>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.table {
  width: 100%;
  border-collapse: collapse;
  font-size: 11.5px;
}
.table th,
.table td {
  padding: 8px 10px;
  text-align: left;
  border-bottom: 1px solid rgba(148, 170, 200, 0.07);
  vertical-align: top;
}
.table th {
  color: var(--muted);
  font-size: 10px;
  font-weight: 500;
  white-space: nowrap;
}
.table tbody tr:hover {
  background: rgba(148, 170, 200, 0.04);
}
.evidence-json {
  max-height: 420px;
  overflow: auto;
  margin: 0;
  padding: 10px;
  font-size: 10.5px;
  line-height: 1.5;
  background: rgba(8, 14, 26, 0.6);
  border-radius: 6px;
  white-space: pre-wrap;
  word-break: break-all;
}

.change-table {
  display: flex;
  flex-direction: column;
  font-size: 11px;
}
.change-row {
  display: grid;
  grid-template-columns: 84px 150px minmax(0, 1.2fr) minmax(0, 1fr) 120px 58px;
  gap: 10px;
  align-items: center;
  padding: 7px 0;
  border-top: 1px dashed rgba(26, 41, 66, 0.6);
}
.change-row:first-of-type {
  border-top: none;
}
.change-head {
  color: var(--muted);
  font-size: 10.5px;
}
.change-row s {
  color: var(--muted);
  text-decoration-color: rgba(240, 75, 90, 0.6);
}
@media (max-width: 720px) {
  .change-row {
    grid-template-columns: 76px minmax(0, 1fr) 56px;
  }
  .change-row > :nth-child(3),
  .change-row > :nth-child(4),
  .change-row > :nth-child(5) {
    display: none;
  }
}
</style>