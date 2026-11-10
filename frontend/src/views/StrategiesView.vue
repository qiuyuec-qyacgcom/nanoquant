<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { http } from '../api/client'
import Modal from '../components/Modal.vue'
import { useAppStore } from '../stores/app'
import { fmt } from '../utils/format'

interface Filters {
  symbols: string[]
  side: string
  min_quote_volume: number
  min_price: number
  max_price: number
  change_24h_min: number
  change_24h_max: number
  max_spread_bps: number
}

interface Strategy {
  id: number
  name: string
  description: string
  enabled: boolean
  priority: number
  filters: Filters
  summary: string
}

const app = useAppStore()
const items = ref<Strategy[]>([])
const stats = ref<any>(null)
const lastFilter = ref<any>({})
const busy = ref(false)
const showForm = ref(false)
const editing = ref<Strategy | null>(null)
const showPreview = ref(false)
const preview = ref<any>(null)

const form = reactive({
  name: '',
  description: '',
  enabled: true,
  priority: 100,
  side: 'any',
  symbols: '',
  min_quote_volume: 0,
  min_price: 0,
  max_price: 0,
  change_24h_min: -100,
  change_24h_max: 100,
  max_spread_bps: 0,
})

const enabledCount = computed(() => Number(stats.value?.enabled || 0))
const isDefault = computed(() => Boolean(stats.value?.default))
const passedCount = computed(() => (lastFilter.value?.kept || []).length)
const droppedCount = computed(() => (lastFilter.value?.dropped || []).length)

