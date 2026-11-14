<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
// 按需引入：整包 echarts 1.0MB(gzip 343KB)，这里只用到折线/柱状/K 线与少量组件，
// 全量引入会把首屏体积的一半浪费在永远不会渲染的图表类型上。
import * as echarts from 'echarts/core'
import { BarChart, CandlestickChart, LineChart } from 'echarts/charts'
import {
  AxisPointerComponent,
  DataZoomComponent,
  GridComponent,
  LegendComponent,
  TitleComponent,
  TooltipComponent,
} from 'echarts/components'
import { CanvasRenderer } from 'echarts/renderers'
import type { EChartsType } from 'echarts/core'

echarts.use([
  LineChart,
  BarChart,
  CandlestickChart,
  GridComponent,
  TooltipComponent,
  AxisPointerComponent,
  DataZoomComponent,
  TitleComponent,
  LegendComponent,
  CanvasRenderer,
])

const props = defineProps<{ option: any; height?: string; loading?: boolean }>()
const el = ref<HTMLDivElement | null>(null)
let chart: EChartsType | null = null

function render() {
  if (!el.value) return
  if (!chart) chart = echarts.init(el.value, undefined, { renderer: 'canvas' })
  chart.setOption(props.option || {}, true)
  if (props.loading) chart.showLoading('default', { text: '', color: '#3b82f6', maskColor: 'rgba(5,8,15,0.5)' })
  else chart.hideLoading()
}

function resize() {
  chart?.resize()
}

onMounted(() => {
  render()
  window.addEventListener('resize', resize)
})

watch(() => props.option, render, { deep: true })

onBeforeUnmount(() => {
  window.removeEventListener('resize', resize)
  chart?.dispose()
  chart = null
})
</script>

<template>
  <div ref="el" class="chart" :style="{ height: height || '280px' }" />
</template>
