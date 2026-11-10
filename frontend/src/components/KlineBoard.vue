<script setup lang="ts">
/**
 * K 线工作站 (前台 / 后台共用)
 * ---------------------------------------------------------------
 * 数据链路 (对齐 astra-quant-agent 的 ChartWorkstation，但更省额度):
 * - **实时**: 浏览器连本服务 `/api/v1/ws/klines`，服务端复用一条币安
 *   WebSocket 流 (`<symbol>@kline_<interval>`) 推送未收盘蜡烛；
 *   不再每 3 秒轮询 REST，避免触发权重上限 / IP 封禁。
 * - **历史**: 仅在切标的 / 切周期 / 首屏时取一次 600 根（REST 权重 2，带缓存）。
 * - **兜底**: WS 断线时指数退避重连，同时降级为 15 秒一次的低频拉取。
 *
 * 契约: 只展示标的池内合约，顺序 = 添加顺序；空池显示"未添加标的"。
 */
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { http } from '../api/client'
import { publicKlines, publicPool, publicQuotes } from '../api/public'
import type { Candle, PoolSymbol } from '../types/candle'
import { fmt, numClass, pct, px, timeStr } from '../utils/format'
import KlineChartPanel from './KlineChartPanel.vue'

interface TickerBrief {
  last: number
  change_pct: number
}

const props = withDefaults(defineProps<{ variant?: 'public' | 'admin' }>(), { variant: 'public' })
const isAdmin = computed(() => props.variant === 'admin')

const INTERVALS = [
  ['5m', '5分'],
  ['15m', '15分'],
  ['1h', '1小时'],
  ['1d', '日线'],
] as const
const MAIN_INDICATORS = [
  ['MA', 'MA'],
  ['EMA', 'EMA'],
  ['BOLL', 'BOLL'],
  ['NONE', '无'],
] as const
const SUB_INDICATORS = [
  ['VOL', 'VOL'],
  ['MACD', 'MACD'],
  ['RSI', 'RSI'],
  ['NONE', '无'],
] as const
const BARS = 600

const pool = ref<PoolSymbol[]>([])
const selected = ref('')
const interval = ref('5m')
const mainIndicator = ref('MA')
const subIndicator = ref('VOL')
const items = ref<Candle[]>([])
const tickers = ref<Record<string, TickerBrief>>({})
const loading = ref(true)
const chartLoading = ref(false)
const chartError = ref('')
const updatedAt = ref(0)
const nextCloseAt = ref(0)
const skew = ref(0)
const remain = ref(0)
const wsState = ref<'connecting' | 'live' | 'down'>('connecting')

const signature = computed(() => pool.value.map((p) => p.symbol).join(','))
const current = computed(() => pool.value.find((p) => p.symbol === selected.value) || null)
const currentTicker = computed(() => tickers.value[selected.value] || null)
const lastBar = computed<Candle | null>(() => (items.value.length ? items.value[items.value.length - 1] : null))
const lastPrice = computed(() => currentTicker.value?.last || lastBar.value?.c || 0)

/** 模拟盘里有 92 个合约没有 24h ticker（无真实成交），K 线是交易所返回的静态价格：
 *  连续多根 high==low 且无成交量 —— 画出来就是一条直线，不是图表坏了。 */
const chartStatic = computed(() => {
  const list = items.value.slice(-10)
  return list.length >= 5 && list.every((c) => Number(c.h) === Number(c.l))
})

const stats = computed(() => {
  const t = currentTicker.value as any
  if (t?.high) return { high: t.high, low: t.low, quote_volume: t.quote_volume }
  const list = items.value
  if (!list.length) return null
  let high = -Infinity
  let low = Infinity
  let vol = 0
  for (const c of list) {
    high = Math.max(high, c.h)
    low = Math.min(low, c.l)
    vol += c.v
  }
  return { high, low, quote_volume: vol }
})

const countdown = computed(() => {
  if (!nextCloseAt.value) return '--'
  const value = Math.max(0, remain.value)
  const m = Math.floor(value / 60)
  const s = value % 60
  return m > 0 ? `${m}:${String(s).padStart(2, '0')}` : `${s}s`
})

const streamLabel = computed(() => {
  if (wsState.value === 'live') return '实时推送'
  if (wsState.value === 'connecting') return '连接中'
  return '重连中'
})

