<script setup lang="ts">
/**
 * 信号审计: 每轮扫描的量化信号 → 最终裁决 (可解释的拒绝原因).
 * ---------------------------------------------------------------
 * 定位: 替代「实盘矩阵」的可解释性面板。超短线最关键的是"为什么没开仓"，
 * 这里把五因子评分、AI 复核与每一刀的拒绝原因摊开看。
 */
import { computed, onMounted, ref } from 'vue'
import { http } from '../api/client'
import { numClass, pct, timeStr } from '../utils/format'

interface SignalRow {
  id: number
  ts: number
  symbol: string
  direction: string
  score: number
  features: Record<string, number> | null
  ai_action: string
  ai_confidence: number
  ai_reason: string
  decision: string
  reject_reason: string
}

const items = ref<SignalRow[]>([])
const summary = ref<any>(null)
const loading = ref(false)
const symbolFilter = ref('')
const decisionFilter = ref('')
const directionFilter = ref('')

const DECISION_LABELS: Record<string, { text: string; cls: string }> = {
  opened: { text: '已开仓', cls: 'tag-long' },
  rejected: { text: '被拒', cls: 'tag-warn' },
  flat: { text: '平盘', cls: 'tag-dim' },
  pending: { text: '待定', cls: 'tag-blue' },
}

const DIRECTION_LABELS: Record<string, string> = { long: '多', short: '空', flat: '—' }

function feat(row: SignalRow, key: string): string {
  const v = row.features?.[key]
  return v === undefined || v === null ? '--' : String(v)
}

function featureLine(row: SignalRow): string {
  const f = row.features || {}
  const rsi = f.rsi != null ? `RSI ${Number(f.rsi).toFixed(1)}` : ''
  const atr = f.atr_pct != null ? `ATR ${Number(f.atr_pct).toFixed(2)}%` : ''
  const vol = f.vol_ratio != null ? `量比 ${Number(f.vol_ratio).toFixed(2)}` : ''
  const ret = f.ret_5 != null ? `5m ${pct(Number(f.ret_5))}` : ''
  return [rsi, atr, vol, ret].filter(Boolean).join(' · ') || '--'
}

const topReasons = computed(() => summary.value?.top_reasons || [])
const maxReason = computed(() =>
  Math.max(1, ...topReasons.value.map((r: any) => Number(r.count) || 0))
)

/** 近 7 天信号密度（按小时），用来发现信号突然变多/变少 */
const byHour = computed(() => summary.value?.by_hour || [])
const maxHour = computed(() =>
  Math.max(1, ...byHour.value.map((b: any) => Number(b.count) || 0))
)

async function load() {
  loading.value = true
  const params = new URLSearchParams()
  params.set('limit', '200')
  if (symbolFilter.value) params.set('symbol', symbolFilter.value.toUpperCase())
  if (decisionFilter.value) params.set('decision', decisionFilter.value)
  if (directionFilter.value) params.set('direction', directionFilter.value)
  try {
    const res = await http.get(`/engine/signals?${params.toString()}`)
    items.value = res.items || []
    summary.value = res.summary || null
  } catch (err: any) {
    items.value = []
  } finally {
    loading.value = false
  }
}

onMounted(load)
</script>

