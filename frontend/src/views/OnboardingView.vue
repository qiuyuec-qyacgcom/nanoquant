<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { http } from '../api/client'
import { useAppStore } from '../stores/app'
import { fmt, px } from '../utils/format'

interface Step {
  key: string
  title: string
  ok: boolean
  level: 'ok' | 'warn' | 'fail'
  detail: string
  action: string
  action_text: string
}

interface SymbolRow {
  symbol: string
  tradeable: boolean
  reason: string
  price: number
  max_leverage: number
  leverage_used: number
  min_qty: number
  min_notional: number
  allocatable_notional: number
  qty_if_open: number
  notional_if_open: number
}

const app = useAppStore()
const data = ref<any>(null)
const loading = ref(false)

const steps = computed<Step[]>(() => data.value?.steps || [])
const blocking = computed(() => steps.value.filter((s) => !s.ok && s.level === 'fail'))
const warnings = computed(() => steps.value.filter((s) => !s.ok && s.level === 'warn'))
const symbols = computed<SymbolRow[]>(() => data.value?.symbols || [])
const tradeable = computed(() => symbols.value.filter((s) => s.tradeable))

const CHAIN = [
  { key: 'channel', label: '交易通道', desc: '凭证 + 接口' },
  { key: 'signal', label: '信号', desc: '5 因子评分' },
  { key: 'gates', label: '风控闸门', desc: '敞口/回撤/冷却' },
  { key: 'size', label: '仓位计算', desc: '风险预算→数量' },
  { key: 'ai', label: 'AI 复核', desc: '否决/降杠杆' },
  { key: 'order', label: '下单', desc: '市价 + 保护单' },
  { key: 'manage', label: '持仓管理', desc: '止损/移动/超时' },
]

const chainState = computed(() => {
  const blockedKeys = new Set(blocking.value.map((s) => s.key))
  const warnKeys = new Set(warnings.value.map((s) => s.key))
  return CHAIN.map((node, i) => {
    let state: 'ok' | 'warn' | 'fail' = 'ok'
    if (i === 0 && blockedKeys.has('channel')) state = 'fail'
    if (node.key === 'gates' && blockedKeys.has('funds')) state = 'fail'
    if (node.key === 'ai' && warnKeys.has('ai')) state = 'warn'
    if ((node.key === 'signal' || node.key === 'size') && blockedKeys.has('symbols')) state = 'fail'
    if (node.key === 'order' && (blockedKeys.has('engine') || blockedKeys.has('channel'))) state = 'fail'
    return { ...node, state }
  })
})

const engineStep = computed(() => steps.value.find((s) => s.key === 'engine'))

const statusText = computed(() => {
  if (!data.value) return '体检中…'
  if (blocking.value.length) return `还差 ${blocking.value.length} 项前置条件`
  if (engineStep.value && !engineStep.value.ok) return '前置条件已齐备，点右上角「启动引擎」'
  if (data.value.can_open) return '引擎运行中，等待信号触发'
  return '条件齐备，等待信号触发'
})

async function load() {
  loading.value = true
  try {
    data.value = await http.get('/engine/admission')
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    loading.value = false
  }
}

function goto(action: string) {
  if (!action) return
  const hash = action.replace('#', '')
  // 后端返回的是后台相对路径 (如 /strategy?group=data), 需要补上 /admin 前缀
  location.hash = hash.startsWith('/admin') ? hash : `#/admin${hash}`
}

onMounted(load)
</script>