// ---------------------------------------------------------------- 数据加载
let loadSeq = 0

async function fetchPool(): Promise<PoolSymbol[]> {
  if (isAdmin.value) {
    const res = await http.get('/symbols')
    return (res.items || []).map((row: any, index: number) => ({
      symbol: row.symbol,
      name: row.name || row.symbol.replace(/USDT$/, ''),
      tier: row.tier,
      tier_label: row.tier === 'tier_1_bluechip' ? '蓝筹主流' : '高弹性动量',
      order: index + 1,
      added_at: row.created_at,
    }))
  }
  const res = await publicPool()
  return res.symbols || []
}

/** REST 历史到达后，把加载期间缓存的第一帧 WS 合并进去（重复 t 就替换） */
function mergePendingBar() {
  const bar = pendingBar.value
  if (!bar || !items.value.length) return
  pendingBar.value = null
  const list = items.value
  if (bar.t === list[list.length - 1].t) {
    items.value = [...list.slice(0, -1), bar]
  } else if (bar.t > list[list.length - 1].t) {
    items.value = [...list, bar].slice(-BARS)
  }
}

/** 历史 K 线: reset=true 时先清空 (切标的/切周期立即进入加载态), 避免旧标的数据滞留 */
async function reloadChart(reset = false) {
  const symbol = selected.value
  const period = interval.value
  if (!symbol) return
  const seq = ++loadSeq
  if (reset) {
    items.value = []
    pendingBar.value = null
  }
  chartLoading.value = true
  try {
    const res = isAdmin.value
      ? await http.get(
          `/chart/klines?symbol=${encodeURIComponent(symbol)}&interval=${period}&limit=${BARS}`
        )
      : await publicKlines(symbol, period, BARS)
    if (seq !== loadSeq || symbol !== selected.value || period !== interval.value) return
    items.value = res.items || []
    mergePendingBar()
    nextCloseAt.value = Number(res.next_close_at || 0)
    const server = Number(res.server_time || 0)
    skew.value = server ? server - Date.now() : 0
    updatedAt.value = res.updated_at || Math.floor(Date.now() / 1000)
    chartError.value = ''
  } catch (err: any) {
    if (seq !== loadSeq) return
    pendingBar.value = null
    chartError.value = String(err?.message || 'K 线加载失败')
  } finally {
    if (seq === loadSeq) chartLoading.value = false
  }
}

async function fetchQuotes() {
  if (!pool.value.length) return
  try {
    if (isAdmin.value) {
      const res = await http.get('/engine/quotes')
      const map: Record<string, TickerBrief> = {}
      for (const sym of res.symbols || []) {
        const t = res.tickers?.[sym]
        if (t) map[sym] = { last: Number(t.last || 0), change_pct: Number(t.change_pct || 0) }
      }
      tickers.value = map
    } else {
      const res = await publicQuotes()
      tickers.value = (res.tickers || {}) as Record<string, TickerBrief>
    }
  } catch {
    /* 报价失败不影响 K 线主链路 */
  }
}

async function syncPool(): Promise<boolean> {
  const next = await fetchPool()
  const changed = next.map((s) => s.symbol).join(',') !== signature.value
  pool.value = next
  if (!next.length) {
    selected.value = ''
    items.value = []
    tickers.value = {}
    updatedAt.value = 0
    nextCloseAt.value = 0
    closeSocket()
    return changed
  }
  if (!next.some((s) => s.symbol === selected.value)) selected.value = next[0].symbol
  return changed
}

// ---------------------------------------------------------------- WebSocket
let socket: WebSocket | null = null
let wsTimer: number | undefined
let wsRetry = 0
/** REST 600 根还没到时的第一帧 WS：先缓存，等历史到达再合并，避免图表只画一根变成点/直线 */
const pendingBar = ref<Candle | null>(null)

