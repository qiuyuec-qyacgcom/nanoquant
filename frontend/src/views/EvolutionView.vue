<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { http } from '../api/client'
import Modal from '../components/Modal.vue'
import { useAppStore } from '../stores/app'
import type { Mantra, Trade } from '../types'
import { CATEGORY_LABELS, EXIT_LABELS, pct, timeStr, usdt } from '../utils/format'

const app = useAppStore()
const mantras = ref<Mantra[]>([])
const stats = ref<any>(null)
const injection = ref<any>(null)
const runs = ref<any[]>([])
const trades = ref<Trade[]>([])
const category = ref('')
const busy = ref(false)
const showForm = ref(false)
const editing = ref<Mantra | null>(null)
const reviewResult = ref<any>(null)
const form = reactive({ title: '', rule: '', category: 'entry', confidence: 0.55 })

const filtered = computed(() => (category.value ? mantras.value.filter((m) => m.category === category.value) : mantras.value))

async function load() {
  const [m, inj, r, t] = await Promise.allSettled([
    http.get('/evolution/mantras?limit=300'),
    http.get('/evolution/injection'),
    http.get('/evolution/runs?limit=20'),
    http.get('/engine/trades?limit=12'),
  ])
  if (m.status === 'fulfilled') {
    mantras.value = m.value.items
    stats.value = m.value.stats
  }
  if (inj.status === 'fulfilled') injection.value = inj.value
  if (r.status === 'fulfilled') runs.value = r.value.items
  if (t.status === 'fulfilled') trades.value = t.value.items
}

function openCreate() {
  editing.value = null
  Object.assign(form, { title: '', rule: '', category: 'entry', confidence: 0.55 })
  showForm.value = true
}

function openEdit(m: Mantra) {
  editing.value = m
  Object.assign(form, { title: m.title, rule: m.rule, category: m.category, confidence: m.confidence })
  showForm.value = true
}