async function load() {
  busy.value = true
  try {
    const res = await http.get('/strategies')
    items.value = res.items || []
    stats.value = res.stats
    lastFilter.value = res.last_filter || {}
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

function openCreate() {
  editing.value = null
  Object.assign(form, {
    name: '', description: '', enabled: true, priority: 100, side: 'any', symbols: '',
    min_quote_volume: 0, min_price: 0, max_price: 0,
    change_24h_min: -100, change_24h_max: 100, max_spread_bps: 0,
  })
  showForm.value = true
}

function openEdit(item: Strategy) {
  editing.value = item
  const f = item.filters || ({} as Filters)
  Object.assign(form, {
    name: item.name,
    description: item.description || '',
    enabled: item.enabled,
    priority: item.priority,
    side: f.side || 'any',
    symbols: (f.symbols || []).join(','),
    min_quote_volume: f.min_quote_volume || 0,
    min_price: f.min_price || 0,
    max_price: f.max_price || 0,
    change_24h_min: f.change_24h_min ?? -100,
    change_24h_max: f.change_24h_max ?? 100,
    max_spread_bps: f.max_spread_bps || 0,
  })
  showForm.value = true
}

function buildPayload() {
  const symbols = form.symbols
    .split(/[,，\s]+/)
    .map((s) => s.trim().toUpperCase())
    .filter(Boolean)
  return {
    name: form.name.trim(),
    description: form.description.trim(),
    enabled: form.enabled,
    priority: Number(form.priority) || 0,
    filters: {
      symbols,
      side: form.side,
      min_quote_volume: Number(form.min_quote_volume) || 0,
      min_price: Number(form.min_price) || 0,
      max_price: Number(form.max_price) || 0,
      change_24h_min: Number(form.change_24h_min),
      change_24h_max: Number(form.change_24h_max),
      max_spread_bps: Number(form.max_spread_bps) || 0,
    },
  }
}

async function save() {
  if (!form.name.trim()) {
    app.notify('请填写策略名称', 'error')
    return
  }
  busy.value = true
  try {
    if (editing.value) await http.patch(`/strategies/${editing.value.id}`, buildPayload())
    else await http.post('/strategies', buildPayload())
    showForm.value = false
    app.notify('策略已保存，下一轮扫描生效', 'ok')
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

async function toggle(item: Strategy) {
  try {
    await http.patch(`/strategies/${item.id}`, { enabled: !item.enabled })
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

async function remove(item: Strategy) {
  if (!confirm(`删除策略「${item.name}」？删除后该策略不再参与粗筛。`)) return
  busy.value = true
  try {
    await http.del(`/strategies/${item.id}`)
    app.notify('策略已删除', 'ok')
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

async function runPreview() {
  busy.value = true
  try {
    preview.value = await http.get('/strategies/preview')
    if (preview.value.ok === false) app.notify(preview.value.error || '预览失败', 'error')
    showPreview.value = true
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

onMounted(load)
</script>

<template>
  <div class="stack">
    <div class="kpi-grid">
      <div class="kpi">
        <div class="kpi-label">策略总数</div>
        <div class="kpi-value">{{ stats?.total ?? 0 }}</div>
        <div class="kpi-foot">启用 {{ enabledCount }} 条</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">当前模式</div>
        <div class="kpi-value" style="font-size: 18px">{{ isDefault ? '默认全池' : '策略筛选' }}</div>
        <div class="kpi-foot">{{ isDefault ? '未启用策略，轮询全部标的' : '按优先级匹配，命中才进深度扫描' }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">最近一轮通过</div>
        <div class="kpi-value">{{ passedCount }}</div>
        <div class="kpi-foot">过滤 {{ droppedCount }} 个标的</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">方向偏好</div>
        <div class="kpi-value" style="font-size: 13px; display: flex; gap: 5px; flex-wrap: wrap; margin-top: 8px">
          <span class="tag tag-dim">只做多 {{ stats?.by_side?.long ?? 0 }}</span>
          <span class="tag tag-dim">只做空 {{ stats?.by_side?.short ?? 0 }}</span>
          <span class="tag tag-dim">不限 {{ stats?.by_side?.any ?? 0 }}</span>
        </div>
      </div>
    </div>

    <div class="note">
      引擎每轮先按策略粗筛标的池，命中才拉 K 线做深度分析；按优先级从小到大匹配，第一条命中即放行。
      一条策略都没启用时走默认：全池轮询（与旧行为一致）。粗筛只读 24h 行情与可选盘口点差，不额外消耗 K 线权重。
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">策略列表</span>
        <div class="right" style="gap: 8px">
          <span class="tag tag-dim">{{ items.length }} 条</span>
          <button class="btn btn-sm" :disabled="busy" @click="runPreview">预览放行标的</button>
          <button class="btn btn-sm btn-primary" :disabled="busy" @click="openCreate">新增策略</button>
        </div>
      </div>
      <div class="panel-body flush table-scroll">
        <table class="table">
          <thead>
            <tr>
              <th style="width: 64px">优先级</th>
              <th>名称</th>
              <th>条件</th>
              <th style="width: 72px">本轮命中</th>
              <th style="width: 64px">启用</th>
              <th style="width: 110px"></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="item in items" :key="item.id">
              <td class="mono muted">{{ item.priority }}</td>
              <td>
                <div>{{ item.name }}</div>
                <div v-if="item.description" class="muted" style="font-size: 10.5px">{{ item.description }}</div>
              </td>
              <td class="muted" style="white-space: normal; max-width: 560px">{{ item.summary }}</td>
              <td class="mono">{{ lastFilter?.matched?.[item.id] ?? 0 }}</td>
              <td><button class="switch" :class="{ on: item.enabled }" @click="toggle(item)" /></td>
              <td class="right">
                <button class="btn btn-xs" :disabled="busy" @click="openEdit(item)">编辑</button>
                <button class="btn btn-xs btn-danger" :disabled="busy" style="margin-left: 6px" @click="remove(item)">删除</button>
              </td>
            </tr>
            <tr v-if="!items.length">
              <td colspan="6" class="empty">
                还没有策略 · 当前默认全池轮询；新增一条即可只分析符合条件的标的
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <Modal v-if="showForm" :title="editing ? '编辑策略' : '新增策略'" wide @close="showForm = false">
      <div class="stack">
        <div class="grid g2" style="gap: 12px">
          <div class="field">
            <span class="label">名称</span>
            <input v-model="form.name" class="input" placeholder="例如：主流币放量突破" />
          </div>
          <div class="field">
            <span class="label">优先级（越小越先匹配）</span>
            <input v-model.number="form.priority" type="number" min="0" step="10" class="input mono" />
          </div>
        </div>
        <div class="field">
          <span class="label">说明</span>
          <input v-model="form.description" class="input" placeholder="可选，记录这条策略的意图" />
        </div>
        <div class="row">
          <button class="switch" :class="{ on: form.enabled }" @click="form.enabled = !form.enabled" />
          <span class="muted" style="font-size: 11.5px">启用（关闭后不参与粗筛）</span>
        </div>

        <div class="section-title">粗筛条件（全部满足才命中）</div>
        <div class="grid g2" style="gap: 12px">
          <div class="field">
            <span class="label">标的白名单</span>
            <input v-model="form.symbols" class="input" placeholder="留空=全池；如 BTCUSDT,ETHUSDT" />
          </div>
          <div class="field">
            <span class="label">方向偏好</span>
            <select v-model="form.side" class="select">
              <option value="any">不限（多空都做）</option>
              <option value="long">只做多</option>
              <option value="short">只做空</option>
            </select>
          </div>
          <div class="field">
            <span class="label">24h 成交额下限 (USDT)</span>
            <input v-model.number="form.min_quote_volume" type="number" min="0" step="1000000" class="input mono" />
            <div class="hint">例如 5000000 = 500 万；0 = 不限</div>
          </div>
          <div class="field">
            <span class="label">盘口点差上限 (bps)</span>
            <input v-model.number="form.max_spread_bps" type="number" min="0" max="500" step="1" class="input mono" />
            <div class="hint">超过则过滤；0 = 不检查（不额外拉盘口）</div>
          </div>
          <div class="field">
            <span class="label">最新价下限 / 上限</span>
            <div class="row" style="gap: 6px">
              <input v-model.number="form.min_price" type="number" min="0" step="any" class="input mono" placeholder="0=不限" />
              <input v-model.number="form.max_price" type="number" min="0" step="any" class="input mono" placeholder="0=不限" />
            </div>
          </div>
          <div class="field">
            <span class="label">24h 涨跌幅区间 (%)</span>
            <div class="row" style="gap: 6px">
              <input v-model.number="form.change_24h_min" type="number" step="any" class="input mono" />
              <input v-model.number="form.change_24h_max" type="number" step="any" class="input mono" />
            </div>
            <div class="hint">默认 -100~100 表示不限</div>
          </div>
        </div>
        <div class="hint">
          粗筛只用 24h 行情（涨跌/成交额/最新价）与可选盘口点差；RSI、ATR、信号分等需要 K 线的条件在粗筛之后仍按全局策略参数判断。
        </div>
      </div>
      <template #footer>
        <button class="btn" @click="showForm = false">取消</button>
        <button class="btn btn-primary" :disabled="busy" @click="save">保存</button>
      </template>
    </Modal>

    <Modal v-if="showPreview" title="策略预览（当前行情）" wide @close="showPreview = false">
      <div class="stack">
        <div class="row" style="flex-wrap: wrap; gap: 8px">
          <span class="tag" :class="preview?.default ? 'tag-warn' : 'tag-long'">
            {{ preview?.default ? '默认全池（未启用策略）' : `按 ${preview?.summary?.profiles ?? 0} 条策略筛选` }}
          </span>
          <span class="tag tag-dim">通过 {{ preview?.summary?.kept?.length ?? 0 }}</span>
          <span class="tag tag-dim">过滤 {{ preview?.summary?.dropped?.length ?? 0 }}</span>
          <span v-if="preview?.ok === false" class="tag tag-short">{{ preview?.error }}</span>
        </div>
        <div class="panel-body flush table-scroll" style="max-height: 420px">
          <table class="table">
            <thead>
              <tr><th>标的</th><th>最新价</th><th>24h 涨跌</th><th>24h 成交额</th><th>结果</th><th>方向</th></tr>
            </thead>
            <tbody>
              <tr v-for="row in preview?.items || []" :key="row.symbol">
                <td class="mono">{{ row.symbol }}</td>
                <td class="mono">{{ fmt(row.price) }}</td>
                <td class="mono" :class="row.change_24h_pct >= 0 ? 'up' : 'down'">{{ row.change_24h_pct }}%</td>
                <td class="mono muted">{{ fmt(row.quote_volume) }}</td>
                <td>
                  <span class="tag" :class="row.kept ? 'tag-long' : 'tag-dim'">{{ row.kept ? '放行' : '过滤' }}</span>
                </td>
                <td class="muted">{{ row.side_bias === 'long' ? '只做多' : row.side_bias === 'short' ? '只做空' : '不限' }}</td>
              </tr>
              <tr v-if="!(preview?.items || []).length"><td colspan="6" class="empty">标的池为空</td></tr>
            </tbody>
          </table>
        </div>
        <div class="hint">预览只读，不会修改策略；结果基于当前 24h 行情，可能与下一轮引擎扫描略有差异。</div>
      </div>
    </Modal>
  </div>
</template>