function applyBar(bar: any) {
  if (!bar || typeof bar.t !== 'number') return
  // 切标的/切周期期间, 旧连接的迟到帧要丢弃, 否则会把旧标的蜡烛插进新标的序列
  if (bar.symbol && String(bar.symbol).toUpperCase() !== selected.value.toUpperCase()) return
  if (bar.interval && String(bar.interval) !== interval.value) return
  const next: Candle = {
    t: Number(bar.t),
    o: Number(bar.o),
    h: Number(bar.h),
    l: Number(bar.l),
    c: Number(bar.c),
    v: Number(bar.v),
  }
  const list = items.value
  if (!list.length) {
    pendingBar.value = next
    return
  }
  if (next.t === list[list.length - 1].t) {
    items.value = [...list.slice(0, -1), next]
  } else if (next.t > list[list.length - 1].t) {
    items.value = [...list, next].slice(-BARS)
  } else {
    return
  }
  if (bar.next_close_at) nextCloseAt.value = Number(bar.next_close_at)
  const eventTime = Number(bar.event_time || 0)
  if (eventTime) skew.value = eventTime - Date.now()
  updatedAt.value = Math.floor(Date.now() / 1000)
}

function closeSocket() {
  if (wsTimer) {
    window.clearTimeout(wsTimer)
    wsTimer = undefined
  }
  if (socket) {
    socket.onclose = null
    socket.onmessage = null
    socket.onerror = null
    socket.close()
    socket = null
  }
}

function connectStream() {
  closeSocket()
  if (!selected.value || typeof WebSocket === 'undefined') return
  const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  const url =
    `${proto}//${window.location.host}/api/v1/ws/klines` +
    `?symbol=${encodeURIComponent(selected.value)}&interval=${encodeURIComponent(interval.value)}`
  wsState.value = 'connecting'
  let ws: WebSocket
  try {
    ws = new WebSocket(url)
  } catch {
    wsState.value = 'down'
    return
  }
  socket = ws
  ws.onopen = () => {
    if (socket !== ws) return
    wsRetry = 0
    wsState.value = 'live'
  }
  ws.onmessage = (evt) => {
    if (socket !== ws) return
    let msg: any
    try {
      msg = JSON.parse(evt.data)
    } catch {
      return
    }
    if (msg?.type === 'kline') applyBar(msg.data)
  }
  ws.onclose = () => {
    if (socket !== ws) return
    socket = null
    wsState.value = 'down'
    wsRetry += 1
    const delay = Math.min(15000, 1000 * 2 ** Math.min(wsRetry, 4))
    wsTimer = window.setTimeout(() => {
      if (!document.hidden) connectStream()
    }, delay)
  }
}

// ---------------------------------------------------------------- 生命周期
async function bootstrap() {
  loading.value = true
  try {
    await syncPool()
    if (pool.value.length) {
      await Promise.all([reloadChart(), fetchQuotes()])
      connectStream()
    }
  } catch (err: any) {
    chartError.value = err?.message || '加载失败'
  } finally {
    loading.value = false
  }
}

async function refreshNow() {
  await syncPool()
  if (pool.value.length) {
    await Promise.all([reloadChart(), fetchQuotes()])
    connectStream()
  }
}

// 切标的 / 切周期: 重新拉历史 + 重订阅, 两条路径行为完全一致.
// 用显式方法包裹, 保证「点标的」与「点周期」触发的是同一条更新链路,
// 且立即失效任何在途的历史请求, 杜绝旧标的的响应覆盖新标的.
function reloadCurrent() {
  if (!selected.value) return
  loadSeq += 1 // 让尚未返回的 reloadChart 直接作废
  reloadChart(true)
  connectStream()
}
watch([selected, interval], reloadCurrent)

let refreshTimer: number | undefined
let quoteTimer: number | undefined
let poolTimer: number | undefined
let clockTimer: number | undefined

function tickCountdown() {
  if (!nextCloseAt.value) {
    remain.value = 0
    return
  }
  const now = Date.now() + skew.value
  remain.value = Math.max(0, Math.ceil((nextCloseAt.value - now) / 1000))
}

function onVisible() {
  if (document.hidden) return
  if (!socket && selected.value) connectStream()
  if (selected.value) reloadChart()
}