async function save() {
  try {
    if (editing.value) await http.patch(`/evolution/mantras/${editing.value.id}`, { ...form })
    else await http.post('/evolution/mantras', { ...form })
    showForm.value = false
    app.notify('心法已保存', 'ok')
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

async function toggle(m: Mantra) {
  try {
    await http.patch(`/evolution/mantras/${m.id}`, { enabled: !m.enabled })
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

async function remove(m: Mantra) {
  if (!confirm(`删除心法「${m.title}」?`)) return
  try {
    await http.del(`/evolution/mantras/${m.id}`)
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

async function globalReview() {
  busy.value = true
  reviewResult.value = null
  try {
    reviewResult.value = await http.post('/evolution/global-review')
    app.notify('全局复盘完成', 'ok')
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

async function reviewTrade(t: Trade) {
  busy.value = true
  try {
    const res = await http.post(`/evolution/review/${t.id}?force=true`)
    app.notify(`复盘完成, 产出 ${res.applied?.length || 0} 条`, 'ok')
    await load()
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
        <div class="kpi-label">心法总数</div>
        <div class="kpi-value">{{ stats?.total ?? 0 }}</div>
        <div class="kpi-foot">生效中 {{ stats?.active ?? 0 }} · 已替代 {{ stats?.superseded ?? 0 }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">平均置信度</div>
        <div class="kpi-value">{{ ((stats?.avg_confidence ?? 0) * 100).toFixed(0) }}%</div>
        <div class="kpi-foot">有效 TTL {{ stats?.ttl_days ?? 7 }} 天</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">注入上限</div>
        <div class="kpi-value">{{ stats?.max_injected ?? 8 }}</div>
        <div class="kpi-foot">按健康分排序取前 N 条</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">分类分布</div>
        <div class="kpi-value" style="font-size: 13px; display: flex; gap: 5px; flex-wrap: wrap; margin-top: 8px">
          <span v-for="(v, k) in stats?.by_category || {}" :key="k" class="tag tag-dim">{{ CATEGORY_LABELS[String(k)] || k }} {{ v }}</span>
          <span v-if="!Object.keys(stats?.by_category || {}).length" class="muted">暂无</span>
        </div>
      </div>
    </div>

    <div class="grid g2">
      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">当前注入 Prompt 的心法</span>
          <div class="right"><span class="tag tag-blue">{{ injection?.count ?? 0 }} 条生效</span></div>
        </div>
        <div class="panel-body">
          <pre v-if="injection?.text" class="code">{{ injection.text }}</pre>
          <div v-else class="empty">暂无可注入心法 · 完成交易复盘后会自动沉淀</div>
          <div class="hint" style="margin-top: 8px">这里是真正进入模型上下文的文本，避免「面板显示 A、实际注入 B」。</div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">复盘动作</span>
          <div class="right">
            <button class="btn btn-sm" :disabled="busy" @click="globalReview">{{ busy ? '执行中…' : '立即全局复盘' }}</button>
          </div>
        </div>
        <div class="panel-body flush table-scroll" style="max-height: 250px">
          <table class="table">
            <thead><tr><th>时间</th><th>标的</th><th>盈亏</th><th>出场</th><th>复盘</th><th></th></tr></thead>
            <tbody>
              <tr v-for="t in trades" :key="t.id">
                <td class="mono muted">{{ timeStr(t.closed_at, false) }}</td>
                <td>{{ t.symbol }}</td>
                <td class="mono" :class="t.pnl >= 0 ? 'up' : 'down'">{{ usdt(t.pnl, 5) }}</td>
                <td class="muted">{{ EXIT_LABELS[t.exit_reason] || t.exit_reason }}</td>
                <td><span class="tag" :class="t.reviewed ? 'tag-long' : 'tag-dim'">{{ t.reviewed ? '已复盘' : '待复盘' }}</span></td>
                <td class="right"><button class="btn btn-xs" :disabled="busy" @click="reviewTrade(t)">复盘</button></td>
              </tr>
              <tr v-if="!trades.length"><td colspan="6" class="empty">还没有闭环交易</td></tr>
            </tbody>
          </table>
        </div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">分析心法库</span>
        <div class="right">
          <select v-model="category" class="select" style="width: 120px; padding: 4px 8px">
            <option value="">全部分类</option>
            <option v-for="(label, key) in CATEGORY_LABELS" :key="key" :value="key">{{ label }}</option>
          </select>
          <button class="btn btn-sm btn-primary" @click="openCreate">+ 手工新增</button>
        </div>
      </div>
      <div class="panel-body flush table-scroll" style="max-height: 560px">
        <table class="table">
          <thead>
            <tr><th>标题 / 规则</th><th>分类</th><th>置信</th><th>健康</th><th>命中/胜负</th><th>版本</th><th>来源</th><th>状态</th><th>操作</th></tr>
          </thead>
          <tbody>
            <tr v-for="m in filtered" :key="m.id">
              <td style="white-space: normal; max-width: 460px">
                <div style="font-weight: 600; font-size: 12px">{{ m.title }}</div>
                <div class="muted" style="font-size: 11px; line-height: 1.6; margin-top: 2px">{{ m.rule }}</div>
              </td>
              <td><span class="tag tag-dim">{{ CATEGORY_LABELS[m.category] || m.category }}</span></td>
              <td class="mono">{{ (m.confidence * 100).toFixed(0) }}%</td>
              <td>
                <div class="row" style="gap: 6px">
                  <div class="bar-track" style="width: 46px"><div class="bar-fill" :style="{ width: Math.round(m.health * 100) + '%' }" /></div>
                  <span class="mono muted" style="font-size: 10.5px">{{ (m.health * 100).toFixed(0) }}</span>
                </div>
              </td>
              <td class="mono muted">{{ m.hits }} / {{ m.wins }}-{{ m.losses }}</td>
              <td class="mono">v{{ m.version }}</td>
              <td class="muted">{{ m.source === 'manual' ? '手工' : m.source === 'global_review' ? '全局' : '复盘' }}</td>
              <td>
                <span v-if="m.superseded_by" class="tag tag-warn">被 v{{ m.version + 1 }} 替代</span>
                <span v-else-if="m.expired" class="tag tag-warn">已过期</span>
                <span v-else class="tag" :class="m.enabled ? 'tag-long' : 'tag-dim'">{{ m.enabled ? '生效' : '停用' }}</span>
              </td>
              <td>
                <div class="row" style="gap: 4px">
                  <button class="btn btn-xs" @click="toggle(m)">{{ m.enabled ? '停用' : '启用' }}</button>
                  <button class="btn btn-xs" @click="openEdit(m)">编辑</button>
                  <button class="btn btn-xs btn-danger" @click="remove(m)">删</button>
                </div>
              </td>
            </tr>
            <tr v-if="!filtered.length"><td colspan="9" class="empty">心法库为空 · 平仓后系统会自动复盘生成，也可手工新增</td></tr>
          </tbody>
        </table>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">进化执行记录</span>
        <div class="right"><span class="tag tag-dim">{{ runs.length }} 次</span></div>
      </div>
      <div class="panel-body flush table-scroll" style="max-height: 320px">
        <table class="table">
          <thead><tr><th>时间</th><th>类型</th><th>输入</th><th>新增</th><th>替代</th><th>结论</th></tr></thead>
          <tbody>
            <tr v-for="r in runs" :key="r.id">
              <td class="mono muted">{{ timeStr(r.ts) }}</td>
              <td><span class="tag" :class="r.kind === 'global_review' ? 'tag-blue' : 'tag-dim'">{{ r.kind === 'global_review' ? '全局复盘' : '逐笔复盘' }}</span></td>
              <td class="mono">{{ r.input_count }}</td>
              <td class="mono up">{{ r.produced }}</td>
              <td class="mono warn">{{ r.replaced }}</td>
              <td class="muted" style="white-space: normal; max-width: 520px">{{ r.summary }}</td>
            </tr>
            <tr v-if="!runs.length"><td colspan="6" class="empty">暂无记录</td></tr>
          </tbody>
        </table>
      </div>
    </div>

    <Modal v-if="showForm" :title="editing ? '编辑心法' : '新增心法'" @close="showForm = false">
      <div class="stack">
        <div class="field"><span class="label">标题</span><input v-model="form.title" class="input" placeholder="例如: 量能萎缩时禁止追多" /></div>
        <div class="field">
          <span class="label">规则 (需含触发条件)</span>
          <textarea v-model="form.rule" class="input" rows="4" placeholder="当 1 分钟量比 < 0.85 且 ATR 占比 < 0.2% 时，即使信号分达标也不开仓。" />
        </div>
        <div class="grid g2" style="gap: 12px">
          <div class="field">
            <span class="label">分类</span>
            <select v-model="form.category" class="select">
              <option v-for="(label, key) in CATEGORY_LABELS" :key="key" :value="key">{{ label }}</option>
            </select>
          </div>
          <div class="field"><span class="label">置信度</span><input v-model.number="form.confidence" type="number" step="0.05" min="0" max="1" class="input mono" /></div>
        </div>
        <div class="hint">含「加大杠杆 / 取消止损 / 满仓」等放大风险的表述会被防污染审计拦截。</div>
      </div>
      <template #footer>
        <button class="btn" @click="showForm = false">取消</button>
        <button class="btn btn-primary" @click="save">保存</button>
      </template>
    </Modal>

    <Modal v-if="reviewResult" title="全局复盘结果" wide @close="reviewResult = null">
      <div class="stack">
        <div class="row">
          <span class="tag" :class="reviewResult.ok ? 'tag-long' : 'tag-short'">{{ reviewResult.ok ? '完成' : '失败' }}</span>
          <span class="muted" style="font-size: 11px">{{ reviewResult.used_ai ? 'AI 参与' : '规则兜底' }} · {{ reviewResult.error }}</span>
        </div>
        <div v-if="reviewResult.diagnosis?.length" class="stack" style="gap: 4px">
          <div class="label">识别到的不足</div>
          <div v-for="d in reviewResult.diagnosis" :key="d" class="note" style="margin: 0">{{ d }}</div>
        </div>
        <div v-if="reviewResult.applied?.length" class="stack" style="gap: 4px">
          <div class="label">产出心法</div>
          <div v-for="(a, i) in reviewResult.applied" :key="i" class="row" style="font-size: 11.5px">
            <span class="tag tag-dim">{{ a.action }}</span>
            <span>{{ a.title }}</span>
            <span class="muted">{{ a.reason }}</span>
          </div>
        </div>
        <div v-if="reviewResult.retired?.length" class="stack" style="gap: 4px">
          <div class="label">淘汰</div>
          <div v-for="r in reviewResult.retired" :key="r" class="row" style="font-size: 11.5px"><span class="tag tag-warn">retire</span>{{ r }}</div>
        </div>
        <div v-if="reviewResult.stats" class="code">{{ JSON.stringify(reviewResult.stats, null, 2) }}</div>
      </div>
    </Modal>
  </div>
</template>
