<script setup lang="ts">
/**
 * 决策经验 (日志 → 打标 → RAG 经验库)
 * -----------------------------------------------------------------
 * 借鉴「会自我学习的 Agent」的闭环骨架，但去掉收益叙事、只留可验证工程：
 * 1. 日志 —— AI 每次方向判断留一份快照；
 * 2. 打标 —— 24h 后回访，把预测方向 vs 实际价格对齐成对/错/部分；
 * 3. 经验库 —— 打标结论带标签归档，下次相似场景检索 Top-k 注入 AI 决策。
 * 与「自进化心法」互补：那里是平仓复盘 → 规则级心法，这里是决策级 + 场景化。
 */
import { computed, onMounted, ref } from 'vue'
import { http } from '../api/client'
import { useAppStore } from '../stores/app'
import { pct, timeStr } from '../utils/format'

const app = useAppStore()
const journal = ref<any[]>([])
const journalStats = ref<any>(null)
const experiences = ref<any[]>([])
const expStats = ref<any>(null)
const filterStatus = ref('')
const busy = ref(false)

const accuracy = computed(() => journalStats.value?.accuracy ?? 0)

const DIR_LABEL: Record<string, string> = { long: '做多', short: '做空', flat: '观望' }
const OUTCOME_LABEL: Record<string, string> = { correct: '判断正确', wrong: '判断错误', partial: '部分正确' }
const STATUS_LABEL: Record<string, string> = { pending: '待回访', labeled: '已打标', skipped: '留痕' }

async function load() {
  const qs = filterStatus.value ? `&status=${filterStatus.value}` : ''
  const [j, e] = await Promise.allSettled([
    http.get(`/journal?limit=200${qs}`),
    http.get('/journal/experiences?limit=300'),
  ])
  if (j.status === 'fulfilled') {
    journal.value = j.value.items
    journalStats.value = j.value.stats
  }
  if (e.status === 'fulfilled') {
    experiences.value = e.value.items
    expStats.value = e.value.stats
  }
}

function onFilter() {
  load()
}