<template>
  <div class="stack" style="gap: 12px">
    <div class="panel">
      <div class="panel-body" style="padding: 16px 20px">
        <div class="row" style="gap: 10px; flex-wrap: wrap">
          <span class="panel-title" style="font-size: 14px">信号审计</span>
          <span class="tag tag-dim">五因子评分</span>
          <span class="tag tag-dim">AI 复核</span>
          <span class="tag tag-dim">拒绝原因可解释</span>
          <div class="spacer" />
          <span class="muted" style="font-size: 11px">每 5 秒一轮扫描，仅当前交易环境</span>
        </div>
        <div class="hint" style="margin-top: 8px">
          引擎每一轮对标的池逐标的计算五因子得分，过闸门、过 AI 复核后才可能开仓。
          任何一步不通过都会写入「拒绝原因」，本页按时间倒序摊开。
        </div>
      </div>
    </div>

    <div class="kpi-grid">
      <div class="kpi">
        <div class="kpi-label">扫描信号</div>
        <div class="kpi-value">{{ summary?.total ?? '--' }}</div>
        <div class="kpi-foot">全部裁决</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">已开仓</div>
        <div class="kpi-value up">{{ summary?.opened ?? '--' }}</div>
        <div class="kpi-foot">通过全部闸门</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">被拒</div>
        <div class="kpi-value down">{{ summary?.rejected ?? '--' }}</div>
        <div class="kpi-foot">含明确拒绝原因</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">平盘 / 待定</div>
        <div class="kpi-value">{{ (summary?.flat ?? 0) + (summary?.pending ?? 0) }}</div>
        <div class="kpi-foot">分数不足或结构不符</div>
      </div>
    </div>

    <div class="grid" style="grid-template-columns: minmax(0, 1fr) 300px; gap: 12px">
      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">信号流水</span>
          <div class="right" style="gap: 8px">
            <input v-model="symbolFilter" class="input mono" style="width: 120px" placeholder="标的，如 BTC" @keyup.enter="load" />
            <select v-model="directionFilter" class="input" style="width: 88px" @change="load">
              <option value="">方向</option>
              <option value="long">多</option>
              <option value="short">空</option>
              <option value="flat">平</option>
            </select>
            <select v-model="decisionFilter" class="input" style="width: 100px" @change="load">
              <option value="">裁决</option>
              <option value="opened">已开仓</option>
              <option value="rejected">被拒</option>
              <option value="flat">平盘</option>
              <option value="pending">待定</option>
            </select>
            <button class="btn btn-sm" :disabled="loading" @click="load">筛选</button>
          </div>
        </div>
        <div class="panel-body" style="padding: 0">
          <div v-if="loading" class="empty" style="padding: 40px">加载中…</div>
          <div v-else-if="!items.length" class="empty" style="padding: 40px">暂无信号记录</div>
          <table v-else class="table">
            <thead>
              <tr>
                <th style="width: 118px">时间</th>
                <th style="width: 84px">标的</th>
                <th style="width: 40px">方向</th>
                <th style="width: 64px">评分</th>
                <th>特征读数</th>
                <th style="width: 108px">AI</th>
                <th style="width: 64px">裁决</th>
                <th>拒绝原因</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="row in items" :key="row.id">
                <td class="mono muted">{{ timeStr(row.ts) }}</td>
                <td class="mono">{{ row.symbol }}</td>
                <td :class="row.direction === 'long' ? 'up' : row.direction === 'short' ? 'down' : 'flat'">
                  {{ DIRECTION_LABELS[row.direction] || row.direction }}
                </td>
                <td class="mono" :class="numClass(row.score)">{{ row.score.toFixed(2) }}</td>
                <td class="mono muted" style="font-size: 10.5px">{{ featureLine(row) }}</td>
                <td>
                  <div class="stack" style="gap: 2px">
                    <span class="mono" style="font-size: 10.5px">{{ row.ai_action || '—' }}</span>
                    <span v-if="row.ai_confidence" class="muted" style="font-size: 9.5px">
                      conf {{ (row.ai_confidence * 100).toFixed(0) }}%
                    </span>
                  </div>
                </td>
                <td>
                  <span class="tag" :class="(DECISION_LABELS[row.decision] || { cls: 'tag-dim' }).cls">
                    {{ (DECISION_LABELS[row.decision] || { text: row.decision }).text }}
                  </span>
                </td>
                <td class="muted" style="font-size: 11px">{{ row.reject_reason || '—' }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>

      <aside class="panel" style="align-self: start">
        <div class="panel-hd"><span class="panel-title">拒绝原因 Top</span></div>
        <div class="panel-body stack" style="gap: 10px">
          <div v-if="!topReasons.length" class="empty" style="padding: 20px 0">暂无被拒信号</div>
          <div v-for="r in topReasons" :key="r.reason" class="stack" style="gap: 4px">
            <div class="row" style="gap: 8px; font-size: 11px">
              <span class="muted" style="flex: 1; line-height: 1.6">{{ r.reason }}</span>
              <span class="mono" style="color: var(--warn)">{{ r.count }}</span>
            </div>
            <div class="bar"><div class="bar-fill" :style="{ width: (r.count / maxReason) * 100 + '%' }" /></div>
          </div>

          <div class="muted" style="font-size: 11px; margin-top: 6px">信号密度（近 7 天 · 按小时，UTC）</div>
          <div v-if="byHour.length" class="hours">
            <div
              v-for="b in byHour"
              :key="b.hour"
              class="hour-col"
              :title="`${b.hour}:00 · ${b.count} 条`"
            >
              <div class="hour-bar" :style="{ height: Math.max(2, (b.count / maxHour) * 100) + '%' }" />
            </div>
          </div>
          <div v-if="byHour.length" class="row hours-axis">
            <span>00</span><span>06</span><span>12</span><span>18</span><span>23</span>
          </div>
        </div>
      </aside>
    </div>
  </div>
</template>

<style scoped>
.hours {
  display: flex;
  align-items: flex-end;
  gap: 2px;
  height: 48px;
  padding: 4px 2px;
  background: rgba(8, 14, 26, 0.5);
  border-radius: 6px;
}
.hour-col {
  flex: 1;
  display: flex;
  align-items: flex-end;
  height: 100%;
}
.hour-bar {
  width: 100%;
  background: linear-gradient(180deg, rgba(59, 130, 246, 0.9), rgba(59, 130, 246, 0.35));
  border-radius: 2px;
}
.hours-axis {
  justify-content: space-between;
  font-size: 9.5px;
  color: var(--muted);
  padding: 0 2px;
}

.bar {
  height: 4px;
  border-radius: 2px;
  background: rgba(148, 170, 200, 0.12);
  overflow: hidden;
}
.bar-fill {
  height: 100%;
  background: linear-gradient(90deg, #e0b155, #f0c674);
  border-radius: 2px;
}
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
@media (max-width: 960px) {
  .grid {
    grid-template-columns: minmax(0, 1fr) !important;
  }
}
</style>
