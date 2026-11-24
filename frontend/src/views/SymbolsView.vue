<script setup lang="ts">
/**
 * 标的池 (规则迁移自 astra-quant-agent)
 * ---------------------------------------------------------------
 * - 添加: 服务器用币安在线合约表实时校验 (精确匹配 / USDT 永续 / 交易中), 前端不做本地白名单;
 * - 去重与容量上限由服务器判定; 规格 (分层/杠杆/精度/最小下单) 随添加落库;
 * - 删除: 必须输入确认短语 REMOVE <SYMBOL>, 有持仓或无法确认持仓时会被拒绝.
 */
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import { http } from '../api/client'
import Modal from '../components/Modal.vue'
import { useAppStore } from '../stores/app'
import { fmt, timeStr } from '../utils/format'
import { deleteReason, keepRest, resultCount, scanCounts, skippedText } from '../utils/symbolScan'

interface PoolRow {
  symbol: string
  name: string
  tier: string

  enabled: boolean
  max_leverage: number
  precision: number
  tick_sz: string
  min_sz: string
  min_notional: number
  ct_val: number
  min_score: number
  note: string
  created_at: number
}

interface Candidate {
  symbol: string
  name: string
  tier: string
  tier_label: string
  max_leverage: number
  tick_sz: string
  min_sz: string
  min_notional: number
  contract_type?: string
  underlying_type?: string
  is_equity?: boolean
  in_pool: boolean
}

interface ScanState {
  last_run_ts: number
  last_result: any
}

const app = useAppStore()
const pool = ref<PoolRow[]>([])
const candidates = ref<Candidate[]>([])
const count = ref(0)
const limits = reactive({ minimum: 0, maximum: 20 })
const state = reactive({ status: '', detail: '' })
const envLabel = ref('')
const keyword = ref('')
const searching = ref(false)
const searched = ref('')
const hint = ref('')
const adding = ref('')
const loading = ref(false)

const pendingDelete = ref<PoolRow | null>(null)
const phrase = ref('')
const deleteError = ref('')
const deleting = ref(false)

const scan = ref<ScanState | null>(null)
const scanning = ref(false)
const scanPlan = ref<any | null>(null)
const showScanPlan = ref(false)
const scanError = ref('')

const requiredPhrase = computed(() => (pendingDelete.value ? `REMOVE ${pendingDelete.value.symbol}` : ''))
const canDelete = computed(() => phrase.value.trim().toUpperCase() === requiredPhrase.value)
const full = computed(() => count.value >= limits.maximum)
const lastScan = computed(() => scanCounts(scan.value?.last_result))
const planKeepRest = computed(() =>
  keepRest(scanPlan.value?.keep, scanPlan.value?.held, scanPlan.value?.suspended),
)
/** dry-run 阶段两路跳过名单都为 null，此时只提示一次「执行时二次检查」。 */
const skipPending = computed(
  () => scanPlan.value?.delete_skipped == null && scanPlan.value?.add_skipped == null,
)
const normalized = computed(() => {
  const cleaned = Array.from(keyword.value.trim())
    .filter((ch) => /[\p{L}\p{N}]/u.test(ch))
    .join('')
  return cleaned.replace(/[a-z]/g, (ch) => ch.toUpperCase())
})

const canAddDirect = computed(() => /^[\p{L}\p{N}]{2,18}USDT$/u.test(normalized.value))

async function loadPool() {
  loading.value = true
  try {
    const res = await http.get('/symbols')
    pool.value = res.items || []
    count.value = res.count ?? pool.value.length
    if (res.state?.limits) {
      limits.minimum = res.state.limits.minimum ?? 0
      limits.maximum = res.state.limits.maximum ?? 20
    }
    state.status = res.state?.status || ''
    state.detail = res.state?.detail || ''
    scan.value = res.scan || null
    envLabel.value = app.status?.env_label || ''
    scanError.value = ''
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    loading.value = false
  }
}