onMounted(() => {
  bootstrap()
  clockTimer = window.setInterval(tickCountdown, 1000)
  // 轻量安全刷新 (每 5 秒一次, 权重 2): 对标 astra 的 3 秒轮询。
  // WS 负责秒级更新最后一根蜡烛, 这里负责兜底——即使切标的/切周期时
  // 单次请求偶发失败, 也能在 5 秒内收敛到正确数据, 而不是等到
  // 下一根 5m 蜡烛 (最长 5 分钟) 才更新。
  refreshTimer = window.setInterval(() => {
    if (document.hidden || !selected.value) return
    reloadChart()
  }, 5000)
  // 24h 快照权重 40, 60 秒一次足够
  quoteTimer = window.setInterval(() => {
    if (document.hidden) return
    fetchQuotes()
  }, 60000)
  poolTimer = window.setInterval(() => {
    if (document.hidden) return
    syncPool()
      .then((changed) => {
        if (changed && pool.value.length) {
          reloadChart()
          connectStream()
        }
        return undefined
      })
      .catch(() => undefined)
  }, 10000)
  document.addEventListener('visibilitychange', onVisible)
})

onBeforeUnmount(() => {
  window.clearInterval(refreshTimer)
  window.clearInterval(quoteTimer)
  window.clearInterval(poolTimer)
  window.clearInterval(clockTimer)
  document.removeEventListener('visibilitychange', onVisible)
  closeSocket()
})
</script>

