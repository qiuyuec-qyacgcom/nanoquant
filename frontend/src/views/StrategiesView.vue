<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { http } from '../api/client'
import Modal from '../components/Modal.vue'
import { useAppStore } from '../stores/app'

interface Filters {
  symbols: string[]
  side: string
}

interface Strategy {
  id: number
  name: string
  description: string
  rule: string
  enabled: boolean
  priority: number
  filters: Filters
  summary: string
}

const app = useAppStore()
const items = ref<Strategy[]>([])
const stats = ref<any>(null)
const lastApplied = ref('')
const busy = ref(false)
const showForm = ref(false)
const editing = ref<Strategy | null>(null)
const showPreview = ref(false)
const preview = ref<any>(null)

const form = reactive({
  name: '',
  description: '',
  rule: '',
  enabled: true,
  priority: 100,
  side: 'any',
  symbols: '',
})

const enabledCount = computed(() => Number(stats.value?.enabled || 0))
const isDefault = computed(() => Boolean(stats.value?.default))

async function load() {
  busy.value = true
  try {
    const res = await http.get('/strategies')
    items.value = res.items || []
    stats.value = res.stats
    lastApplied.value = res.last_applied || ''
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

function openCreate() {
  editing.value = null
  Object.assign(form, {
    name: '', description: '', rule: '', enabled: true, priority: 100, side: 'any', symbols: '',
  })
  showForm.value = true
}

function openEdit(item: Strategy) {
  editing.value = item
  const f = item.filters || ({} as Filters)
  Object.assign(form, {
    name: item.name,
    description: item.description || '',
    rule: item.rule || '',
    enabled: item.enabled,
    priority: item.priority,
    side: f.side || 'any',
    symbols: (f.symbols || []).join(','),
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
    rule: form.rule.trim(),
    enabled: form.enabled,
    priority: Number(form.priority) || 0,
    filters: { symbols, side: form.side },
  }
}

async function save() {
  if (!form.name.trim()) {
    app.notify('请填写策略名称', 'error')
    return
  }
  if (form.rule.trim().length < 8) {
    app.notify('策略内容至少 8 个字，AI 需要可执行的判断依据', 'error')
    return
  }
  busy.value = true
  try {
    if (editing.value) await http.patch(`/strategies/${editing.value.id}`, buildPayload())
    else await http.post('/strategies', buildPayload())
    showForm.value = false
    app.notify('策略已保存，下一轮 AI 决策生效', 'ok')
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
  if (!confirm(`删除策略「${item.name}」？`)) return
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
        <div class="kpi-label">AI 策略数</div>
        <div class="kpi-value">{{ stats?.total ?? 0 }}</div>
        <div class="kpi-foot">启用 {{ enabledCount }} 条</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">当前分析模式</div>
        <div class="kpi-value" style="font-size: 17px">{{ isDefault ? '默认通用分析' : '按策略分析' }}</div>
        <div class="kpi-foot">{{ isDefault ? '未启用策略，AI 按通用铁律判断' : '命中策略时按策略正文分析' }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">最近应用</div>
        <div class="kpi-value" style="font-size: 15px">{{ lastApplied || '默认' }}</div>
        <div class="kpi-foot">最近一次 AI 决策命中的策略</div>
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
      本页是 <b>AI 分析指令</b>：引擎照常扫描全部标的，决策时把适用策略的正文交给模型，由它按策略判断是否开仓、方向与节奏。
      一条策略都没启用、或标的不在适用范围时，AI 按通用铁律分析（默认行为）。<b>这里改不了杠杆、止损与仓位</b>——那些是风控铁律。
      要调机器实际执行的模式、周期、杠杆区间、止损止盈，去
      <router-link to="/admin/strategy?group=strategy">参数配置 → 策略</router-link>。
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">AI 策略列表</span>
        <div class="right" style="gap: 8px">
          <span class="tag tag-dim">{{ items.length }} 条</span>
          <button class="btn btn-sm" :disabled="busy" @click="runPreview">预览适用标的</button>
          <button class="btn btn-sm btn-primary" :disabled="busy" @click="openCreate">新增 AI 策略</button>
        </div>
      </div>
      <div class="panel-body flush table-scroll">
        <table class="table">
          <thead>
            <tr>
              <th style="width: 64px">优先级</th>
              <th style="width: 150px">名称</th>
              <th>策略内容</th>
              <th style="width: 130px">适用范围</th>
              <th style="width: 80px">本轮</th>
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
              <td class="muted" style="white-space: normal; max-width: 520px">
                {{ item.rule.length > 90 ? item.rule.slice(0, 90) + '…' : item.rule }}
              </td>
              <td class="muted" style="white-space: normal">{{ item.summary }}</td>
              <td>
                <span class="tag" :class="lastApplied === item.name ? 'tag-long' : 'tag-dim'">
                  {{ lastApplied === item.name ? '已应用' : '待命中' }}
                </span>
              </td>
              <td><button class="switch" :class="{ on: item.enabled }" @click="toggle(item)" /></td>
              <td class="right">
                <button class="btn btn-xs" :disabled="busy" @click="openEdit(item)">编辑</button>
                <button class="btn btn-xs btn-danger" :disabled="busy" style="margin-left: 6px" @click="remove(item)">删除</button>
              </td>
            </tr>
            <tr v-if="!items.length">
              <td colspan="7" class="empty">
                还没有策略 · 当前 AI 走默认通用分析；新增一条即可让它按你的打法判断
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
            <input v-model="form.name" class="input" placeholder="例如：主流币放量突破跟随" />
          </div>
          <div class="field">
            <span class="label">优先级（越小越先命中）</span>
            <input v-model.number="form.priority" type="number" min="0" step="10" class="input mono" />
          </div>
        </div>
        <div class="field">
          <span class="label">说明</span>
          <input v-model="form.description" class="input" placeholder="可选，记录这条策略的意图" />
        </div>
        <div class="field">
          <span class="label">策略内容（交给 AI 的分析指令）</span>
          <textarea
            v-model="form.rule"
            class="input"
            rows="6"
            placeholder="例如：只在 1h 趋势向上、15m 回踩 EMA21 不破且量比 > 1.2 时做多；入场后 30 分钟内若未突破前高则减半；跌破回踩低点止损；逆势与放量下跌一律不做。"
          />
          <div class="hint">写清入场条件、方向、节奏与回避情形；AI 会按这段正文分析，但不能覆盖杠杆/止损/仓位风控。</div>
        </div>
        <div class="grid g2" style="gap: 12px">
          <div class="field">
            <span class="label">适用标的</span>
            <input v-model="form.symbols" class="input" placeholder="留空=全池；如 BTCUSDT,ETHUSDT" />
          </div>
          <div class="field">
            <span class="label">方向偏好</span>
            <select v-model="form.side" class="select">
              <option value="any">不限（多空都做）</option>
              <option value="long">只做多（AI 反向提案会被拒）</option>
              <option value="short">只做空（AI 反向提案会被拒）</option>
            </select>
          </div>
        </div>
        <div class="row">
          <button class="switch" :class="{ on: form.enabled }" @click="form.enabled = !form.enabled" />
          <span class="muted" style="font-size: 11.5px">启用（关闭后该策略不参与 AI 决策）</span>
        </div>
      </div>
      <template #footer>
        <button class="btn" @click="showForm = false">取消</button>
        <button class="btn btn-primary" :disabled="busy" @click="save">保存</button>
      </template>
    </Modal>

    <Modal v-if="showPreview" title="策略适用范围预览" wide @close="showPreview = false">
      <div class="stack">
        <div class="row" style="flex-wrap: wrap; gap: 8px">
          <span class="tag" :class="preview?.default ? 'tag-warn' : 'tag-long'">
            {{ preview?.default ? '未启用策略 · 全部走默认分析' : `按 ${preview?.profiles ?? 0} 条策略匹配` }}
          </span>
          <span class="tag tag-dim">标的池 {{ (preview?.items || []).length }} 个</span>
        </div>
        <div class="panel-body flush table-scroll" style="max-height: 420px">
          <table class="table">
            <thead><tr><th>标的</th><th>适用策略</th><th>方向</th></tr></thead>
            <tbody>
              <tr v-for="row in preview?.items || []" :key="row.symbol">
                <td class="mono">{{ row.symbol }}</td>
                <td>
                  <span class="tag" :class="row.default ? 'tag-dim' : 'tag-blue'">{{ row.profile || '默认分析' }}</span>
                </td>
                <td class="muted">{{ row.side === 'long' ? '只做多' : row.side === 'short' ? '只做空' : '不限' }}</td>
              </tr>
              <tr v-if="!(preview?.items || []).length"><td colspan="3" class="empty">标的池为空</td></tr>
            </tbody>
          </table>
        </div>
        <div class="hint">预览只做本地匹配，不拉行情、不消耗交易所权重。</div>
      </div>
    </Modal>
  </div>
</template>
