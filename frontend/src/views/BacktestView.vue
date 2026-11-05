<script setup lang="ts">
/**
 * 回测: 只用纯量化部分做参数粗筛.
 * 明确不含 AI 复核与盘口点差闸门, 结果不代表实盘收益.
 */
import { computed, onMounted, ref } from 'vue'
import { http } from '../api/client'
import EChart from '../components/EChart.vue'
import { EXIT_LABELS, numClass, timeStr } from '../utils/format'

interface TradeRow {
  side: string
  entry_ts: number
  exit_ts: number
  entry: number
  exit: number
  qty: number
  pnl: number
  pnl_pct: number
  bars: number
  reason: string
  score: number
}

const pool = ref<any[]>([])
const form = ref({
  symbol: '',
  interval: '5m',
  limit: 1000,
  fee_bps: 5,
  slippage_bps: 2,
  initial_equity: 100,
})
const running = ref(false)
const result = ref<any>(null)
const error = ref('')

const trades = ref<TradeRow[]>([])
const summary = computed(() => result.value || {})
const pnlClass = (v: number) => (v > 0 ? 'up' : v < 0 ? 'down' : 'flat')

// ---------------------------------------------------------------- 参数网格
const gridForm = ref({
  entry_threshold: '0.2, 0.3, 0.4',
  sl_atr_mult: '1.0, 1.2, 1.5',
  tp_atr_mult: '',
  trailing_pct: '',
})
const gridRunning = ref(false)
const gridResult = ref<any>(null)
const gridError = ref('')

function parseList(raw: string): number[] {
  return raw
    .split(/[,，\s]+/)
    .map((v) => v.trim())
    .filter((v) => v.length > 0)
    .map((v) => Number(v))
    .filter((v) => Number.isFinite(v))
}

async function loadPool() {
  try {
    const res = await http.get('/symbols')
    pool.value = res.items || []
    if (!form.value.symbol && pool.value.length) form.value.symbol = pool.value[0].symbol
  } catch {
    pool.value = []
  }
}

async function run() {
  if (!form.value.symbol) {
    error.value = '请先在「标的池」添加合约'
    return
  }
  running.value = true
  error.value = ''
  try {
    const res = await http.post('/backtest/run', form.value)
    result.value = res
    trades.value = (res.trades || []).slice().reverse()
  } catch (err: any) {
    error.value = err?.message || '回测失败'
    result.value = null
    trades.value = []
  } finally {
    running.value = false
  }
}

async function runGrid() {
  if (!form.value.symbol) {
    gridError.value = '请先在「标的池」添加合约'
    return
  }
  const payload = {
    symbol: form.value.symbol,
    interval: form.value.interval,
    limit: form.value.limit,
    fee_bps: form.value.fee_bps,
    slippage_bps: form.value.slippage_bps,
    initial_equity: form.value.initial_equity,
    entry_threshold: parseList(gridForm.value.entry_threshold),
    sl_atr_mult: parseList(gridForm.value.sl_atr_mult),
    tp_atr_mult: parseList(gridForm.value.tp_atr_mult),
    trailing_pct: parseList(gridForm.value.trailing_pct),
  }
  if (!payload.entry_threshold.length && !payload.sl_atr_mult.length && !payload.tp_atr_mult.length && !payload.trailing_pct.length) {
    gridError.value = '至少填写一个参数的一组取值（逗号分隔）'
    return
  }
  gridRunning.value = true
  gridError.value = ''
  try {
    gridResult.value = await http.post('/backtest/grid', payload)
  } catch (err: any) {
    gridError.value = err?.message || '网格扫描失败'
    gridResult.value = null
  } finally {
    gridRunning.value = false
  }
}

