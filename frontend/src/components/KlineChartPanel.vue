<script setup lang="ts">
/**
 * klinecharts 封装 (与 astra-quant-agent 的 ChartWorkstation 同款图表库)
 * ---------------------------------------------------------------------
 * - 自带十字光标 / 缩放 / 拖动 / 指标计算;
 * - 增量刷新: 同一根蜡烛只 updateData, 出现新蜡烛才 applyNewData,
 *   并在全量重建后恢复缩放级别, 避免每 3 秒把用户视角重置。
 */
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { dispose, init, type Chart } from 'klinecharts'
import type { Candle } from '../types/candle'

const props = withDefaults(
  defineProps<{
    items: Candle[]
    height?: string
    /** 主图指标: MA / EMA / BOLL / NONE */
    main?: string
    /** 副图指标: VOL / MACD / RSI / NONE */
    sub?: string
  }>(),
  { height: '430px', main: 'MA', sub: 'VOL' }
)

const emit = defineEmits<{ (e: 'ready', chart: Chart): void }>()

const el = ref<HTMLDivElement | null>(null)
let chart: Chart | null = null
let observer: ResizeObserver | null = null
let rendered = { length: 0, first: 0 }

const STYLES: Record<string, any> = {
  grid: {
    horizontal: { color: 'rgba(148,170,200,0.10)' },
    vertical: { color: 'rgba(148,170,200,0.10)' },
  },
  candle: {
    bar: {
      upColor: '#12c48b',
      downColor: '#f04b5a',
      noChangeColor: '#7c8ba1',
      upBorderColor: '#12c48b',
      downBorderColor: '#f04b5a',
      upWickColor: '#12c48b',
      downWickColor: '#f04b5a',
    },
    priceMark: {
      high: { text: { color: '#dbe6f6' } },
      low: { text: { color: '#dbe6f6' } },
      last: {
        text: { color: '#0b1220' },
        line: { style: 'dashed' },
      },
    },
    tooltip: {
      text: { color: '#dbe6f6', size: 11 },
      rect: {
        color: 'rgba(9,16,28,0.92)',
        borderColor: 'rgba(148,170,200,0.25)',
        borderSize: 1,
        borderRadius: 4,
      },
    },
  },
  indicator: {
    lines: [{ color: '#f2c94c' }, { color: '#56ccf2' }, { color: '#bb6bd9' }],
    tooltip: { text: { color: '#a9bad2', size: 11 } },
    lastValueMark: { text: { color: '#dbe6f6' } },
  },
  xAxis: {
    axisLine: { color: 'rgba(148,170,200,0.25)' },
    tickLine: { color: 'rgba(148,170,200,0.25)' },
    tickText: { color: '#7c8ba1', size: 10 },
  },
  yAxis: {
    axisLine: { color: 'rgba(148,170,200,0.25)' },
    tickLine: { color: 'rgba(148,170,200,0.25)' },
    tickText: { color: '#7c8ba1', size: 10 },
  },
  separator: { color: 'rgba(148,170,200,0.25)' },
  crosshair: {
    horizontal: {
      line: { color: 'rgba(200,214,235,0.45)' },
      text: { backgroundColor: '#1f3557', color: '#ffffff' },
    },
    vertical: {
      line: { color: 'rgba(200,214,235,0.45)' },
      text: { backgroundColor: '#1f3557', color: '#ffffff' },
    },
  },
}

function toData(items: Candle[]) {
  return items.map((c) => ({
    timestamp: c.t,
    open: c.o,
    high: c.h,
    low: c.l,
    close: c.c,
    volume: c.v,
    turnover: c.v * c.c,
  }))
}

function restoreViewport() {
  const space = (chart as any).getBarSpace?.()
  const offset = (chart as any).getOffsetRightDistance?.()
  return () => {
    if (typeof space === 'number' && (chart as any).setBarSpace) (chart as any).setBarSpace(space)
    if (typeof offset === 'number' && (chart as any).setOffsetRightDistance) {
      (chart as any).setOffsetRightDistance(offset)
    }
  }
}

function syncData() {
  if (!chart) return
  const items = props.items || []
  if (!items.length) {
    chart.applyNewData([])
    rendered = { length: 0, first: 0 }
    return
  }
  const data = toData(items)
  const appended =
    rendered.length > 0 && items.length === rendered.length + 1 && items[0].t === rendered.first
  const replaced =
    rendered.length > 0 && items.length === rendered.length && items[0].t === rendered.first

  if (appended) {
    // 新一根蜡烛: 优先增量追加, 若图表库不支持追加再全量重建并恢复视角
    chart.updateData(data[data.length - 1])
    const size = (chart as any).getDataList?.().length ?? items.length
    if (size < items.length) {
      const restore = restoreViewport()
      chart.applyNewData(data)
      restore()
    }
  } else if (replaced) {
    // 未收盘蜡烛更新: 只更新最后一根, 保持缩放与滚动位置
    chart.updateData(data[data.length - 1])
  } else {
    // 首次加载 / 切标的 / 切周期: 全量重建
    chart.applyNewData(data)
  }
  rendered = { length: items.length, first: items[0].t }
}

function buildChart() {
  if (!el.value) return
  chart = init(el.value, { styles: STYLES })
  if (!chart) return
  if (props.main && props.main !== 'NONE') {
    chart.createIndicator(props.main, false, { id: 'candle_pane' })
  }
  if (props.sub && props.sub !== 'NONE') {
    chart.createIndicator(props.sub, true, { id: `${props.sub.toLowerCase()}_pane`, height: 78 })
  }
  ;(el.value as any).__chart = chart
  rendered = { length: 0, first: 0 }
  syncData()
  emit('ready', chart)
}

onMounted(() => {
  if (!el.value) return
  buildChart()
  observer = new ResizeObserver(() => chart?.resize())
  observer.observe(el.value)
})

onBeforeUnmount(() => {
  observer?.disconnect()
  observer = null
  if (el.value) {
    delete (el.value as any).__chart
    dispose(el.value)
  }
  chart = null
})

watch(() => props.items, syncData)
</script>

<template>
  <div ref="el" class="kline-panel" :style="{ height }" />
</template>

<style scoped>
.kline-panel {
  width: 100%;
  border-radius: 8px;
  border: 1px solid var(--line);
  background: radial-gradient(120% 120% at 0% 0%, rgba(24, 38, 60, 0.55), rgba(7, 12, 22, 0.9));
  overflow: hidden;
}
</style>