async function runScan(apply: boolean) {
  if (scanning.value) return
  scanning.value = true
  scanError.value = ''
  try {
    const res = await http.post('/symbols/auto-scan', { apply }, 60_000)
    if (apply) {
      showScanPlan.value = false
      scanPlan.value = null
      await loadPool()
      const skipped = resultCount(res.delete_skipped) + resultCount(res.add_skipped)
      app.notify(
        `已替换 ${resultCount(res.delete)} 个，新增 ${resultCount(res.add)} 个，跳过 ${skipped} 个`,
        'ok',
      )
    } else {
      scanPlan.value = res
      showScanPlan.value = true
    }
  } catch (err: any) {
    if (apply && err?.status === 0) {
      // 超时/网络中断时服务端可能已经执行：先刷新，再让用户核对「上次扫描」
      try {
        await loadPool()
      } catch {
        // 刷新失败不覆盖原错误
      }
      scanError.value = '执行结果未知，请核对上方「上次扫描」数据'
    } else {
      scanError.value = err?.message || '扫描失败'
    }
    app.notify(scanError.value, 'error')
  } finally {
    scanning.value = false
  }
}

function closeScanPlan() {
  if (scanning.value) return
  showScanPlan.value = false
  scanPlan.value = null
}


async function search() {
  const q = keyword.value.trim()
  if (!q) {
    candidates.value = []
    searched.value = ''
    hint.value = ''
    return
  }
  searching.value = true
  try {
    const res = await http.get(`/symbols/available?limit=50&q=${encodeURIComponent(q)}`)
    candidates.value = res.items || []
    searched.value = res.query || q.toUpperCase()
    hint.value = res.hint || ''
  } catch (err: any) {
    app.notify(err.message, 'error')
    candidates.value = []
    hint.value = err.message
  } finally {
    searching.value = false
  }
}

let searchTimer: number | undefined
watch(keyword, () => {
  window.clearTimeout(searchTimer)
  searchTimer = window.setTimeout(search, 350)
})
onBeforeUnmount(() => window.clearTimeout(searchTimer))

async function add(symbol: string) {
  if (!symbol || full.value) return
  adding.value = symbol
  try {
    const res = await http.post('/symbols', { symbol })
    const custom = res?.added?.custom === true
    app.notify(
      custom ? `${symbol} 已作为自定义标的加入（仅记录，不参与交易）` : `${symbol} 已加入标的池`,
      'ok',
    )
    await loadPool()
    if (searched.value) await search()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    adding.value = ''
  }
}

async function openDelete(row: PoolRow) {
  pendingDelete.value = row
  phrase.value = ''
  deleteError.value = ''
}

function closeDelete() {
  if (deleting.value) return
  pendingDelete.value = null
  phrase.value = ''
  deleteError.value = ''
}

async function confirmDelete() {
  const row = pendingDelete.value
  if (!row) return
  if (!canDelete.value) {
    deleteError.value = `请输入完整短语：${requiredPhrase.value}`
    return
  }
  deleting.value = true
  deleteError.value = ''
  try {
    await http.del(`/symbols/${row.symbol}`, { confirmation: requiredPhrase.value })
    app.notify(`${row.symbol} 已从标的池移除`, 'ok')
    pendingDelete.value = null
    phrase.value = ''
    await loadPool()
    if (searched.value) await search()
  } catch (err: any) {
    deleteError.value = err.message
  } finally {
    deleting.value = false
  }
}

onMounted(loadPool)
</script>

