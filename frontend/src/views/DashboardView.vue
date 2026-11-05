<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { http } from '../api/client'
import EChart from '../components/EChart.vue'
import EnvironmentSwitch from '../components/EnvironmentSwitch.vue'
import EventFeed from '../components/EventFeed.vue'
import { useAppStore } from '../stores/app'
import { duration, EXIT_LABELS, fmt, numClass, px, timeStr, usdt } from '../utils/format'

interface EquityPoint { ts: number; wallet: number; unrealized: number; available: number; margin_used: number; positions: number }

const app = useAppStore()
const equity = ref<EquityPoint[]>([])
const trades = ref<any[]>([])
const signals = ref<any[]>([])
const usage = ref<any>(null)
const stages = ref<any>(null)
const busy = ref(false)
const showPosHelp = ref(false)

const status = computed(() => app.status)
const account = computed(() => status.value?.account || ({} as any))
const stats = computed(() => status.value?.stats || ({} as any))
const positions = computed(() => status.value?.positions || [])
const cooldowns = computed(() => Object.entries(stats.value?.cooldowns || {}))
const dlq = computed(() => stats.value?.dead_letters || { pending: 0, stuck: 0, last_ts: 0 })

/** 执行质量 (下单延迟 / 入场滑点) + 止损熔断状态 */
const exec = computed(() => {
  const e = stats.value?.execution || {}
  return {
    latency_p50_ms: Number(e.latency_p50_ms || 0),
    latency_p95_ms: Number(e.latency_p95_ms || 0),
    avg_slippage_bps: Number(e.avg_slippage_bps || 0),
    order_count: Number(e.order_count || 0),
    guard: Number(stats.value?.stoploss_guard_remaining || 0),
  }
})

