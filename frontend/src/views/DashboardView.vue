<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { http } from '../api/client'
import { publicQuotes } from '../api/public'
import EChart from '../components/EChart.vue'
import EnvironmentSwitch from '../components/EnvironmentSwitch.vue'
import EventFeed from '../components/EventFeed.vue'
import { useSilentRefresh } from '../composables/silentRefresh'
import { useAppStore } from '../stores/app'
import { visibleEvents } from '../utils/events'
import { activeOrders, distancePct, isMakerOrder, orderPurposeLabel, protectionForPosition } from '../utils/dashboard'
import { duration, EXIT_LABELS, fmt, numClass, px, timeStr, usdt } from '../utils/format'
import { displayDirection, displayScore } from '../utils/signalAudit'

interface EquityPoint { ts: number; wallet: number; unrealized: number; available: number; margin_used: number; positions: number }

const app = useAppStore()
const visibleAppEvents = computed(() => visibleEvents(app.events))
const equity = ref<EquityPoint[]>([])
const trades = ref<any[]>([])
const signals = ref<any[]>([])
const usage = ref<any>(null)
const stages = ref<any>(null)
const halfAiRaw = ref<any>(null)
const protection = ref<any[]>([])
const orders = ref<any[]>([])
const busy = ref(false)
const showPosHelp = ref(false)
/** H 工作台：工作预设、持仓视图与行内抽屉。 */
type StudioPreset = 'monitor' | 'research' | 'ops'
const preset = ref<StudioPreset>('monitor')
const positionView = ref<'list' | 'table'>('list')
const selectedPosition = ref<any | null>(null)
const initialLoading = ref(true)

const PRESET_TEXT: Record<StudioPreset, string> = {
  monitor: '监控视图 · 聚焦持仓与执行',
  research: '研究视图 · 权益、AI 与信号优先',
  ops: '运维视图 · 风控与订单链路优先',
}
const presetCopy = computed(() => PRESET_TEXT[preset.value])

function selectPosition(position: any) {
  selectedPosition.value =
    positionProtectionRows.value.find((row) => row.id === position.id) || position
}

function closeDrawer() {
  selectedPosition.value = null
}

async function closeSelected() {
  const position = selectedPosition.value
  if (!position) return
  await closePosition(position.id)
  closeDrawer()
}

const status = computed(() => app.status)
const stats = computed(() => status.value?.stats || ({} as any))

/** 池内标的实时报价：让持仓浮盈与保护单距离随行情刷新，而不是等引擎周期落库。 */
const tickers = ref<Record<string, { last: number }>>({})

async function refreshQuotes() {
  try {
    const res = await publicQuotes()
    tickers.value = res.tickers || {}
  } catch {
    /* 报价拉取失败时沿用上次读数，不打断页面 */
  }
}

const positions = computed(() => {
  const list: any[] = status.value?.positions || []
  return list.map((p) => {
    const live = Number(tickers.value[p.symbol]?.last || 0)
    if (!(live > 0) || !(p.entry_price > 0) || !(p.qty > 0)) return p
    const direction = p.side === 'long' ? 1 : -1
    const unrealized = (live - p.entry_price) * p.qty * direction
    return {
      ...p,
      mark_price: live,
      notional: live * p.qty,
      unrealized,
      roi_pct: p.margin > 0 ? (unrealized / p.margin) * 100 : 0,
      unrealized_source: 'live',
    }
  })
})

/** 顶部账户概览与右上角同源：统一使用 /engine/status 的 account 快照，避免用实时持仓二次重算。 */
const account = computed(() => {
  const base: any = status.value?.account || {}
  return {
    ...base,
    wallet: Number(base.wallet || 0),
    available: Number(base.available || 0),
    equity: Number(base.equity || 0),
    unrealized: Number(base.unrealized || 0),
    margin_used: Number(base.margin_used || 0),
  }
})

/** 当前持仓止盈止损：取交易所 STOP_MARKET / TAKE_PROFIT_MARKET 触发价。 */
const positionProtectionRows = computed(() =>
  positions.value.map((p) => {
    const mark = Number(p.mark_price || 0)
    const { stop, takeProfit } = protectionForPosition(p, protection.value)
    const sl = Number(stop?.trigger_price || 0)
    const tp = Number(takeProfit?.trigger_price || 0)
    return {
      ...p,
      exchange_sl_price: sl,
      exchange_tp_price: tp,
      sl_distance_pct: distancePct(mark, sl),
      tp_distance_pct: distancePct(mark, tp),
      protection_status: stop && takeProfit ? '已挂' : stop || takeProfit ? '部分' : '未挂',
    }
  }),
)

const activeOrderRows = computed(() => activeOrders(orders.value).filter(isMakerOrder))

const cooldowns = computed(() => Object.entries(stats.value?.cooldowns || {}))
const dlq = computed(() => stats.value?.dead_letters || { pending: 0, stuck: 0, last_ts: 0 })

/** 执行质量 (下单延迟 / 入场滑点) + 止损熔断状态 */
const exec = computed(() => {
  const total = stages.value?.segments?.total || {}
  return {
    latency_p50_ms: Number(total.p50 || 0),
    latency_p95_ms: Number(total.p95 || 0),
    avg_slippage_bps: Number(stats.value?.execution?.avg_slippage_bps || 0),
    order_count: Number(total.count || 0),
    guard: Number(stats.value?.stoploss_guard_remaining || 0),
  }
})

/** 下单关键路径分段 p99（执行内核七段指标，未连接/无样本时为 0） */
const p99 = computed(() => {
  const seg = stages.value?.segments || {}
  return {
    count: Number(seg.total?.count || 0),
    total: Number(seg.total?.p99 || 0),
    persist: Number(seg.persist?.p99 || 0),
    protect: Number(seg.protect?.p99 || 0),
    sign: Number(seg.sign?.p99 || 0),
  }
})

