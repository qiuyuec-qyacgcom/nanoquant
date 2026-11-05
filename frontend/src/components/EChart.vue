<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
import * as echarts from 'echarts'

const props = defineProps<{ option: any; height?: string; loading?: boolean }>()
const el = ref<HTMLDivElement | null>(null)
let chart: echarts.ECharts | null = null

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