<template>
  <div class="stack">
    <div class="kpi-grid">
      <div class="kpi">
        <div class="kpi-label">标的池</div>
        <div class="kpi-value">{{ count }} <span class="muted" style="font-size: 14px">/ {{ limits.maximum }}</span></div>
        <div class="kpi-foot">{{ state.status === 'ok' ? '池内标的全部参与扫描' : '池为空，引擎不会开仓' }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">交易环境</div>
        <div class="kpi-value" style="font-size: 15px">{{ envLabel || (app.status?.mode === 'live' ? '实盘 · 币安主网' : '模拟盘 · 币安测试网') }}</div>
        <div class="kpi-foot">规格来自该环境的币安合约表</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">蓝筹 / 动量</div>
        <div class="kpi-value">
          {{ pool.filter((p) => p.tier === 'tier_1_bluechip').length }} <span class="muted" style="font-size: 14px">/ {{ pool.filter((p) => p.tier === 'tier_2_momentum').length }}</span>
        </div>
        <div class="kpi-foot">分层规则：BTC/ETH 走 Tier-1 止损参数</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">容量规则</div>
        <div class="kpi-value" style="font-size: 15px">{{ full ? '已满' : '可添加' }}</div>
        <div class="kpi-foot">上限 {{ limits.maximum }} 个 · 重复合约直接拒绝</div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">添加标的</span>
        <div class="right">
          <span class="tag tag-dim">仅币安 USDⓈ-M 永续</span>
          <button class="btn btn-sm" :disabled="loading" @click="loadPool">刷新</button>
        </div>
      </div>
      <div class="panel-body stack" style="gap: 10px">
        <div class="row" style="gap: 8px; flex-wrap: wrap">
          <input
            v-model="keyword"
            class="input mono"
            placeholder="搜索币安合约，或输入中文合约名（如 牛来USDT）"
            style="flex: 1; min-width: 200px"
            @keyup.enter="canAddDirect ? add(normalized) : search()"
          />
          <button class="btn btn-sm" :disabled="!canAddDirect || full || !!adding" @click="add(normalized)">
            {{ adding === normalized ? '…' : '直接加入' }}
          </button>
        </div>
        <div class="hint">
          币安合约会实时校验（USDT 永续、可交易）；中文名也一样先查当前环境的币安合约表。
          找到则正常启用；只有合约表里没有时，才保存为<b>自定义标的</b>（默认禁用、仅记录，不参与币安行情与下单）。
          需要实盘交易时请确认当前环境有对应合约，模拟盘找不到可切到实盘。
        </div>

        <div class="table-scroll" style="max-height: 300px">
          <table class="table">
            <thead>
              <tr>
                <th>合约</th>
                <th>分层</th>
                <th>杠杆</th>
                <th>tick / 步进</th>
                <th>最小名义额</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              <tr v-if="!searched">
                <td colspan="6" class="empty">输入关键词查看可添加的币安永续合约</td>
              </tr>
              <tr v-else-if="searching && !candidates.length">
                <td colspan="6" class="empty">正在读取币安合约表…</td>
              </tr>
              <tr v-else-if="!candidates.length">
                <td colspan="6" class="empty">{{ hint || '没有匹配的可交易合约' }}</td>
              </tr>
              <tr v-for="c in candidates" :key="c.symbol">
                <td>
                  <b class="mono">{{ c.symbol }}</b>
                  <span v-if="c.is_equity" class="tag tag-warn" style="margin-left: 6px">股票</span>
                  <div class="muted" style="font-size: 10px">{{ c.name }}</div>
                </td>
                <td><span class="tag" :class="c.tier === 'tier_1_bluechip' ? 'tag-blue' : 'tag-dim'">{{ c.tier_label }}</span></td>
                <td class="mono">{{ c.max_leverage }}x</td>
                <td class="mono muted" style="font-size: 10.5px">{{ c.tick_sz }} / {{ c.min_sz }}</td>
                <td class="mono">{{ fmt(c.min_notional, 2) }}</td>
                <td>
                  <span v-if="c.in_pool" class="tag tag-long">已在池</span>
                  <button v-else class="btn btn-xs" :disabled="full || adding === c.symbol" @click="add(c.symbol)">
                    {{ adding === c.symbol ? '…' : '加入' }}
                  </button>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd scan-hd">
        <span class="panel-title">当前标的池</span>
        <div class="right">
          <span class="tag tag-dim">{{ count }} 个</span>
          <span v-if="state.detail" class="tag tag-warn">{{ state.detail }}</span>
          <span class="scan-meta">
            上次扫描：{{ scan?.last_run_ts ? timeStr(scan.last_run_ts, false) : '尚未扫描' }}
            <template v-if="scan?.last_run_ts">
              · 扫描 {{ lastScan.scanned }} 个 · 候选 {{ lastScan.candidates }} 个 · 替换
              {{ lastScan.deleted }} 个 · 跳过 {{ lastScan.skipped }} 个 · 保留 {{ lastScan.kept }} 个
            </template>
          </span>
          <span v-if="scanError" class="scan-error">{{ scanError }}</span>
          <button class="btn btn-sm" :disabled="scanning" @click="runScan(false)">
            {{ scanning ? '扫描中…' : '立即扫描' }}
          </button>
        </div>
      </div>
      <div class="panel-body flush table-scroll" style="max-height: 520px">
        <table class="table">
          <thead>
            <tr>
              <th>合约</th>
              <th>分层</th>
              <th>杠杆</th>
              <th>价格精度</th>
              <th>数量步进</th>
              <th>最小名义额</th>

              <th></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in pool" :key="row.symbol">
              <td>
                <b class="mono">{{ row.symbol }}</b>
                <div class="muted" style="font-size: 10px">{{ row.name }} · ctVal {{ row.ct_val }}</div>
                <div v-if="row.note" class="muted" style="font-size: 9px">{{ row.note }}</div>
              </td>
              <td>
                <span
                  class="tag"
                  :class="row.tier === 'tier_1_bluechip' ? 'tag-blue' : row.tier === 'custom' ? 'tag-warn' : 'tag-dim'"
                >
                  {{ row.tier === 'tier_1_bluechip' ? '蓝筹主流' : row.tier === 'custom' ? '自定义 · 仅记录' : '高弹性动量' }}
                </span>
              </td>
              <td class="mono">{{ row.max_leverage }}x</td>
              <td class="mono muted">{{ row.tick_sz }}</td>
              <td class="mono muted">{{ row.min_sz }}（{{ row.precision }} 位）</td>
              <td class="mono">{{ fmt(row.min_notional, 2) }}</td>

              <td>
                <button class="btn btn-xs btn-danger" @click="openDelete(row)">移除</button>
              </td>
            </tr>
            <tr v-if="!pool.length">
              <td colspan="8" class="empty">标的池为空：在上方搜索并加入币安 USDT 永续合约</td>
            </tr>
          </tbody>
        </table>
      </div>
      <div class="panel-body" style="padding-top: 0">
        <span class="hint">
          删除必须输入确认短语 <span class="mono">REMOVE &lt;SYMBOL&gt;</span>；该合约有持仓（本地或交易所）时禁止删除，
          防止失去止损/止盈接管。
        </span>
      </div>
    </div>

    <Modal v-if="pendingDelete" :title="`移除 ${pendingDelete.symbol}`" @close="closeDelete">
      <div class="stack" style="gap: 12px">
        <div class="hint" style="line-height: 1.8">
          删除后引擎将不再扫描该合约。若该合约仍有持仓，服务器会拒绝删除。
          请输入确认短语：
          <div class="mono" style="margin-top: 6px; color: var(--warn)">{{ requiredPhrase }}</div>
        </div>
        <input v-model="phrase" class="input mono" placeholder="输入确认短语" @keyup.enter="confirmDelete" />
        <div v-if="deleteError" class="down" style="font-size: 12px">{{ deleteError }}</div>
      </div>
      <template #footer>
        <button class="btn btn-sm" :disabled="deleting" @click="closeDelete">取消</button>
        <button class="btn btn-sm btn-danger" :disabled="!canDelete || deleting" @click="confirmDelete">
          {{ deleting ? '移除中…' : '确认移除' }}
        </button>
      </template>
    </Modal>

    <Modal v-if="showScanPlan && scanPlan" title="自动换池计划" wide @close="closeScanPlan">
      <div class="stack scan-plan" style="gap: 12px">
        <div class="scan-summary">
          <span class="tag tag-dim">扫描 {{ scanPlan.scanned ?? 0 }} 个</span>
          <span class="tag tag-dim">候选 {{ scanPlan.candidates?.length || 0 }} 个</span>
          <span class="tag tag-warn">将替换 {{ scanPlan.delete?.length || 0 }} 个</span>
          <span class="tag tag-long">将新增 {{ scanPlan.add?.length || 0 }} 个</span>
        </div>

        <div class="scan-row">
          <div class="scan-row-hd">将替换（{{ scanPlan.delete?.length || 0 }}）</div>
          <div v-if="scanPlan.delete?.length" class="scan-list">
            <span v-for="symbol in scanPlan.delete" :key="symbol" class="scan-item">
              <b class="mono">{{ symbol }}</b>
              <span v-if="deleteReason(scanPlan, symbol)" class="scan-reason">{{ deleteReason(scanPlan, symbol) }}</span>
            </span>
          </div>
          <div v-else class="hint">无</div>
        </div>

        <div class="scan-row">
          <div class="scan-row-hd">将新增（{{ scanPlan.add?.length || 0 }}）</div>
          <div v-if="scanPlan.add?.length" class="scan-list">
            <span v-for="symbol in scanPlan.add" :key="symbol" class="scan-item">
              <b class="mono">{{ symbol }}</b>
            </span>
          </div>
          <div v-else class="hint">无</div>
        </div>

        <div class="scan-row">
          <div class="scan-row-hd">保留（其余）· {{ planKeepRest.length }} 个</div>
          <div v-if="planKeepRest.length" class="scan-list">
            <span v-for="symbol in planKeepRest" :key="symbol" class="scan-item">
              <b class="mono">{{ symbol }}</b>
            </span>
          </div>
          <div v-else class="hint">无</div>
        </div>

        <div class="scan-row">
          <div class="scan-row-hd">持仓保留（{{ scanPlan.held?.length || 0 }}）</div>
          <div v-if="scanPlan.held?.length" class="scan-list">
            <span v-for="symbol in scanPlan.held" :key="symbol" class="scan-item scan-item-held">
              <b class="mono">{{ symbol }}</b>
            </span>
          </div>
          <div v-else class="hint">无</div>
        </div>

        <div class="scan-row">
          <div class="scan-row-hd">休市保留（{{ scanPlan.suspended?.length || 0 }}）</div>
          <div v-if="scanPlan.suspended?.length" class="scan-list">
            <span v-for="symbol in scanPlan.suspended" :key="symbol" class="scan-item">
              <b class="mono">{{ symbol }}</b>
            </span>
          </div>
          <div v-else class="hint">无</div>
        </div>

        <div class="scan-row">
          <div class="scan-row-hd">跳过</div>
          <div v-if="skipPending" class="scan-skipped">
            <span>跳过项将在执行时二次检查</span>
          </div>
          <div v-else class="scan-skipped">
            <span>删除跳过：{{ skippedText(scanPlan.delete_skipped) }}</span>
            <span>新增跳过：{{ skippedText(scanPlan.add_skipped) }}</span>
          </div>
        </div>

        <div class="hint">
          计划仅为预览，不会自动执行。确认后将按上述替换/新增写入标的池；执行时会再次检查持仓与合约可用性，
          被跳过的标的会保留在原池。
        </div>
      </div>
      <div v-if="scanError" class="scan-error scan-error-inline" role="alert" aria-live="assertive">
        {{ scanError }}
      </div>
      <template #footer>
        <button class="btn btn-sm" :disabled="scanning" @click="closeScanPlan">取消</button>
        <button class="btn btn-sm btn-primary" :disabled="scanning" @click="runScan(true)">
          {{ scanning ? '执行中…' : '确认执行' }}
        </button>
      </template>
    </Modal>
  </div>
</template>

<style scoped>
.scan-hd {
  flex-wrap: wrap;
  row-gap: 6px;
}
.scan-hd .right {
  flex-wrap: wrap;
  justify-content: flex-end;
}
.scan-meta {
  font-size: 10.5px;
  color: var(--muted);
  white-space: normal;
}
.scan-error {
  font-size: 11px;
  color: var(--warn);
}
.scan-error-inline {
  margin-top: 10px;
  padding: 8px 10px;
  border-radius: var(--r-sm);
  border: 1px solid var(--warn);
  background: rgba(255, 176, 32, 0.1);
  line-height: 1.6;
  text-align: left;
}
.scan-plan {
  max-height: 60vh;
  overflow-y: auto;
}
.scan-summary {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.scan-row-hd {
  font-size: 11px;
  color: var(--muted);
  margin-bottom: 5px;
}
.scan-list {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.scan-item {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 3px 8px;
  border-radius: var(--r-sm);
  background: var(--panel-2);
  border: 1px solid var(--line-soft);
  font-size: 11px;
}
.scan-item-held {
  border-color: var(--up-soft);
}
.scan-reason {
  color: var(--warn);
  font-size: 10.5px;
}
.scan-skipped {
  display: flex;
  flex-wrap: wrap;
  gap: 12px;
  font-size: 11px;
  color: var(--muted);
}
</style>