/** 超短线执行指标: 近 N 笔的胜率/期望/持仓时长 + 当前点差闸门与阶梯止盈 */
const scalp = computed(() => {
  const list = trades.value || []
  const wins = list.filter((t) => Number(t.pnl) > 0)
  const losses = list.filter((t) => Number(t.pnl) <= 0)
  const winRate = list.length ? wins.length / list.length : 0
  const avgWin = wins.length ? wins.reduce((s, t) => s + Number(t.pnl), 0) / wins.length : 0
  const avgLoss = losses.length ? Math.abs(losses.reduce((s, t) => s + Number(t.pnl), 0) / losses.length) : 0
  const expectancy = winRate * avgWin - (1 - winRate) * avgLoss
  const avgHold = list.length ? list.reduce((s, t) => s + Number(t.hold_seconds || 0), 0) / list.length : 0
  const strat: any = app.settings.strategy || {}
  const roi: number[][] = Array.isArray(strat.roi_table) ? strat.roi_table : []
  return {
    count: list.length,
    winRate,
    expectancy,
    avgHold,
    spread: Number(strat.max_spread_bps ?? 0),
    roi,
    roiTail: roi.length ? roi[roi.length - 1] : null,
  }
})

/** 半 AI 运行指标（/engine/stats 最近 24h）：AI 否决、scale 分布、fail-open、闸门、延迟、盘口健康度 */
const HALF_AI_GATE_LABELS: Record<string, string> = {
  funding_veto: '资金费率',
  news_veto: '负面新闻',
  direction_limit: '同向持仓上限',
  correlation_limit: '相关性上限',
  score_split_low: '低分分流',
  ai_fail_open: 'AI 失败放行',
  ai_dq: 'AI 数据质量',
  ai_rr: 'AI 盈亏比',
  ai_confidence: 'AI 置信度',
  ai_direction: 'AI 方向',
  other: '其他',
  none: '无闸门',
}

const halfAi = computed(() => {
  const h: any = halfAiRaw.value?.half_ai || {}
  const gates: Record<string, number> = h.gates || {}
  const ranked = Object.entries(gates)
    .filter(([key]) => key !== 'none')
    .sort((a, b) => Number(b[1]) - Number(a[1]))
  const topGate = ranked.length && Number(ranked[0][1]) > 0
    ? `${HALF_AI_GATE_LABELS[ranked[0][0]] || ranked[0][0]} ${ranked[0][1]}`
    : '--'
  const byProvider: Record<string, any> = h.ai_latency_by_provider || {}
  const topProviders = Object.entries(byProvider)
    .map(([name, row]) => ({
      name,
      samples: Number(row?.samples || 0),
      p95: Number(row?.p95_ms || 0),
    }))
    .filter((row) => row.samples > 0)
    .sort((a, b) => b.samples - a.samples)
    .slice(0, 2)
  return {
    total: Number(h.signals_total || 0),
    opened: Number(h.opened || 0),
    rejected: Number(h.rejected || 0),
    reviewed: Number(h.ai_reviewed || 0),
    vetoRate: Number(h.ai_veto_rate_pct || 0),
    scale05: Number(h.scale_0_5 || 0),
    scale075: Number(h.scale_0_75 || 0),
    scale09: Number(h.scale_0_9_1 || 0),
    scaleNone: Number(h.scale_none || 0),
    failOpen: Number(h.ai_fail_open || 0),
    highSkip: Number(h.high_score_skip || 0),
    promptFull: Number(h.ai_prompt_full || 0),
    promptSlim: Number(h.ai_prompt_slim || 0),
    topGate,
    p50: Number(h.ai_latency_p50_ms || 0),
    p95: Number(h.ai_latency_p95_ms || 0),
    p99: Number(h.ai_latency_p99_ms || 0),
    max: Number(h.ai_latency_max_ms || 0),
    samples: Number(h.ai_latency_samples || 0),
    providerP95: Number(h.ai_latency_p95_ms || 0),
    wallP95: Number(h.ai_wall_p95_ms || 0),
    queueP95: Number(h.ai_queue_p95_ms || 0),
    topProviders,
    bookStale: Number(h.book_stale_pct || 0),
    bookQueries: Number(h.book?.queries || 0),
  }
})

/** 从当前后台主题读取图表配色，切主题后由 `nq-theme` 事件触发重算。 */
const themeTick = ref(0)
function cssVar(name: string, fallback: string) {
  const host = document.querySelector('.shell') as HTMLElement | null
  const value = getComputedStyle(host || document.documentElement).getPropertyValue(name).trim()
  return value || fallback
}

const equityOption = computed(() => {
  themeTick.value
  const points = equity.value
  const panel = cssVar('--panel', '#11161d')
  const line = cssVar('--line', 'rgba(255,255,255,.08)')
  const muted = cssVar('--muted', '#8b98a9')
  const text = cssVar('--text', '#e8edf4')
  const accent = cssVar('--accent', '#2ee6a6')
  return {
    grid: { left: 54, right: 18, top: 22, bottom: 28 },
    tooltip: {
      trigger: 'axis',
      backgroundColor: panel,
      borderColor: line,
      textStyle: { color: text, fontSize: 11 },
      valueFormatter: (v: number) => `${Number(v).toFixed(4)}`,
    },
    xAxis: {
      type: 'category',
      data: points.map((p) => timeStr(p.ts, false)),
      axisLine: { lineStyle: { color: line } },
      axisLabel: { color: muted, fontSize: 10, interval: Math.ceil(points.length / 8) },
    },
    yAxis: {
      type: 'value',
      scale: true,
      splitLine: { lineStyle: { color: line } },
      axisLabel: { color: muted, fontSize: 10, formatter: (v: number) => v.toFixed(3) },
    },
    series: [
      {
        name: '权益',
        type: 'line',
        smooth: true,
        showSymbol: false,
        data: points.map((p) => Number((p.wallet + p.unrealized).toFixed(6))),
        lineStyle: { width: 1.8, color: accent },
        itemStyle: { color: accent },
        areaStyle: { color: accent, opacity: 0.12 },
      },
    ],
  }
})

function onThemeChange() {
  themeTick.value += 1
}