<template>
  <div class="stack">
    <div class="panel" :style="{ borderColor: data?.can_open ? 'rgba(18,196,139,0.45)' : 'var(--line)' }">
      <div class="panel-body" style="display: flex; gap: 18px; align-items: center; flex-wrap: wrap">
        <div style="flex: 1; min-width: 240px">
          <div class="row" style="gap: 8px">
            <span class="dot" :class="data?.can_open ? 'on' : data ? 'warn' : 'off'" />
            <span style="font-size: 16px; font-weight: 600">{{ statusText }}</span>
            <span v-if="data" class="tag" :class="data.mode === 'live' ? 'tag-short' : 'tag-long'">
              {{ data.mode === 'live' ? 'LIVE 实盘' : 'DEMO 模拟盘' }}
            </span>
          </div>
          <div class="brand-sub" style="margin-top: 6px">PRE-TRADE CHECKS · 7 STAGES</div>
          <div class="muted" style="font-size: 11.5px; margin-top: 7px; line-height: 1.7">
            引擎每一轮会依次通过「信号 → 风控闸门 → 仓位计算 → AI 复核 → 下单」五道关；
            任何一道不通过都不会开仓，原因会写入「信号审计」。
          </div>
        </div>
        <div class="row" style="gap: 18px">
          <div class="metric">
            <span>体检得分</span>
            <span class="mono">{{ data ? (data.score * 100).toFixed(0) + '%' : '--' }}</span>
          </div>
          <div class="metric">
            <span>可交易标的</span>
            <span class="mono">{{ data?.tradeable_count ?? 0 }} / {{ symbols.length }}</span>
          </div>
          <div class="metric">
            <span>账户权益</span>
            <span class="mono">{{ fmt(data?.account?.equity, 4) }} USDT</span>
          </div>
          <button class="btn btn-sm" :disabled="loading" @click="load">{{ loading ? '体检中…' : '重新体检' }}</button>
        </div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">开仓检查链</span>
        <div class="right"><span class="tag tag-dim">任一环节失败即不开仓</span></div>
      </div>
      <div class="panel-body">
        <div class="flow">
          <template v-for="(node, i) in chainState" :key="node.key">
            <div class="flow-card" :class="node.state">
              <div class="flow-head">
                <span class="flow-idx">{{ i + 1 }}</span>
                <span class="flow-label">{{ node.label }}</span>
              </div>
              <div class="flow-desc">{{ node.desc }}</div>
            </div>
            <div v-if="i < chainState.length - 1" class="flow-connector" :class="node.state" />
          </template>
        </div>
      </div>
    </div>

    <div class="grid g2">
      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">开仓前置条件</span>
          <div class="right">
            <span class="tag" :class="blocking.length ? 'tag-short' : 'tag-long'">
              {{ steps.filter((s) => s.ok).length }}/{{ steps.length }} 通过
            </span>
          </div>
        </div>
        <div class="panel-body stack" style="gap: 10px">
          <div v-for="s in steps" :key="s.key" class="check-item" :class="s.level">
            <div class="check-mark">{{ s.ok ? '✓' : s.level === 'warn' ? '!' : '×' }}</div>
            <div style="flex: 1; min-width: 0">
              <div class="row" style="gap: 8px">
                <b style="font-size: 12.5px">{{ s.title }}</b>
                <span v-if="!s.ok" class="tag" :class="s.level === 'warn' ? 'tag-warn' : 'tag-short'">
                  {{ s.level === 'warn' ? '建议处理' : '阻塞' }}
                </span>
              </div>
              <div class="muted" style="font-size: 11.5px; line-height: 1.7; margin-top: 3px">{{ s.detail }}</div>
            </div>
            <button v-if="s.action && !s.ok" class="btn btn-xs" @click="goto(s.action)">{{ s.action_text || '去设置' }}</button>
          </div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">标的可下单性</span>
          <div class="right">
            <span class="tag tag-dim">按 0.5% 止损估算</span>
            <button class="btn btn-xs" @click="goto('#/symbols')">管理标的池</button>
          </div>
        </div>
        <div class="panel-body flush table-scroll" style="max-height: 420px">
          <table class="table">
            <thead>
              <tr>
                <th>标的</th>
                <th>最小名义额</th>
                <th>可分配</th>
                <th>下单量</th>
                <th class="nowrap">结论</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="s in symbols" :key="s.symbol">
                <td>
                  <b>{{ s.symbol }}</b>
                  <div class="muted mono" style="font-size: 10px">{{ px(s.price) }} · {{ s.leverage_used }}x</div>
                </td>
                <td class="mono">{{ fmt(s.min_notional, 2) }}</td>
                <td class="mono">{{ fmt(s.allocatable_notional, 2) }}</td>
                <td class="mono">{{ s.qty_if_open || '—' }}</td>
                <td style="white-space: normal; max-width: 220px">
                  <span class="tag" :class="s.tradeable ? 'tag-long' : 'tag-warn'">{{ s.tradeable ? '可开仓' : '不可开仓' }}</span>
                  <div v-if="!s.tradeable" class="hint" style="margin-top: 4px">{{ s.reason }}</div>
                </td>
              </tr>
              <tr v-if="!symbols.length"><td colspan="5" class="empty">标的池为空</td></tr>
            </tbody>
          </table>
        </div>
      </div>
    </div>

  </div>
</template>