/** 下单关键路径分段 p99 (内存滚动窗口) */
const p99 = computed(() => {
  const s = stages.value || {}
  return {
    count: Number(s.count || 0),
    total: Number(s.total_ms?.p99 || 0),
    persist: Number(s.persist_ms?.p99 || 0),
    protect: Number(s.protect_ms?.p99 || 0),
    sign: Number(s.sign_ms?.p99 || 0),
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

const equityOption = computed(() => {
  const points = equity.value
  return {
    grid: { left: 54, right: 18, top: 22, bottom: 28 },
    tooltip: {
      trigger: 'axis',
      backgroundColor: '#0d1727',
      borderColor: '#1f3557',
      textStyle: { color: '#dbe6f5', fontSize: 11 },
      valueFormatter: (v: number) => `${Number(v).toFixed(4)}`,
    },
    xAxis: {
      type: 'category',
      data: points.map((p) => timeStr(p.ts, false)),
      axisLine: { lineStyle: { color: '#1a2942' } },
      axisLabel: { color: '#6b7c99', fontSize: 10, interval: Math.ceil(points.length / 8) },
    },
    yAxis: {
      type: 'value',
      scale: true,
      splitLine: { lineStyle: { color: 'rgba(26,41,66,0.55)' } },
      axisLabel: { color: '#6b7c99', fontSize: 10, formatter: (v: number) => v.toFixed(3) },
    },
    series: [
      {
        name: '权益',
        type: 'line',
        smooth: true,
        showSymbol: false,
        data: points.map((p) => Number((p.wallet + p.unrealized).toFixed(6))),
        lineStyle: { width: 1.8, color: '#3b82f6' },
        itemStyle: { color: '#3b82f6' },
        areaStyle: {
          color: {
            type: 'linear', x: 0, y: 0, x2: 0, y2: 1,
            colorStops: [
              { offset: 0, color: 'rgba(59,130,246,0.32)' },
              { offset: 1, color: 'rgba(59,130,246,0.02)' },
            ],
          },
        },
      },
    ],
  }
})

async function load() {
  await app.loadStatus()
  const [eq, tr, sg, us, ev, ex] = await Promise.allSettled([
    http.get('/engine/equity?hours=48'),
    http.get('/engine/trades?limit=10'),
    http.get('/engine/signals?limit=12'),
    http.get('/ai/usage'),
    app.loadEvents(40),
    http.get('/engine/execution'),
  ])
  if (eq.status === 'fulfilled') equity.value = eq.value.items
  if (tr.status === 'fulfilled') trades.value = tr.value.items
  if (sg.status === 'fulfilled') signals.value = sg.value.items
  if (us.status === 'fulfilled') usage.value = us.value
  if (ex.status === 'fulfilled') stages.value = ex.value
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

let timer: number | undefined
onMounted(async () => {
  await load()
  timer = window.setInterval(load, 8000)
})
onBeforeUnmount(() => window.clearInterval(timer))
</script>

<template>
  <div class="stack">
    <div class="kpi-grid">
      <div class="kpi">
        <div class="kpi-label">账户权益 <span class="tag" :class="status?.mode === 'live' ? 'tag-short' : 'tag-long'">{{ status?.mode === 'live' ? '实盘' : '模拟盘' }}</span></div>
        <div class="kpi-value">{{ fmt(account.equity, 4) }}</div>
        <div class="kpi-foot">钱包 {{ fmt(account.wallet, 4) }} · 可用 {{ fmt(account.available, 4) }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">浮动盈亏</div>
        <div class="kpi-value" :class="numClass(account.unrealized)">{{ usdt(account.unrealized, 4) }}</div>
        <div class="kpi-foot">占用保证金 {{ fmt(account.margin_used, 4) }} USDT</div>
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

    <div class="grid g2">
      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">权益曲线 · 48H</span>
          <div class="right"><span class="tag tag-dim">{{ equity.length }} 个采样点</span></div>
        </div>
        <div class="panel-body">
          <EChart v-if="equity.length > 1" :option="equityOption" height="240px" />
          <div v-else class="empty">权益采样将在引擎运行后每 60 秒记录一次</div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">引擎控制</span>
          <div class="right">
            <span class="dot" :class="status?.running ? (status?.enabled ? 'on' : 'off') : 'err'" />
            <span class="muted" style="font-size: 11px">
              {{ status?.running ? (status?.enabled ? `运行中 · ${status?.last_cycle_ms}ms` : '待命') : '未运行' }}
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
            · 数据延迟 {{ status?.last_cycle_ms }}ms
          </div>
          <div class="grid g3" style="gap: 10px">
            <div class="metric"><span>单笔风险</span><span>{{ app.settings.risk?.risk_per_trade_pct ?? '-' }}%</span></div>
            <div class="metric"><span>最大杠杆</span><span>{{ app.settings.risk?.max_leverage ?? '-' }}x</span></div>
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

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">超短线执行</span>
        <div class="right">
          <router-link v-if="dlq.pending" to="/admin/dead-letters" class="tag tag-warn">
            死信 {{ dlq.pending }}{{ dlq.stuck ? ` (卡住 ${dlq.stuck})` : '' }}
          </router-link>
          <span class="tag tag-dim">近 {{ scalp.count }} 笔</span>
          <router-link to="/admin/strategy?group=strategy" class="btn btn-xs">调参</router-link>
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

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">当前持仓</span>
        <div class="right">
          <span class="tag tag-dim">{{ positions.length }} 个</span>
          <button class="btn btn-xs" @click="showPosHelp = !showPosHelp">{{ showPosHelp ? '收起说明' : '口径说明' }}</button>
        </div>
      </div>
      <div v-if="showPosHelp" class="panel-body" style="padding-bottom: 0">
        <div class="note" style="margin: 0; font-size: 11.5px; line-height: 1.9">
          <b>持仓</b>：一笔尚未平掉的合约头寸，由「开仓」产生、由「平仓」结束。
          <b>数量</b>是币的数量（不是 USDT）；<b>名义额</b>=数量×标记价，是这笔仓位的真实市场敞口；
          <b>保证金</b>=名义额÷杠杆，是账户被占用的资金；
          <b>浮盈亏</b>=(标记价−开仓价)×数量×方向，未落袋，会随价格实时变化；
          <b>ROI</b>=浮盈亏÷保证金，即保证金收益率（杠杆会放大该比例）；
          <b>强平价</b>为按逐仓近似公式估算，仅供参考，实际以交易所为准。
        </div>
      </div>
      <div class="panel-body flush table-scroll">
        <table class="table" style="min-width: 1080px">
          <thead>
            <tr>
              <th>标的</th>
              <th>方向</th>
              <th title="币的数量, 由名义额 ÷ 价格并按交易所步进取整得到">数量</th>
              <th title="数量 × 标记价, 即真实市场敞口">名义额</th>
              <th>开仓价</th>
              <th>标记价</th>
              <th title="开仓时确定的杠杆; 保证金 = 名义额 ÷ 杠杆">杠杆</th>
              <th title="数量 × 开仓价 ÷ 杠杆, 该仓位占用的保证金">保证金</th>
              <th title="硬止损 / 止盈目标价; 触发由引擎每轮比对或交易所保护单执行">止损 / 止盈</th>
              <th title="按逐仓公式估算的强平价, 仅供参考">强平(估)</th>
              <th title="未实现盈亏, 以及占保证金的收益率">浮盈亏 (ROI)</th>
              <th title="从开仓到现在的时长; 超过 max_hold_minutes 会超时离场">持仓时长</th>
              <th title="开仓时 AI 的置信度, 悬浮可看理由">AI</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="p in positions" :key="p.id">
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
              </td>
              <td class="mono muted">{{ duration((Date.now() / 1000) - p.opened_at) }}</td>
              <td>
                <span v-if="p.ai_confidence" class="tag tag-ai" :title="p.ai_reason">
                  {{ (p.ai_confidence * 100).toFixed(0) }}%
                </span>
                <span v-else class="muted">-</span>
              </td>
              <td class="right"><button class="btn btn-xs btn-danger" @click="closePosition(p.id)">平仓</button></td>
            </tr>
            <tr v-if="!positions.length">
              <td colspan="11" class="empty">暂无持仓 · 引擎会在信号与风控同时满足时自动开仓</td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>

    <div class="grid g2">
      <div class="panel">
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

      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">实时事件</span>
          <div class="right"><span class="tag tag-dim">{{ app.events.length }}</span></div>
        </div>
        <div class="panel-body"><EventFeed :events="app.events" height="260px" /></div>
      </div>
    </div>

    <div class="panel">
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
                  {{ s.direction }}
                </span>
              </td>
              <td class="mono">{{ s.score?.toFixed(3) }}</td>
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
</template>