async function labelNow() {
  busy.value = true
  try {
    const res = await http.post('/journal/label')
    app.notify(`回访打标完成: ${res.labeled ?? 0} 条（到期 ${res.due ?? 0}）`, 'ok')
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

async function archiveNow() {
  busy.value = true
  try {
    const res = await http.post('/journal/archive')
    app.notify(`经验归档: 新增 ${res.archived ?? 0} · 合并 ${res.merged ?? 0}`, 'ok')
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

async function remove(row: any) {
  if (!confirm(`删除经验「${row.symbol}」?`)) return
  try {
    await http.del(`/journal/experiences/${row.id}`)
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

onMounted(load)
</script>

<template>
  <div class="stack">
    <div class="kpi-grid">
      <div class="kpi">
        <div class="kpi-label">决策总数</div>
        <div class="kpi-value">{{ journalStats?.total ?? 0 }}</div>
        <div class="kpi-foot">待回访 {{ journalStats?.pending ?? 0 }} · 已打标 {{ journalStats?.labeled ?? 0 }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">方向判断准确率</div>
        <div class="kpi-value">{{ accuracy.toFixed(1) }}<span class="muted" style="font-size: 14px">%</span></div>
        <div class="kpi-foot">对 {{ journalStats?.correct ?? 0 }} · 错 {{ journalStats?.wrong ?? 0 }} · 部分 {{ journalStats?.partial ?? 0 }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">经验库</div>
        <div class="kpi-value">{{ expStats?.enabled ?? 0 }} <span class="muted" style="font-size: 14px">/ {{ expStats?.max ?? 200 }}</span></div>
        <div class="kpi-foot">胜 {{ expStats?.wins ?? 0 }} · 负 {{ expStats?.losses ?? 0 }} · 停用 {{ expStats?.disabled ?? 0 }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">检索注入</div>
        <div class="kpi-value">{{ expStats?.retrieve_top_k ?? 5 }}</div>
        <div class="kpi-foot">相似场景 Top-k · 与决策同源打分</div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">决策日志（回访打标）</span>
        <div class="right" style="gap: 6px; flex-wrap: wrap">
          <select v-model="filterStatus" class="select" style="width: 110px; padding: 4px 8px" @change="onFilter">
            <option value="">全部状态</option>
            <option value="pending">待回访</option>
            <option value="labeled">已打标</option>
            <option value="skipped">留痕</option>
          </select>
          <button class="btn btn-sm btn-primary" :disabled="busy" @click="labelNow">手动打标</button>
          <button class="btn btn-sm" :disabled="busy" @click="archiveNow">手动归档</button>
        </div>
      </div>
      <div class="hint" style="padding: 8px 12px 0">
        每次 AI 给出方向判断都会留一份快照；24h 后自动回访实际价格，把「预测方向 vs 实际」对齐成对/错。
        hold（否决）只留痕不参与对赌，以免把「不追多」误判成可验证的方向预测。
      </div>
      <div class="panel-body flush table-scroll" style="max-height: 440px">
        <table class="table">
          <thead>
            <tr><th>时间</th><th>标的</th><th>方向</th><th>信心</th><th>目标</th><th>实际</th><th>结论</th><th>状态</th></tr>
          </thead>
          <tbody>
            <tr v-for="r in journal" :key="r.id">
              <td class="mono muted">{{ timeStr(r.ts, false) }}</td>
              <td class="mono">{{ r.symbol }}</td>
              <td><span class="tag" :class="r.direction === 'long' ? 'tag-long' : r.direction === 'short' ? 'tag-short' : 'tag-dim'">{{ DIR_LABEL[r.direction] || r.direction }}</span></td>
              <td class="mono">{{ (r.confidence * 100).toFixed(0) }}%</td>
              <td class="mono muted">{{ r.target_pct > 0 ? '+' : '' }}{{ r.target_pct?.toFixed(2) }}%</td>
              <td class="mono" :class="r.actual_pct >= 0 ? 'up' : 'down'">{{ r.actual_price ? (r.actual_pct > 0 ? '+' : '') + r.actual_pct.toFixed(2) + '%' : '—' }}</td>
              <td>
                <span v-if="r.outcome" class="tag" :class="r.outcome === 'correct' ? 'tag-long' : r.outcome === 'wrong' ? 'tag-warn' : 'tag-dim'">
                  {{ OUTCOME_LABEL[r.outcome] || r.outcome }}
                </span>
                <span v-else class="muted">—</span>
              </td>
              <td><span class="tag tag-dim">{{ STATUS_LABEL[r.status] || r.status }}</span></td>
            </tr>
            <tr v-if="!journal.length"><td colspan="8" class="empty">暂无决策记录 · 开启 AI 后每次方向判断会自动留痕</td></tr>
          </tbody>
        </table>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">经验库（场景化 RAG）</span>
        <div class="right"><span class="tag tag-dim">同币种 + 标签 Jaccard + 数值场景相似度</span></div>
      </div>
      <div class="panel-body flush table-scroll" style="max-height: 440px">
        <table class="table">
          <thead>
            <tr><th>标的</th><th>标签</th><th>经验</th><th>结果</th><th>命中/胜负</th><th>时间</th><th></th></tr>
          </thead>
          <tbody>
            <tr v-for="e in experiences" :key="e.id">
              <td class="mono">{{ e.symbol }}</td>
              <td style="max-width: 220px">
                <div class="row" style="gap: 3px; flex-wrap: wrap">
                  <span v-for="t in (e.tags || []).slice(0, 5)" :key="t" class="tag tag-dim" style="font-size: 9.5px">{{ t }}</span>
                </div>
              </td>
              <td class="muted" style="white-space: normal; max-width: 460px; font-size: 11.5px">{{ e.lesson }}</td>
              <td><span class="tag" :class="e.outcome === 'win' ? 'tag-long' : 'tag-warn'">{{ e.outcome === 'win' ? '胜' : '负' }}</span></td>
              <td class="mono muted">{{ e.hits }} / {{ e.wins }}-{{ e.losses }}</td>
              <td class="mono muted">{{ timeStr(e.created_at, false) }}</td>
              <td class="right"><button class="btn btn-xs btn-danger" @click="remove(e)">删</button></td>
            </tr>
            <tr v-if="!experiences.length"><td colspan="7" class="empty">经验库为空 · 决策打标后会自动沉淀</td></tr>
          </tbody>
        </table>
      </div>
    </div>
  </div>
</template>