<style scoped>
.flow {
  display: flex;
  align-items: stretch;
  gap: 0;
  overflow-x: auto;
  padding: 4px 2px 8px;
}
.flow-card {
  flex: 1 1 0;
  min-width: 152px;
  padding: 10px 12px;
  border: 1px solid var(--line);
  border-radius: 10px;
  background: linear-gradient(180deg, rgba(20, 33, 55, 0.72), rgba(10, 17, 29, 0.88));
  display: flex;
  flex-direction: column;
  gap: 6px;
  transition: transform 0.15s ease, border-color 0.15s ease, box-shadow 0.15s ease;
}
.flow-card:hover {
  transform: translateY(-2px);
  border-color: #2a4877;
  box-shadow: 0 6px 18px rgba(0, 0, 0, 0.28);
}
.flow-head {
  display: flex;
  align-items: center;
  gap: 7px;
}
.flow-idx {
  width: 19px;
  height: 19px;
  border-radius: 6px;
  display: grid;
  place-items: center;
  font-family: var(--mono);
  font-size: 10.5px;
  font-weight: 700;
  background: #16233a;
  color: #9fb6d6;
  border: 1px solid var(--line);
  flex: 0 0 19px;
}
.flow-label {
  font-size: 12.5px;
  font-weight: 600;
  white-space: nowrap;
}
.flow-desc {
  font-size: 10.5px;
  color: var(--muted);
  line-height: 1.55;
  padding-left: 26px;
}

/* 模块之间的连接线: 位于卡片外侧垂直居中, 不覆盖任何文字 */
.flow-connector {
  flex: 0 0 20px;
  position: relative;
  display: flex;
  align-items: center;
}
.flow-connector::before {
  content: '';
  width: 100%;
  height: 1px;
  background: linear-gradient(90deg, var(--line), #2b4573);
}
.flow-connector::after {
  content: '';
  position: absolute;
  right: 1px;
  top: 50%;
  width: 5px;
  height: 5px;
  border-top: 1px solid #2b4573;
  border-right: 1px solid #2b4573;
  transform: translateY(-50%) rotate(45deg);
}

.flow-card.ok {
  border-color: rgba(18, 196, 139, 0.3);
}
.flow-card.ok .flow-idx {
  background: rgba(18, 196, 139, 0.14);
  border-color: rgba(18, 196, 139, 0.5);
  color: var(--up);
}
.flow-card.warn {
  border-color: rgba(240, 169, 42, 0.36);
}
.flow-card.warn .flow-idx {
  background: rgba(240, 169, 42, 0.14);
  border-color: rgba(240, 169, 42, 0.5);
  color: var(--warn);
}
.flow-card.fail {
  border-color: rgba(240, 75, 90, 0.4);
}
.flow-card.fail .flow-idx {
  background: rgba(240, 75, 90, 0.14);
  border-color: rgba(240, 75, 90, 0.5);
  color: var(--down);
}
.flow-card.fail .flow-label {
  color: #ff9aa3;
}

.flow-connector.ok::before {
  background: linear-gradient(90deg, rgba(18, 196, 139, 0.5), #2b4573);
}
.flow-connector.warn::before {
  background: linear-gradient(90deg, rgba(240, 169, 42, 0.5), #2b4573);
}
.flow-connector.fail::before {
  background: linear-gradient(90deg, rgba(240, 75, 90, 0.5), #2b4573);
}

.check-item {
  display: flex;
  gap: 10px;
  align-items: flex-start;
  padding: 10px 12px;
  border-radius: 9px;
  background: rgba(12, 20, 34, 0.65);
  border: 1px solid var(--line-soft);
}
.check-mark {
  width: 20px;
  height: 20px;
  border-radius: 6px;
  display: grid;
  place-items: center;
  font-size: 12px;
  font-weight: 700;
  flex: 0 0 20px;
}
.check-item.ok .check-mark {
  background: rgba(18, 196, 139, 0.15);
  color: var(--up);
}
.check-item.warn {
  border-color: rgba(240, 169, 42, 0.28);
}
.check-item.warn .check-mark {
  background: rgba(240, 169, 42, 0.15);
  color: var(--warn);
}
.check-item.fail {
  border-color: rgba(240, 75, 90, 0.32);
}
.check-item.fail .check-mark {
  background: rgba(240, 75, 90, 0.15);
  color: var(--down);
}

/* 窄屏: 卡片纵向堆叠, 连接线改为竖直短线 */
@media (max-width: 1000px) {
  .flow {
    flex-direction: column;
    align-items: stretch;
    overflow-x: visible;
  }
  .flow-card {
    min-width: 0;
  }
  .flow-connector {
    flex: 0 0 18px;
    padding-left: 18px;
    align-items: stretch;
  }
  .flow-connector::before {
    width: 1px;
    height: 100%;
    background: linear-gradient(180deg, var(--line), #2b4573);
  }
  .flow-connector::after {
    right: auto;
    left: 15px;
    top: auto;
    bottom: 2px;
    transform: rotate(135deg);
  }
}
</style>
