<script setup lang="ts">
import { computed } from 'vue'

const props = defineProps<{ data: number[]; height?: number }>()

const path = computed(() => {
  const data = props.data || []
  if (data.length < 2) return ''
  const min = Math.min(...data)
  const max = Math.max(...data)
  const span = max - min || 1
  const w = 100
  const h = 34
  const step = w / (data.length - 1)
  return data
    .map((v, i) => `${i === 0 ? 'M' : 'L'}${(i * step).toFixed(2)},${(h - ((v - min) / span) * h).toFixed(2)}`)
    .join(' ')
})

const rising = computed(() => {
  const d = props.data || []
  return d.length > 1 && d[d.length - 1] >= d[0]
})
</script>

<template>
  <svg :viewBox="`0 0 100 34`" preserveAspectRatio="none" class="spark" :style="{ height: (height || 40) + 'px' }">
    <defs>
      <linearGradient :id="rising ? 'sparkUp' : 'sparkDown'" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" :stop-color="rising ? '#12c48b' : '#f04b5a'" stop-opacity="0.35" />
        <stop offset="100%" :stop-color="rising ? '#12c48b' : '#f04b5a'" stop-opacity="0" />
      </linearGradient>
    </defs>
    <path v-if="path" :d="`${path} L100,34 L0,34 Z`" :fill="rising ? 'url(#sparkUp)' : 'url(#sparkDown)'" />
    <path v-if="path" :d="path" fill="none" :stroke="rising ? '#12c48b' : '#f04b5a'" stroke-width="1.4" />
  </svg>
</template>

<style scoped>
.spark {
  width: 100%;
  display: block;
  overflow: visible;
}
</style>