<template>
  <div class="stack" style="gap: 12px">
    <!-- 空池 -->
    <div v-if="!loading && !pool.length" class="panel kb-empty-panel">
      <div class="panel-body kb-empty">
        <div class="kb-empty-ico">▤</div>
        <div class="kb-empty-title">{{ isAdmin ? '标的池为空' : '未添加标的' }}</div>
        <div class="kb-empty-desc">
          前台与后台的 K 线都只展示「标的池」内的合约。请先添加要跟踪的币安 USDT 永续合约，
          保存后这里会按<b>添加顺序</b>自动展示 K 线。
        </div>
        <div class="kb-empty-tags">
          <span class="tag tag-dim">仅池内合约</span>
          <span class="tag tag-dim">按添加顺序</span>
          <span class="tag tag-dim">WebSocket 推送</span>
        </div>
        <router-link to="/admin/symbols" class="btn btn-sm btn-primary" style="margin-top: 16px">
          前往标的池添加
        </router-link>
      </div>
    </div>

    <template v-else>
      <div class="panel">
        <div class="panel-hd">
          <div class="stack" style="gap: 2px">
            <span class="panel-title">{{ isAdmin ? 'K 线工作站' : '标的 K 线' }}</span>
            <span class="muted" style="font-size: 10.5px">
              共 {{ pool.length }} 个标的 · 严格按添加顺序 · 币安 WebSocket 推送 · 600 根历史
            </span>
          </div>
          <div class="right">
            <span class="tag" :class="wsState === 'live' ? 'tag-long' : 'tag-warn'">
              {{ streamLabel }}
            </span>
            <span class="muted mono" style="font-size: 10.5px">收盘倒计时 {{ countdown }}</span>
            <span class="muted mono" style="font-size: 10.5px">{{ updatedAt ? timeStr(updatedAt) : '--' }}</span>
            <div class="kb-intervals">
              <button
                v-for="[value, label] in INTERVALS"
                :key="value"
                type="button"
                class="kb-chip"
                :class="{ on: interval === value }"
                @click="interval = value"
              >
                {{ label }}
              </button>
            </div>
            <button class="btn btn-sm" :disabled="chartLoading" @click="refreshNow">刷新</button>
          </div>
        </div>

        <div class="panel-body stack" style="gap: 12px">
          <!-- 标的切换: 顺序 = 添加顺序 -->
          <div class="kb-tabs">
            <button
              v-for="row in pool"
              :key="row.symbol"
              type="button"
              class="kb-tab"
              :class="{ on: selected === row.symbol }"
              @click="selected = row.symbol"
            >
              <span class="kb-tab-order">{{ row.order }}</span>
              <span class="kb-tab-symbol mono">{{ row.symbol }}</span>
              <span v-if="tickers[row.symbol]" class="kb-tab-chg mono" :class="numClass(tickers[row.symbol].change_pct)">
                {{ pct(tickers[row.symbol].change_pct) }}
              </span>
            </button>
          </div>

          <div class="kb-grid">
            <div class="kb-main">
              <div class="kb-quote">
                <div class="kb-quote-price mono" :class="numClass(currentTicker?.change_pct)">
                  {{ lastPrice ? px(lastPrice) : '--' }}
                </div>
                <div class="kb-quote-meta">
                  <span class="mono" :class="numClass(currentTicker?.change_pct)">
                    {{ currentTicker ? pct(currentTicker.change_pct) : '--' }}
                  </span>
                  <span class="muted">24H</span>
                  <span class="muted mono">高 {{ stats ? px(stats.high) : '--' }}</span>
                  <span class="muted mono">低 {{ stats ? px(stats.low) : '--' }}</span>
                  <span class="muted mono">额 {{ stats ? fmt(stats.quote_volume) : '--' }}</span>
                  <span v-if="current" class="tag" :class="current.tier === 'tier_1_bluechip' ? 'tag-blue' : 'tag-dim'">
                    {{ current.tier_label }}
                  </span>
                </div>
              </div>

              <div class="kb-indi">
                <span class="muted">主图</span>
                <button
                  v-for="[key, label] in MAIN_INDICATORS"
                  :key="key"
                  type="button"
                  class="kb-chip"
                  :class="{ on: mainIndicator === key }"
                  @click="mainIndicator = key"
                >
                  {{ label }}
                </button>
                <span class="muted" style="margin-left: 10px">副图</span>
                <button
                  v-for="[key, label] in SUB_INDICATORS"
                  :key="key"
                  type="button"
                  class="kb-chip"
                  :class="{ on: subIndicator === key }"
                  @click="subIndicator = key"
                >
                  {{ label }}
                </button>
              </div>

              <div v-if="chartStatic" class="note warn" style="margin: 0 0 8px">
                {{ selected }} 在当前环境（模拟盘）没有真实成交，K 线是交易所返回的静态价格（直线），
                价格与成交量都不变化；引擎的 ATR / 量比闸门会自动跳过它，不会开仓。
                切到实盘或换一个有成交的标的即可。
              </div>
              <KlineChartPanel
                v-if="items.length > 1"
                :key="`${mainIndicator}-${subIndicator}`"
                :items="items"
                height="430px"
                :main="mainIndicator"
                :sub="subIndicator"
              />
              <div v-else class="kb-empty" style="height: 430px; justify-content: center">
                <div class="kb-empty-title">
                  {{ chartLoading ? 'K 线加载中…' : items.length ? 'K 线数据不足' : '暂无 K 线数据' }}
                </div>
                <div v-if="!chartLoading" class="kb-empty-desc">
                  <template v-if="items.length">
                    {{ selected }} 只收到 1 根蜡烛，暂不渲染图表（避免显示成一个点）；稍等下一根或点刷新重试。
                  </template>
                  <template v-else>
                    {{ selected }} 在当前交易环境没有返回 K 线。常见原因是该合约在模拟盘没有行情源/成交，
                    或所选周期数据不足；可切换周期、换标的，或切到实盘环境。
                  </template>
                </div>
              </div>
              <div v-if="chartError" class="note error" style="margin: 8px 0 0">
                {{ selected }} 行情暂不可用：{{ chartError }}
              </div>
            </div>

            <aside class="kb-side">
              <div class="kb-side-hd">标的顺序</div>
              <button
                v-for="row in pool"
                :key="row.symbol"
                type="button"
                class="kb-side-row"
                :class="{ on: selected === row.symbol }"
                @click="selected = row.symbol"
              >
                <span class="kb-side-order">{{ row.order }}</span>
                <span class="kb-side-name">
                  <b class="mono">{{ row.name }}</b>
                  <small class="muted mono">{{ row.symbol }}</small>
                </span>
                <span class="kb-side-price mono">{{ tickers[row.symbol] ? px(tickers[row.symbol].last) : '--' }}</span>
                <span class="kb-side-chg mono" :class="numClass(tickers[row.symbol]?.change_pct)">
                  {{ tickers[row.symbol] ? pct(tickers[row.symbol].change_pct) : '--' }}
                </span>
              </button>
            </aside>
          </div>
        </div>
      </div>
    </template>
  </div>
</template>