async function load() {

  const [eq, tr, sg, us, ev, ex, pr, od, hs] = await Promise.allSettled([
    http.get('/engine/equity?hours=48'),
    http.get('/engine/trades?limit=10'),
    http.get('/engine/signals?limit=12'),
    http.get('/ai/usage'),
    app.loadEvents(40),
    http.get('/engine/execution'),
    http.get('/engine/protection'),
    http.get('/engine/orders?limit=30'),
    http.get('/engine/stats'),
  ])
  if (eq.status === 'fulfilled') equity.value = eq.value.items
  if (tr.status === 'fulfilled') trades.value = tr.value.items
  if (sg.status === 'fulfilled') signals.value = sg.value.items
  if (us.status === 'fulfilled') usage.value = us.value
  if (ex.status === 'fulfilled') stages.value = ex.value
  if (pr.status === 'fulfilled') protection.value = pr.value.items || []
  if (od.status === 'fulfilled') orders.value = od.value.items || []
  if (hs.status === 'fulfilled') halfAiRaw.value = hs.value
  initialLoading.value = false
}

async function cycleNow() {
  busy.value = true
  try {
    const res = await http.post('/engine/cycle-now')
    app.notify(`已执行一轮: ${JSON.stringify(res.result)}`, 'ok')
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}



async function closePosition(id: number) {
  if (!confirm('确认平掉该仓位?')) return
  try {
    await http.post(`/engine/positions/${id}/close`, { reason: 'manual' })
    app.notify('平仓指令已提交', 'ok')
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}


onMounted(async () => {
  window.addEventListener('nq-theme', onThemeChange)
  await app.loadStatus()
  await refreshQuotes()
  await load()
})

onBeforeUnmount(() => {
  window.removeEventListener('nq-theme', onThemeChange)
})

useSilentRefresh(load, 3000)
useSilentRefresh(refreshQuotes, 3000)
</script>

<template>
  <div class="studio">
    <div class="studio-bar">
      <div class="studio-heading">
        <h2>概览工作台</h2>
        <p>{{ presetCopy }}</p>
      </div>
      <div class="studio-actions">
        <div class="studio-seg" role="group" aria-label="工作视图">
          <button type="button" :class="{ on: preset === 'monitor' }" @click="preset = 'monitor'">监控</button>
          <button type="button" :class="{ on: preset === 'research' }" @click="preset = 'research'">研究</button>
          <button type="button" :class="{ on: preset === 'ops' }" @click="preset = 'ops'">运维</button>
        </div>
        <span class="tag tag-dim">{{ status?.mode === 'live' ? 'LIVE' : 'DEMO' }} · {{ status?.cycle_interval_sec || 15 }}s</span>
      </div>
    </div>

    <div class="kpi-grid studio-kpis">
      <div class="kpi">
        <div class="kpi-label">账户权益 <span class="tag" :class="status?.mode === 'live' ? 'tag-short' : 'tag-long'">{{ status?.mode === 'live' ? '实盘' : '模拟盘' }}</span></div>
        <div class="kpi-value">{{ fmt(account.equity, 4) }}</div>
        <div class="kpi-foot">钱包 {{ fmt(account.wallet, 4) }} · 可用 {{ fmt(account.available, 4) }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">
          浮动盈亏
          <span v-if="account.stale" class="tag tag-warn" :title="account.error">缓存值</span>
        </div>
        <div class="kpi-value" :class="numClass(account.unrealized)">{{ usdt(account.unrealized, 4) }}</div>
        <div class="kpi-foot">
          {{ account.stale ? '账户接口暂不可用 · 沿用上次读数' : '占用保证金 ' + fmt(account.margin_used, 4) + ' USDT' }}
        </div>
      </div>
      <div class="kpi">
        <div class="kpi-label">累计已实现</div>
        <div class="kpi-value" :class="numClass(stats.realized_pnl)">{{ usdt(stats.realized_pnl, 4) }}</div>
        <div class="kpi-foot">{{ stats.trades || 0 }} 笔闭环 · 胜率 {{ (stats.win_rate || 0).toFixed(1) }}%</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">在仓 / 上限</div>
        <div class="kpi-value">{{ positions.length }} <span class="muted" style="font-size: 14px">/ {{ app.settings.engine?.max_positions || 3 }}</span></div>
        <div class="kpi-foot">连亏 {{ stats.consecutive_losses || 0 }} 笔 · 冷却 {{ cooldowns.length }} 个标的</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">AI 用量 (30 天)</div>
        <div class="kpi-value">{{ usage?.total_30d?.calls ?? 0 }} <span class="muted" style="font-size: 14px">次</span></div>
        <div class="kpi-foot">花费 ${{ (usage?.total_30d?.cost_usd || 0).toFixed(4) }} / 预算 ${{ usage?.budget?.monthly_usd ?? 0 }}</div>
      </div>
    </div>

    <div class="studio-grid" :data-preset="preset" :class="{ 'is-loading': initialLoading }">
    <div class="grid g2 studio-row">
      <div class="panel studio-card span2" data-card="equity">
        <div class="panel-hd">
          <span class="panel-title">权益曲线 · 48H</span>
          <div class="right"><span class="tag tag-dim">{{ equity.length }} 个采样点</span></div>
        </div>
        <div class="panel-body">
          <EChart v-if="equity.length > 1" :option="equityOption" height="240px" />
          <div v-else class="empty">权益采样将在引擎运行后每 60 秒记录一次</div>
        </div>
      </div>

      <div class="panel studio-card" data-card="engine">
        <div class="panel-hd">
          <span class="panel-title">引擎控制</span>
          <div class="right">
            <span class="dot" :class="status?.running ? (status?.enabled ? 'on' : 'off') : 'err'" />
            <span class="muted" style="font-size: 11px">
              {{ status?.running ? (status?.enabled ? `运行中 · ${status?.last_cycle_ms}ms` : '待命') : (status?.enabled ? '已开启 · 无心跳' : '未运行') }}
            </span>
          </div>
        </div>
        <div class="panel-body stack" style="gap: 14px">
          <div class="row" style="flex-wrap: wrap">
            <span class="label" style="width: 76px">交易环境</span>
            <EnvironmentSwitch compact :show-meta="false" @changed="load" />
            <div class="spacer" />
            <button class="btn btn-sm" :disabled="busy" @click="cycleNow">立即执行一轮</button>
          </div>
          <div class="note" style="margin: 0">
            交易环境:
            <b>{{ status?.env_label || (status?.mode === 'live' ? '实盘 · 币安主网' : '模拟盘 · 币安测试网') }}</b>
            · 账户 {{ (status?.account as any)?.error ? '不可用：' + (status?.account as any)?.error : '已同步' }}
            · 心跳 {{ (status?.heartbeat_age ?? -1) >= 0 ? status?.heartbeat_age + 's 前' : '无' }}
            <template v-if="status?.liveness_threshold_sec">
              （判活阈值 {{ status.liveness_threshold_sec }}s · 扫描周期 {{ status.cycle_interval_sec }}s）
            </template>
            · 数据延迟 {{ status?.last_cycle_ms }}ms
            <template v-if="status?.rate_limit?.banned">
              · <span class="down">
                <template v-if="(status.rate_limit.declared_remaining ?? 0) > 0">
                  交易所封禁至 {{ status.rate_limit.declared_label }}（还需约
                  {{ Math.ceil((status.rate_limit.declared_remaining ?? 0) / 60) }} 分钟），
                  本地 {{ status.rate_limit.ban_remaining }}s 后试探
                </template>
                <template v-else>交易所限频冷却 {{ status.rate_limit.ban_remaining }}s</template>
                （权重 {{ status.rate_limit.used_weight }}/{{ status.rate_limit.capacity }}）
              </span>
            </template>
          </div>
          <div v-if="status?.trade_mode?.need_hedge" class="note warn" style="margin: 0">
            账户为单向持仓，系统按双向逐仓设计。
            <router-link to="/admin/settings/params?group=engine">前往参数配置一键开启双向持仓</router-link>
          </div>
          <div v-else-if="status?.trade_mode?.need_isolated?.length" class="note warn" style="margin: 0">
            有 {{ status.trade_mode.need_isolated.length }} 个标的仍是全仓，建议在
            <router-link to="/admin/settings/params?group=engine">参数配置 → 交易模式</router-link> 切为逐仓。
          </div>
          <div class="grid g3" style="gap: 10px">

            <div class="metric"><span>杠杆区间</span><span>{{ app.settings.risk?.min_leverage ?? '-' }}–{{ app.settings.risk?.max_leverage ?? '-' }}x</span></div>
            <div class="metric"><span>日内亏损上限</span><span>{{ app.settings.risk?.daily_loss_limit_pct ?? '-' }}%</span></div>
          </div>
          <div v-if="cooldowns.length" class="stack" style="gap: 6px">
            <div v-for="[sym, cd] in cooldowns" :key="sym" class="row" style="font-size: 11px">
              <span class="tag tag-warn">{{ sym }}</span>
              <span class="muted">{{ (cd as any).reason }} · 剩余 {{ duration((cd as any).remaining) }}</span>
            </div>
          </div>
        </div>
      </div>
    </div>

    <div class="panel studio-card" data-card="execution">
      <div class="panel-hd">
        <span class="panel-title">超短线执行</span>
        <div class="right">
          <router-link v-if="dlq.pending" to="/admin/settings/dead-letters" class="tag tag-warn">
            待补偿 {{ dlq.pending }}{{ dlq.stuck ? ` (滞留 ${dlq.stuck})` : '' }}
          </router-link>
          <span class="tag tag-dim">近 {{ scalp.count }} 笔</span>
          <router-link to="/admin/settings/params?group=strategy" class="btn btn-xs">调参</router-link>
        </div>
      </div>
      <div class="panel-body stack" style="gap: 12px">
        <div class="grid g4" style="gap: 10px">
          <div class="metric">
            <span>点差闸门</span>
            <span class="mono" :class="scalp.spread ? 'up' : 'flat'">{{ scalp.spread ? scalp.spread + ' bps' : '关闭' }}</span>
          </div>
          <div class="metric">
            <span>阶梯止盈</span>
            <span class="mono">{{ scalp.roi.length ? scalp.roi.length + ' 档' : '关闭' }}</span>
          </div>
          <div class="metric">
            <span>平均持仓</span>
            <span class="mono">{{ scalp.avgHold ? duration(scalp.avgHold) : '--' }}</span>
          </div>
          <div class="metric">
            <span>单笔期望</span>
            <span class="mono" :class="scalp.expectancy >= 0 ? 'up' : 'down'">
              {{ scalp.count ? usdt(scalp.expectancy, 4) : '--' }}
            </span>
          </div>
        </div>
        <div class="grid g4" style="gap: 10px">
          <div class="metric">
            <span>下单延迟 P50</span>
            <span class="mono">{{ exec.latency_p50_ms ? exec.latency_p50_ms + 'ms' : '--' }}</span>
          </div>
          <div class="metric">
            <span>下单延迟 P95</span>
            <span class="mono">{{ exec.latency_p95_ms ? exec.latency_p95_ms + 'ms' : '--' }}</span>
          </div>
          <div class="metric">
            <span>平均入场滑点</span>
            <span class="mono" :class="exec.avg_slippage_bps > 0 ? 'down' : 'flat'">
              {{ exec.order_count || exec.avg_slippage_bps ? exec.avg_slippage_bps + ' bps' : '--' }}
            </span>
          </div>
          <div class="metric">
            <span>止损熔断</span>
            <span class="mono" :class="exec.guard > 0 ? 'down' : 'up'">
              {{ exec.guard > 0 ? '暂停 ' + exec.guard + 's' : '正常' }}
            </span>
          </div>
        </div>
        <div class="grid g4" style="gap: 10px">
          <div class="metric">
            <span>下单 p99</span>
            <span class="mono">{{ p99.count ? p99.total + 'ms' : '--' }}</span>
          </div>
          <div class="metric">
            <span>写库 p99</span>
            <span class="mono">{{ p99.count ? p99.persist + 'ms' : '--' }}</span>
          </div>
          <div class="metric">
            <span>保护单 p99</span>
            <span class="mono">{{ p99.count ? p99.protect + 'ms' : '--' }}</span>
          </div>
          <div class="metric">
            <span>签名 p99</span>
            <span class="mono">{{ p99.count ? p99.sign + 'ms' : '--' }}</span>
          </div>
        </div>
        <div class="hint">
          阶梯止盈（越拿越容易止盈）：
          <template v-if="scalp.roi.length">
            <span v-for="(row, i) in scalp.roi" :key="i" class="tag tag-dim" style="margin-left: 6px">
              ≥{{ row[0] }}min · {{ row[1] }}%
            </span>
          </template>
          <template v-else>已关闭，仅按固定止盈 / 移动止损 / 超时出场</template>
        </div>
        <div class="hint">
          点差闸门在开仓前用买一/卖一价校验；下单延迟与入场滑点来自最近成交的订单/交易，
          止损熔断在窗口内多次止损后全局暂停开仓。
        </div>
      </div>
    </div>

    <div class="panel studio-card" data-card="halfai">
      <div class="panel-hd">
        <span class="panel-title">半 AI 运行 · 24H</span>
        <div class="right">
          <span class="tag tag-dim">{{ halfAi.total }} 条信号</span>
          <span class="tag tag-dim">开 {{ halfAi.opened }} · 拒 {{ halfAi.rejected }}</span>
        </div>
      </div>
      <div class="panel-body stack" style="gap: 12px">
        <div class="grid g4" style="gap: 10px">
          <div class="metric">
            <span>AI 否决率</span>
            <span
              class="mono"
              :class="halfAi.vetoRate > 0 ? 'down' : 'flat'"
              :title="`AI 复核信号中否决占比（窗口内 ${halfAi.reviewed} 条 AI 复核）`"
            >{{ halfAi.vetoRate.toFixed(1) }}%</span>
          </div>
          <div class="metric">
            <span>fail-open</span>
            <span class="mono" :class="halfAi.failOpen ? 'down' : 'up'">{{ halfAi.failOpen }} 次</span>
          </div>
          <div class="metric">
            <span>高分区跳过</span>
            <span class="mono">{{ halfAi.highSkip }} 次</span>
          </div>
          <div class="metric">
            <span>Top 闸门</span>
            <span class="mono">{{ halfAi.topGate }}</span>
          </div>
        </div>
        <div class="grid g4" style="gap: 10px">
          <div class="metric"><span>scale 0.5</span><span class="mono">{{ halfAi.scale05 }}</span></div>
          <div class="metric"><span>scale 0.75</span><span class="mono">{{ halfAi.scale075 }}</span></div>
          <div class="metric"><span>scale 0.9~1</span><span class="mono">{{ halfAi.scale09 }}</span></div>
          <div class="metric"><span>scale 无</span><span class="mono muted">{{ halfAi.scaleNone }}</span></div>
        </div>
        <div class="grid g2" style="gap: 10px">
          <div class="metric"><span>提示词 完整</span><span class="mono">{{ halfAi.promptFull }}</span></div>
          <div class="metric"><span>提示词 瘦身</span><span class="mono">{{ halfAi.promptSlim }}</span></div>
        </div>
        <div class="grid g4" style="gap: 10px">
          <div class="metric">
            <span>AI 延迟 P50</span>
            <span class="mono">{{ halfAi.p50 ? halfAi.p50 + 'ms' : '--' }}</span>
          </div>
          <div class="metric">
            <span>AI 延迟 P95</span>
            <span class="mono" :class="halfAi.p95 > 3000 ? 'down' : 'flat'">{{ halfAi.p95 ? halfAi.p95 + 'ms' : '--' }}</span>
          </div>
          <div class="metric">
            <span title="nearest-rank 分位；样本很少时 P99 可能等于最大值，需结合样本数解读">AI 延迟 P99</span>
            <span class="mono" :class="halfAi.p99 > 3000 ? 'down' : 'flat'">{{ halfAi.p99 ? halfAi.p99 + 'ms' : '--' }}</span>
          </div>
          <div class="metric">
            <span>AI 延迟 max</span>
            <span class="mono" :class="halfAi.max > 3000 ? 'down' : 'flat'">{{ halfAi.max ? halfAi.max + 'ms' : '--' }}</span>
          </div>
        </div>
        <div class="grid g3" style="gap: 10px">
          <div class="metric">
            <span title="候选进入复核到真正开始调用的等待，含线程调度与锁竞争">排队 P95</span>
            <span class="mono" :class="halfAi.queueP95 > 3000 ? 'down' : 'flat'">{{ halfAi.queueP95 ? halfAi.queueP95 + 'ms' : '--' }}</span>
          </div>
          <div class="metric">
            <span title="整次复核总耗时，含排队、供应商调用与解析">总耗时 P95</span>
            <span class="mono" :class="halfAi.wallP95 > 3000 ? 'down' : 'flat'">{{ halfAi.wallP95 ? halfAi.wallP95 + 'ms' : '--' }}</span>
          </div>
          <div class="metric">
            <span title="供应商 HTTP 耗时 P95，全环境样本">供应商 P95</span>
            <span class="mono" :class="halfAi.providerP95 > 3000 ? 'down' : 'flat'">{{ halfAi.providerP95 ? halfAi.providerP95 + 'ms' : '--' }}</span>
          </div>
        </div>
        <div class="grid g2" style="gap: 10px">
          <div class="metric">
            <span>{{ halfAi.topProviders[0] ? halfAi.topProviders[0].name + ' P95' : '供应商 P95' }}</span>
            <span class="mono" :class="halfAi.topProviders[0]?.p95 > 3000 ? 'down' : 'flat'">
              {{ halfAi.topProviders[0]?.p95 ? halfAi.topProviders[0].p95 + 'ms' : '--' }}
            </span>
          </div>
          <div class="metric">
            <span>{{ halfAi.topProviders[1] ? halfAi.topProviders[1].name + ' P95' : '供应商 P95（Top2）' }}</span>
            <span class="mono" :class="halfAi.topProviders[1]?.p95 > 3000 ? 'down' : 'flat'">
              {{ halfAi.topProviders[1]?.p95 ? halfAi.topProviders[1].p95 + 'ms' : '--' }}
            </span>
          </div>
        </div>
        <div class="grid g3" style="gap: 10px">
          <div class="metric">
            <span title="窗口内正延迟的 AI 调用条数，样本越少 P99 越接近最大值">AI 延迟样本</span>
            <span class="mono">{{ halfAi.samples }} 条</span>
          </div>
          <div class="metric">
            <span>盘口 stale（进程累计）</span>
            <span class="mono" :class="halfAi.bookStale > 0 ? 'down' : 'up'">{{ halfAi.bookStale.toFixed(1) }}%</span>
          </div>
          <div class="metric">
            <span>盘口读取</span>
            <span class="mono muted">{{ halfAi.bookQueries }} 次</span>
          </div>
        </div>
        <div class="hint">
          最近 24 小时口径：AI 否决率 = AI 复核信号中否决占比（数据质量、盈亏比、置信度、方向，
          或 AI 明确 reject/hold）；fail-open 为 AI 复核失败后放行；
          排队 = 总耗时 - 供应商耗时 - 解析耗时，下限 0；
          盘口 stale 为本地 WebSocket 盘口过期并回退 REST 的进程累计比例。
        </div>
      </div>
    </div>

    <div class="panel studio-card span2" data-card="positions">
      <div class="panel-hd">
        <span class="panel-title">当前持仓</span>
        <div class="right">
          <div class="studio-views" role="group" aria-label="持仓视图">
            <button type="button" :class="{ on: positionView === 'list' }" @click="positionView = 'list'">列表</button>
            <button type="button" :class="{ on: positionView === 'table' }" @click="positionView = 'table'">表格</button>
          </div>
          <span class="tag tag-dim">{{ positions.length }} 个</span>
          <button class="btn btn-xs" @click="showPosHelp = !showPosHelp">{{ showPosHelp ? '收起说明' : '口径说明' }}</button>
        </div>
      </div>
      <div v-if="showPosHelp" class="panel-body" style="padding-bottom: 0">
        <div class="note" style="margin: 0; font-size: 11.5px; line-height: 1.9">
          <b>持仓</b>：一笔尚未平掉的合约头寸，由「开仓」产生、由「平仓」结束。
          <b>数量</b>是币的数量（不是 USDT）；<b>名义额</b>=数量×标记价，是这笔仓位的真实市场敞口；
          <b>保证金</b>=名义额÷杠杆，是账户被占用的资金；
          <b>浮盈亏</b>优先取交易所 positionRisk 的未实现盈亏（按 markPrice 结算，与保护单口径一致）；
          同步超过 90 秒才回退到按最新成交价估算；
          <b>ROI</b>=浮盈亏÷保证金，即保证金收益率（杠杆会放大该比例）；
          <b>强平价</b>为按逐仓近似公式估算，仅供参考，实际以交易所为准。
        </div>
      </div>
      <div v-if="positionView === 'list'" class="panel-body">
        <div class="pos-list">
          <button
            v-for="p in positionProtectionRows"
            :key="p.id"
            type="button"
            class="pos-row"
            @click="selectPosition(p)"
          >
            <span class="pos-symbol"><i :class="{ short: p.side !== 'long' }" />{{ p.symbol }}</span>
            <span class="tag pos-side" :class="p.side === 'long' ? 'tag-long' : 'tag-short'">
              {{ p.side === 'long' ? '做多' : '做空' }}
            </span>
            <span class="mono pos-qty">{{ p.qty }}</span>
            <span class="mono" :class="numClass(p.unrealized)">{{ usdt(p.unrealized, 4) }}</span>
            <span class="tag pos-state" :class="p.protection_status === '已挂' ? 'tag-long' : p.protection_status === '部分' ? 'tag-warn' : 'tag-dim'">
              {{ p.protection_status }}
            </span>
          </button>
          <div v-if="!positionProtectionRows.length" class="empty">暂无持仓 · 引擎会在信号与风控同时满足时自动开仓</div>
        </div>
        <div class="hint" style="margin-top: 10px">点击行在右侧查看详情并可直接平仓；切到「表格」查看全部字段。</div>
      </div>
      <div v-else class="panel-body flush table-scroll">
        <table class="table" style="min-width: 1080px">
          <thead>
            <tr>
              <th>标的</th>
              <th>方向</th>
              <th title="币的数量, 由名义额 ÷ 价格并按交易所步进取整得到">数量</th>
              <th title="数量 × 标记价, 即真实市场敞口">名义额</th>
              <th>开仓价</th>
              <th title="交易所 markPrice, 盈亏与保护单均按此价结算">标记价</th>
              <th title="开仓时确定的杠杆; 保证金 = 名义额 ÷ 杠杆">杠杆</th>
              <th title="数量 × 开仓价 ÷ 杠杆, 该仓位占用的保证金">保证金</th>
              <th title="硬止损 / 止盈目标价; 触发由引擎每轮比对或交易所保护单执行">止损 / 止盈</th>
              <th title="按逐仓公式估算的强平价, 仅供参考">强平(估)</th>
              <th title="与顶部「浮动盈亏」同一次请求实时读取 positionRisk，两者应一致；括号内为占保证金的收益率">浮盈亏 (ROI)</th>
              <th title="从开仓到现在的时长">持仓时长</th>
              <th title="开仓时 AI 的置信度, 悬浮可看理由">AI</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="p in positions" :key="p.id" style="cursor: pointer" @click="selectPosition(p)">
              <td>
                <b>{{ p.symbol }}</b>
                <span
                  v-if="p.partial_tp"
                  class="tag tag-dim"
                  style="margin-left: 6px"
                  title="已在目标 R 倍数处平掉一部分，剩余仓位继续持有并受移动止损保护"
                >已部分止盈</span>
              </td>
              <td><span class="tag" :class="p.side === 'long' ? 'tag-long' : 'tag-short'">{{ p.side === 'long' ? '做多' : '做空' }}</span></td>
              <td class="mono">{{ p.qty }}</td>
              <td class="mono">{{ fmt(p.notional ?? p.qty * p.mark_price, 2) }}</td>
              <td class="mono">{{ px(p.entry_price) }}</td>
              <td class="mono">{{ px(p.mark_price) }}</td>
              <td class="mono">{{ p.leverage }}x</td>
              <td class="mono muted">{{ fmt(p.margin, 3) }}</td>
              <td class="mono muted">{{ px(p.sl_price) }} / {{ px(p.tp_price) }}</td>
              <td class="mono muted">{{ px(p.liquidation_price) }}</td>
              <td class="mono" :class="numClass(p.unrealized)">
                {{ usdt(p.unrealized, 5) }}
                <span class="muted" style="font-size: 10px">({{ (p.roi_pct ?? 0).toFixed(1) }}%)</span>
                <span
                  v-if="p.unrealized_source === 'estimated'"
                  class="tag tag-warn"
                  style="margin-left: 4px"
                  title="本次请求未能从交易所读到该仓位，按本地标记价估算"
                >估算</span>
              </td>
              <td class="mono muted">{{ duration((Date.now() / 1000) - p.opened_at) }}</td>
              <td>
                <span v-if="p.ai_confidence" class="tag tag-ai" :title="p.ai_reason">
                  {{ (p.ai_confidence * 100).toFixed(0) }}%
                </span>
                <span v-else class="muted">-</span>
              </td>
              <td class="right"><button class="btn btn-xs btn-danger" @click.stop="closePosition(p.id)">平仓</button></td>
            </tr>
            <tr v-if="!positions.length">
              <td colspan="14" class="empty">暂无持仓 · 引擎会在信号与风控同时满足时自动开仓</td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <div class="panel studio-card span2" data-card="protection">
      <div class="panel-hd">
        <span class="panel-title">当前持仓止盈止损</span>
        <div class="right">
          <span class="tag tag-dim">{{ positionProtectionRows.length }} 仓</span>
          <span class="muted" style="font-size: 10.5px">交易所 STOP_MARKET / TAKE_PROFIT_MARKET 触发价；未挂显示 --</span>
        </div>
      </div>
      <div class="panel-body flush table-scroll">
        <table class="table">
          <thead>
            <tr>
              <th>标的</th><th>方向</th><th>杠杆</th><th>数量</th><th>标记价</th><th>交易所止损</th><th>交易所止盈</th><th title="标记价到交易所止损触发价还差多少">距止损</th><th title="标记价到交易所止盈触发价还差多少">距止盈</th><th>保护</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="p in positionProtectionRows" :key="p.id">
              <td><b>{{ p.symbol }}</b></td>
              <td><span class="tag" :class="p.side === 'long' ? 'tag-long' : 'tag-short'">{{ p.side === 'long' ? '做多' : '做空' }}</span></td>
              <td class="mono">{{ p.leverage }}x</td>
              <td class="mono">{{ p.qty }}</td>
              <td class="mono">{{ px(p.mark_price) }}</td>
              <td class="mono">{{ p.exchange_sl_price > 0 ? px(p.exchange_sl_price) : '--' }}</td>
              <td class="mono">{{ p.exchange_tp_price > 0 ? px(p.exchange_tp_price) : '--' }}</td>
              <td class="mono muted">{{ p.sl_distance_pct != null ? `${p.sl_distance_pct.toFixed(2)}%` : '--' }}</td>
              <td class="mono muted">{{ p.tp_distance_pct != null ? `${p.tp_distance_pct.toFixed(2)}%` : '--' }}</td>
              <td><span class="tag" :class="p.protection_status === '已挂' ? 'tag-long' : p.protection_status === '部分' ? 'tag-warn' : 'tag-dim'">{{ p.protection_status }}</span></td>
            </tr>
            <tr v-if="!positionProtectionRows.length">
              <td colspan="10" class="empty">当前没有持仓 · 交易所保护单挂出后这里显示触发价</td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <div class="panel studio-card span2" data-card="maker">
      <div class="panel-hd">
        <span class="panel-title">当前 Maker 挂单</span>
        <div class="right">
          <span class="tag tag-dim">{{ activeOrderRows.length }} 条</span>
          <span class="muted" style="font-size: 10.5px">只显示 LIMIT/post-only 未完成委托；市价与保护单不在此列</span>
        </div>
      </div>
      <div class="panel-body flush table-scroll" style="max-height: 260px">
        <table class="table">
          <thead>
            <tr><th>时间</th><th>标的</th><th>方向</th><th>用途</th><th>类型</th><th>数量</th><th>价格</th><th>状态</th></tr>
          </thead>
          <tbody>
            <tr v-for="o in activeOrderRows" :key="o.id">
              <td class="mono muted">{{ timeStr(o.ts, false) }}</td>
              <td><b>{{ o.symbol }}</b></td>
              <td><span class="tag" :class="o.side === 'BUY' ? 'tag-long' : 'tag-short'">{{ o.side === 'BUY' ? '买' : '卖' }}</span></td>
              <td class="muted">{{ orderPurposeLabel(o.purpose) }}</td>
              <td class="muted">{{ o.order_type }}</td>
              <td class="mono">{{ o.qty || '--' }}</td>
              <td class="mono">{{ o.price ? px(o.price) : '--' }}</td>
              <td><span class="tag tag-dim">{{ o.status || '--' }}</span></td>
            </tr>
            <tr v-if="!activeOrderRows.length">
              <td colspan="8" class="empty">当前没有只做 Maker 的未完成挂单 · 市价单和保护单不在此列</td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <div class="grid g2 studio-row">
      <div class="panel studio-card" data-card="trades">
        <div class="panel-hd">
          <span class="panel-title">最近成交</span>
          <div class="right"><span class="tag tag-dim">近 10 笔</span></div>
        </div>
        <div class="panel-body flush table-scroll" style="max-height: 300px">
          <table class="table">
            <thead><tr><th>时间</th><th>标的</th><th>方向</th><th>出场</th><th>盈亏</th><th>复盘</th></tr></thead>
            <tbody>
              <tr v-for="t in trades" :key="t.id">
                <td class="mono muted">{{ timeStr(t.closed_at, false) }}</td>
                <td><b>{{ t.symbol }}</b></td>
                <td><span class="tag" :class="t.side === 'long' ? 'tag-long' : 'tag-short'">{{ t.side === 'long' ? '多' : '空' }}</span></td>
                <td class="muted">{{ EXIT_LABELS[t.exit_reason] || t.exit_reason }}</td>
                <td class="mono" :class="numClass(t.pnl)">{{ usdt(t.pnl, 5) }}</td>
                <td>
                  <span class="tag" :class="t.reviewed ? 'tag-long' : 'tag-dim'">{{ t.reviewed ? '已复盘' : '待复盘' }}</span>
                </td>
              </tr>
              <tr v-if="!trades.length"><td colspan="6" class="empty">暂无成交记录</td></tr>
            </tbody>
          </table>
        </div>
      </div>

      <div class="panel studio-card" data-card="events">
        <div class="panel-hd">
          <span class="panel-title">实时事件</span>
          <div class="right"><span class="tag tag-dim">{{ visibleAppEvents.length }}</span></div>
        </div>
        <div class="panel-body"><EventFeed :events="visibleAppEvents" height="260px" /></div>
      </div>
    </div>

    <div class="panel studio-card span2" data-card="orderflow">
      <div class="panel-hd">
        <span class="panel-title">委托流水</span>
        <div class="right">
          <span class="tag tag-dim">最近 {{ orders.length }} 条</span>
          <span class="tag tag-dim" title="挂单回执只有 NEW，成交/撤销由后台每 60s 回查交易所补齐">状态由交易所回查收敛</span>
        </div>
      </div>
      <div class="panel-body flush table-scroll" style="max-height: 260px">
        <table class="table">
          <thead>
            <tr><th>时间</th><th>标的</th><th>方向</th><th>类型</th><th>用途</th><th>数量</th><th>价格</th><th>状态</th></tr>
          </thead>
          <tbody>
            <tr v-for="o in orders" :key="o.id">
              <td class="mono muted">{{ timeStr(o.ts, false) }}</td>
              <td><b>{{ o.symbol }}</b></td>
              <td><span class="tag" :class="o.side === 'BUY' ? 'tag-long' : 'tag-short'">{{ o.side === 'BUY' ? '买' : '卖' }}</span></td>
              <td class="muted">{{ o.order_type }}</td>
              <td class="muted">{{ o.purpose }}</td>
              <td class="mono">{{ o.qty || '--' }}</td>
              <td class="mono">{{ o.price ? px(o.price) : '--' }}</td>
              <td>
                <span
                  class="tag"
                  :class="{
                    'tag-long': o.status === 'FILLED',
                    'tag-short': ['CANCELED', 'CANCELLED', 'EXPIRED', 'REJECTED'].includes(o.status),
                    'tag-dim': !['FILLED', 'CANCELED', 'CANCELLED', 'EXPIRED', 'REJECTED'].includes(o.status),
                  }"
                >{{ o.status || '--' }}</span>
              </td>
            </tr>
            <tr v-if="!orders.length"><td colspan="8" class="empty">暂无委托记录</td></tr>
          </tbody>
        </table>
      </div>
    </div>

    <div class="panel studio-card span2" data-card="signals">
      <div class="panel-hd">
        <span class="panel-title">信号审计 · 为什么开/不开仓</span>
        <div class="right"><span class="tag tag-dim">最近 {{ signals.length }} 条</span></div>
      </div>
      <div class="panel-body flush table-scroll" style="max-height: 320px">
        <table class="table">
          <thead>
            <tr><th>时间</th><th>标的</th><th>方向</th><th>分数</th><th>AI 裁决</th><th>最终决策</th><th>说明</th></tr>
          </thead>
          <tbody>
            <tr v-for="s in signals" :key="s.id">
              <td class="mono muted">{{ timeStr(s.ts, false) }}</td>
              <td>{{ s.symbol }}</td>
              <td>
                <span class="tag" :class="s.direction === 'long' ? 'tag-long' : s.direction === 'short' ? 'tag-short' : 'tag-dim'">
                  {{ displayDirection(s).text }}
                </span>
              </td>
              <td class="mono">{{ displayScore(s).text }}</td>
              <td>
                <span v-if="s.ai_action" class="tag tag-ai">{{ s.ai_action }} · {{ ((s.ai_confidence || 0) * 100).toFixed(0) }}%</span>
                <span v-else class="muted">-</span>
              </td>
              <td>
                <span class="tag" :class="s.decision === 'opened' ? 'tag-long' : s.decision === 'rejected' ? 'tag-warn' : 'tag-dim'">
                  {{ s.decision }}
                </span>
              </td>
              <td class="muted ellipsis" style="max-width: 420px">{{ s.reject_reason || s.ai_reason || '—' }}</td>
            </tr>
            <tr v-if="!signals.length"><td colspan="7" class="empty">暂无信号, 引擎启动后每轮扫描会在此留痕</td></tr>
          </tbody>
        </table>
      </div>
    </div>
    </div>

    <template v-if="selectedPosition">
      <div class="studio-mask" @click="closeDrawer" />
      <aside class="studio-drawer" role="dialog" aria-modal="true" aria-label="持仓详情">
        <div class="studio-drawer-head">
          <h3>{{ selectedPosition.symbol }}</h3>
          <button class="btn btn-xs btn-ghost" type="button" @click="closeDrawer">关闭</button>
        </div>
        <p class="studio-drawer-sub">
          {{ selectedPosition.side === 'long' ? 'LONG' : 'SHORT' }} · {{ selectedPosition.leverage }}× ·
          {{ selectedPosition.protection_status }}
        </p>
        <div class="studio-kv"><span>入场价</span><b>{{ px(selectedPosition.entry_price) }}</b></div>
        <div class="studio-kv"><span>标记价</span><b>{{ px(selectedPosition.mark_price) }}</b></div>
        <div class="studio-kv"><span>数量</span><b>{{ selectedPosition.qty }}</b></div>
        <div class="studio-kv"><span>止损 / 止盈</span><b>{{ px(selectedPosition.sl_price) }} / {{ px(selectedPosition.tp_price) }}</b></div>
        <div class="studio-kv">
          <span>交易所保护价</span>
          <b>
            {{ selectedPosition.exchange_sl_price > 0 ? px(selectedPosition.exchange_sl_price) : '--' }} /
            {{ selectedPosition.exchange_tp_price > 0 ? px(selectedPosition.exchange_tp_price) : '--' }}
          </b>
        </div>
        <div class="studio-kv">
          <span>浮盈亏 (ROI)</span>
          <b :class="numClass(selectedPosition.unrealized)">
            {{ usdt(selectedPosition.unrealized, 4) }} ({{ (selectedPosition.roi_pct ?? 0).toFixed(1) }}%)
          </b>
        </div>
        <div class="studio-kv">
          <span>持仓时长</span>
          <b>{{ duration((Date.now() / 1000) - selectedPosition.opened_at) }}</b>
        </div>
        <div class="studio-drawer-actions">
          <button class="btn btn-xs btn-danger" type="button" @click="closeSelected">平仓</button>
          <router-link class="btn btn-xs" to="/admin/trading/signals">查看信号</router-link>
        </div>
        <p class="studio-drawer-note">行内下钻，不跳页；字段与持仓表同源。移动端自动变为全屏抽屉。</p>
      </aside>
    </template>
  </div>
</template>