// ------------------------------------------------------------ 净值曲线 / 导出
const equityOption = computed(() => {
  const curve: number[] = result.value?.equity_curve || []
  if (!curve.length) return null
  return {
    grid: { left: 56, right: 18, top: 18, bottom: 28 },
    tooltip: { trigger: 'axis', valueFormatter: (v: number) => `${v}` },
    xAxis: {
      type: 'category',
      data: curve.map((_, i) => i + 1),
      boundaryGap: false,
      axisLine: { lineStyle: { color: 'rgba(148,170,200,0.18)' } },
      axisLabel: { color: '#64748b', fontSize: 10, interval: Math.max(0, Math.floor(curve.length / 8)) },
    },
    yAxis: {
      type: 'value',
      scale: true,
      splitLine: { lineStyle: { color: 'rgba(148,170,200,0.08)' } },
      axisLabel: { color: '#64748b', fontSize: 10 },
    },
    series: [
      {
        type: 'line',
        data: curve,
        showSymbol: false,
        lineStyle: { color: '#3b82f6', width: 1.6 },
        areaStyle: { color: 'rgba(59,130,246,0.16)' },
      },
    ],
  }
})

function downloadCsv(name: string, rows: (string | number)[][]) {
  const csv = rows
    .map((row) => row.map((cell) => `"${String(cell ?? '').replace(/"/g, '""')}"`).join(','))
    .join('\n')
  const blob = new Blob([`\ufeff${csv}`], { type: 'text/csv;charset=utf-8' })
  const url = URL.createObjectURL(blob)
  const link = document.createElement('a')
  link.href = url
  link.download = name
  link.click()
  URL.revokeObjectURL(url)
}

function exportTrades() {
  if (!trades.value.length) return
  const head = ['方向', '开仓时间', '开仓价', '平仓价', '盈亏', '涨跌%', '持仓根数', '出场原因', '评分']
  const rows = trades.value.map((t) => [
    t.side,
    timeStr(t.entry_ts),
    t.entry,
    t.exit,
    t.pnl,
    t.pnl_pct,
    t.bars,
    EXIT_LABELS[t.reason] || t.reason,
    t.score,
  ])
  downloadCsv(`backtest-${form.value.symbol}-${form.value.interval}.csv`, [head, ...rows])
}

function exportGrid() {
  const items = gridResult.value?.items || []
  if (!items.length) return
  const keys: string[] = Array.from(new Set<string>(items.flatMap((row: any) => Object.keys(row.params || {}))))
  const head = [...keys, '交易数', '胜率', '盈亏比', '单笔期望', '总收益', '最大回撤', '平均持仓']
  const rows = items.map((row: any) => [
    ...keys.map((k) => row.params?.[k] ?? ''),
    row.summary.trades,
    row.summary.win_rate,
    row.summary.profit_factor,
    row.summary.expectancy,
    row.summary.total_return_pct,
    row.summary.max_drawdown_pct,
    row.summary.avg_bars,
  ])
  downloadCsv(`backtest-grid-${form.value.symbol}-${form.value.interval}.csv`, [head, ...rows])
}

onMounted(loadPool)
</script>