<style scoped>
.kb-empty-panel {
  border-style: dashed;
}
.kb-empty {
  display: flex;
  flex-direction: column;
  align-items: center;
  text-align: center;
  padding: 54px 20px;
}
.kb-empty-ico {
  width: 54px;
  height: 54px;
  display: grid;
  place-items: center;
  border-radius: 14px;
  border: 1px dashed var(--line);
  color: var(--muted);
  font-size: 22px;
  margin-bottom: 14px;
}
.kb-empty-title {
  font-size: 17px;
  font-weight: 600;
  margin-bottom: 8px;
}
.kb-empty-desc {
  max-width: 520px;
  font-size: 12px;
  line-height: 1.9;
  color: var(--muted);
}
.kb-empty-tags {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
  justify-content: center;
  margin-top: 14px;
}
.kb-intervals {
  display: flex;
  gap: 4px;
  padding: 2px;
  border: 1px solid var(--line);
  border-radius: 7px;
}
.kb-chip {
  background: transparent;
  border: 0;
  color: var(--muted);
  font-size: 11px;
  padding: 3px 8px;
  border-radius: 5px;
  cursor: pointer;
}
.kb-chip:hover {
  color: var(--text);
}
.kb-chip.on {
  background: rgba(59, 130, 246, 0.18);
  color: #9dc1ff;
}
.kb-tabs {
  display: flex;
  gap: 6px;
  overflow-x: auto;
  padding-bottom: 2px;
}
.kb-tab {
  display: flex;
  align-items: center;
  gap: 7px;
  padding: 6px 10px;
  border-radius: 8px;
  border: 1px solid var(--line);
  background: #0a1220;
  color: var(--text);
  cursor: pointer;
  white-space: nowrap;
}
.kb-tab:hover {
  border-color: #2f5fa8;
}
.kb-tab.on {
  border-color: #3b82f6;
  background: rgba(59, 130, 246, 0.14);
}
.kb-tab-order {
  width: 16px;
  height: 16px;
  display: grid;
  place-items: center;
  border-radius: 4px;
  background: rgba(148, 170, 200, 0.14);
  font-size: 9.5px;
  color: var(--muted);
}
.kb-tab-symbol {
  font-size: 11.5px;
}
.kb-tab-chg {
  font-size: 10.5px;
}
.kb-grid {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 236px;
  gap: 12px;
}
.kb-main {
  min-width: 0;
}
.kb-quote {
  display: flex;
  align-items: baseline;
  gap: 12px;
  flex-wrap: wrap;
  margin-bottom: 6px;
}
.kb-quote-price {
  font-size: 22px;
  font-weight: 600;
}
.kb-quote-meta {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  font-size: 11px;
}
.kb-indi {
  display: flex;
  align-items: center;
  gap: 4px;
  margin-bottom: 6px;
  font-size: 11px;
}
.kb-side {
  border: 1px solid var(--line);
  border-radius: 9px;
  overflow: hidden;
  align-self: start;
}
.kb-side-hd {
  padding: 8px 10px;
  font-size: 10.5px;
  color: var(--muted);
  border-bottom: 1px solid var(--line);
  background: rgba(148, 170, 200, 0.05);
}
.kb-side-row {
  width: 100%;
  display: grid;
  grid-template-columns: 18px minmax(0, 1fr) auto auto;
  align-items: center;
  gap: 8px;
  padding: 7px 10px;
  background: transparent;
  border: 0;
  border-bottom: 1px solid rgba(148, 170, 200, 0.06);
  color: var(--text);
  cursor: pointer;
  text-align: left;
}
.kb-side-row:hover {
  background: rgba(148, 170, 200, 0.06);
}
.kb-side-row.on {
  background: rgba(59, 130, 246, 0.12);
}
.kb-side-order {
  font-size: 9.5px;
  color: var(--muted);
}
.kb-side-name {
  display: flex;
  flex-direction: column;
  min-width: 0;
}
.kb-side-name b {
  font-size: 11.5px;
}
.kb-side-name small {
  font-size: 9.5px;
  overflow: hidden;
  text-overflow: ellipsis;
}
.kb-side-price {
  font-size: 11px;
}
.kb-side-chg {
  font-size: 10px;
  width: 58px;
  text-align: right;
}
@media (max-width: 900px) {
  .kb-grid {
    grid-template-columns: minmax(0, 1fr);
  }
  .kb-side {
    display: none;
  }
}
</style>