<template>
  <div class="stack" style="gap: 12px">
    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">回测（纯量化粗筛）</span>
        <div class="right">
          <span class="tag tag-warn">不含 AI / 点差闸门</span>
          <span class="tag tag-dim">历史 K 线</span>
        </div>
      </div>
      <div class="panel-body stack" style="gap: 12px">
        <div class="hint">
          用与实盘相同的五因子打分与出场规则跑历史 K 线，手续费与滑点显式计入。
          结果只用于参数粗筛与逻辑回归验证，<b>不能当作实盘收益预期</b>。
        </div>
        <div class="grid g6 bt-form" style="gap: 10px">
          <label class="field">
            <span class="label">标的</span>
            <select v-model="form.symbol" class="input">
              <option v-for="row in pool" :key="row.symbol" :value="row.symbol">{{ row.symbol }}</option>
            </select>
          </label>
          <label class="field">
            <span class="label">周期</span>
            <select v-model="form.interval" class="input">
              <option value="5m">5m</option>
              <option value="15m">15m</option>
              <option value="30m">30m</option>
              <option value="1h">1h</option>
              <option value="4h">4h</option>
              <option value="1d">1d</option>
            </select>
          </label>
          <label class="field">
            <span class="label">K 线根数</span>
            <select v-model.number="form.limit" class="input">
              <option :value="500">500</option>
              <option :value="1000">1000</option>
              <option :value="1500">1500</option>
            </select>
          </label>
          <label class="field">
            <span class="label">手续费 (bps)</span>
            <input v-model.number="form.fee_bps" class="input mono" type="number" min="0" max="100" />
          </label>
          <label class="field">
            <span class="label">滑点 (bps)</span>
            <input v-model.number="form.slippage_bps" class="input mono" type="number" min="0" max="100" />
          </label>
          <label class="field">
            <span class="label">初始权益</span>
            <input v-model.number="form.initial_equity" class="input mono" type="number" min="1" />
          </label>
        </div>
        <div class="row" style="gap: 8px">
          <button class="btn btn-primary" :disabled="running || !form.symbol" @click="run">
            {{ running ? '回测中…' : '开始回测' }}
          </button>
          <span v-if="error" class="muted" style="color: var(--down); font-size: 11.5px">{{ error }}</span>
          <span v-else-if="result" class="muted" style="font-size: 11.5px">
            {{ result.symbol }} · {{ result.interval }} · {{ result.bars }} 根 ·
            {{ timeStr(result.from_ts) }} → {{ timeStr(result.to_ts) }}
          </span>
        </div>
      </div>
    </div>

    <template v-if="result">
      <div class="kpi-grid">
        <div class="kpi">
          <div class="kpi-label">交易数</div>
          <div class="kpi-value">{{ summary.trade_count }}</div>
          <div class="kpi-foot">胜 {{ summary.wins }} / 负 {{ summary.losses }}</div>
        </div>
        <div class="kpi">
          <div class="kpi-label">胜率</div>
          <div class="kpi-value">{{ summary.win_rate }}%</div>
          <div class="kpi-foot">盈亏比 {{ summary.profit_factor }}</div>
        </div>
        <div class="kpi">
          <div class="kpi-label">单笔期望</div>
          <div class="kpi-value" :class="pnlClass(summary.expectancy)">{{ summary.expectancy }}</div>
          <div class="kpi-foot">平均持仓 {{ summary.avg_bars }} 根</div>
        </div>
        <div class="kpi">
          <div class="kpi-label">总收益</div>
          <div class="kpi-value" :class="pnlClass(summary.total_return_pct)">{{ summary.total_return_pct }}%</div>
          <div class="kpi-foot">期末权益 {{ summary.final_equity }}</div>
        </div>
        <div class="kpi">
          <div class="kpi-label">最大回撤</div>
          <div class="kpi-value down">{{ summary.max_drawdown_pct }}%</div>
          <div class="kpi-foot">峰值到谷底</div>
        </div>
        <div class="kpi">
          <div class="kpi-label">手续费合计</div>
          <div class="kpi-value">{{ summary.fee_total }}</div>
          <div class="kpi-foot">双边 taker</div>
        </div>
      </div>

      <div v-if="equityOption" class="panel">
        <div class="panel-hd">
          <span class="panel-title">净值曲线（按已平仓交易结算）</span>
          <div class="right"><span class="tag tag-dim">{{ result.equity_curve?.length || 0 }} 笔</span></div>
        </div>
        <div class="panel-body">
          <EChart :option="equityOption" height="220px" />
        </div>
      </div>

      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">分段统计</span>
          <div class="right">
            <span v-for="row in result.by_exit || []" :key="row.reason" class="tag tag-dim">
              {{ EXIT_LABELS[row.reason] || row.reason }} · {{ row.count }}
            </span>
          </div>
        </div>
        <div class="panel-body">
          <div class="grid g3" style="gap: 14px">
            <div>
              <div class="muted" style="font-size: 11px; margin-bottom: 6px">按开仓小时（UTC）</div>
              <table class="table">
                <thead><tr><th>小时</th><th>笔数</th><th>胜率</th><th>盈亏</th></tr></thead>
                <tbody>
                  <tr v-for="row in result.by_hour || []" :key="row.hour">
                    <td class="mono">{{ String(row.hour).padStart(2, '0') }}:00</td>
                    <td class="mono">{{ row.trades }}</td>
                    <td class="mono">{{ row.win_rate }}%</td>
                    <td class="mono" :class="numClass(row.pnl)">{{ row.pnl }}</td>
                  </tr>
                </tbody>
              </table>
            </div>
            <div>
              <div class="muted" style="font-size: 11px; margin-bottom: 6px">按星期</div>
              <table class="table">
                <thead><tr><th>星期</th><th>笔数</th><th>胜率</th><th>盈亏</th></tr></thead>
                <tbody>
                  <tr v-for="row in result.by_weekday || []" :key="row.weekday">
                    <td>{{ row.weekday }}</td>
                    <td class="mono">{{ row.trades }}</td>
                    <td class="mono">{{ row.win_rate }}%</td>
                    <td class="mono" :class="numClass(row.pnl)">{{ row.pnl }}</td>
                  </tr>
                </tbody>
              </table>
            </div>
            <div>
              <div class="muted" style="font-size: 11px; margin-bottom: 6px">按持仓时长</div>
              <table class="table">
                <thead><tr><th>区间</th><th>笔数</th><th>胜率</th><th>盈亏</th></tr></thead>
                <tbody>
                  <tr v-for="row in result.by_hold || []" :key="row.bucket">
                    <td class="mono">{{ row.bucket }}</td>
                    <td class="mono">{{ row.trades }}</td>
                    <td class="mono">{{ row.win_rate }}%</td>
                    <td class="mono" :class="numClass(row.pnl)">{{ row.pnl }}</td>
                  </tr>
                </tbody>
              </table>
            </div>
          </div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">成交明细（最近 100 笔，倒序）</span>
          <div class="right">
            <span class="tag tag-warn">粗筛结果，非实盘预期</span>
            <button class="btn btn-xs" :disabled="!trades.length" @click="exportTrades">导出 CSV</button>
          </div>
        </div>
        <div class="panel-body" style="padding: 0">
          <div v-if="!trades.length" class="empty" style="padding: 36px">该区间没有触发任何信号</div>
          <table v-else class="table">
            <thead>
              <tr>
                <th>方向</th>
                <th>开仓时间</th>
                <th>开仓价</th>
                <th>平仓价</th>
                <th>盈亏</th>
                <th>涨跌%</th>
                <th>持仓(根)</th>
                <th>出场原因</th>
                <th>评分</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(t, i) in trades" :key="i">
                <td :class="t.side === 'long' ? 'up' : 'down'">{{ t.side === 'long' ? '多' : '空' }}</td>
                <td class="mono muted">{{ timeStr(t.entry_ts) }}</td>
                <td class="mono">{{ t.entry }}</td>
                <td class="mono">{{ t.exit }}</td>
                <td class="mono" :class="numClass(t.pnl)">{{ t.pnl }}</td>
                <td class="mono" :class="numClass(t.pnl_pct)">{{ t.pnl_pct }}%</td>
                <td class="mono muted">{{ t.bars }}</td>
                <td class="muted">{{ EXIT_LABELS[t.reason] || t.reason }}</td>
                <td class="mono muted">{{ t.score }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    </template>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">参数网格</span>
        <div class="right">
          <span class="tag tag-warn">样本内排序，过拟合自负</span>
          <span class="tag tag-dim">最多 48 组</span>
          <button class="btn btn-xs" :disabled="!gridResult?.items?.length" @click="exportGrid">导出 CSV</button>
        </div>
      </div>
      <div class="panel-body stack" style="gap: 12px">
        <div class="hint">
          逗号分隔多个取值（每个参数最多 4 个），与上方标的/周期/费率共用。扫描按总收益 + 盈亏比排序，
          只说明「在这段样本里哪组更能打」，不代表未来表现。
        </div>
        <div class="grid g4 bt-form" style="gap: 10px">
          <label class="field">
            <span class="label">入场阈值</span>
            <input v-model="gridForm.entry_threshold" class="input mono" placeholder="0.2, 0.3, 0.4" />
          </label>
          <label class="field">
            <span class="label">止损 ATR 倍数</span>
            <input v-model="gridForm.sl_atr_mult" class="input mono" placeholder="1.0, 1.2, 1.5" />
          </label>
          <label class="field">
            <span class="label">止盈 ATR 倍数</span>
            <input v-model="gridForm.tp_atr_mult" class="input mono" placeholder="1.5, 2.0, 2.5" />
          </label>
          <label class="field">
            <span class="label">移动止损 %</span>
            <input v-model="gridForm.trailing_pct" class="input mono" placeholder="0.6, 0.8" />
          </label>
        </div>
        <div class="row" style="gap: 8px">
          <button class="btn btn-primary" :disabled="gridRunning || !form.symbol" @click="runGrid">
            {{ gridRunning ? '扫描中…' : '开始扫描' }}
          </button>
          <span v-if="gridError" class="muted" style="color: var(--down); font-size: 11.5px">{{ gridError }}</span>
          <span v-else-if="gridResult" class="muted" style="font-size: 11.5px">
            {{ gridResult.runs }} 组 · 耗时 {{ gridResult.elapsed_ms }}ms
            <template v-if="gridResult.truncated">（已截断到 {{ gridResult.max_runs }} 组）</template>
          </span>
        </div>

        <div v-if="gridResult" class="panel" style="padding: 0">
          <table class="table">
            <thead>
              <tr>
                <th>#</th>
                <th>参数</th>
                <th>交易数</th>
                <th>胜率</th>
                <th>盈亏比</th>
                <th>单笔期望</th>
                <th>总收益</th>
                <th>最大回撤</th>
                <th>平均持仓</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="(row, i) in gridResult.items" :key="i">
                <td class="mono muted">{{ i + 1 }}</td>
                <td class="mono" style="white-space: normal">
                  <span v-for="(v, k) in row.params" :key="k" class="tag tag-dim" style="margin-right: 4px">
                    {{ k }}={{ v }}
                  </span>
                </td>
                <td class="mono">{{ row.summary.trades }}</td>
                <td class="mono">{{ row.summary.win_rate }}%</td>
                <td class="mono">{{ row.summary.profit_factor }}</td>
                <td class="mono" :class="numClass(row.summary.expectancy)">{{ row.summary.expectancy }}</td>
                <td class="mono" :class="pnlClass(row.summary.total_return_pct)">{{ row.summary.total_return_pct }}%</td>
                <td class="mono down">{{ row.summary.max_drawdown_pct }}%</td>
                <td class="mono muted">{{ row.summary.avg_bars }}</td>
              </tr>
            </tbody>
          </table>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.bt-form .field {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.bt-form .label {
  font-size: 10.5px;
  color: var(--muted);
}
.table {
  width: 100%;
  border-collapse: collapse;
  font-size: 11.5px;
}
.table th,
.table td {
  padding: 7px 10px;
  text-align: left;
  border-bottom: 1px solid rgba(148, 170, 200, 0.07);
  white-space: nowrap;
}
.table th {
  color: var(--muted);
  font-size: 10px;
  font-weight: 500;
}
.table tbody tr:hover {
  background: rgba(148, 170, 200, 0.04);
}
.g6 {
  grid-template-columns: repeat(6, minmax(0, 1fr));
}
@media (max-width: 900px) {
  .g6 {
    grid-template-columns: repeat(3, minmax(0, 1fr));
  }
}
</style>
