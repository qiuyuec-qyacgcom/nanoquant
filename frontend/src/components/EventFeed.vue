<script setup lang="ts">
import { computed } from 'vue'
import type { EventItem } from '../types'
import { visibleEvents } from '../utils/events'
import { timeStr } from '../utils/format'

const props = defineProps<{ events: EventItem[]; height?: string }>()
const visible = computed(() => visibleEvents(props.events))
</script>

<template>
  <div class="scroll-y" :style="{ maxHeight: height || '320px' }">
    <div v-if="!visible.length" class="empty">暂无事件</div>
    <div v-for="e in visible" :key="e.id ?? e.ts + e.message" class="event-row">
      <span class="event-time">{{ timeStr(e.ts) }}</span>
      <span class="event-cat" :class="{ down: e.level === 'error', up: e.level === 'warn' }">{{ e.category }}</span>
      <span class="event-msg" :class="{ down: e.level === 'error', muted: e.level === 'debug' }">{{ e.message }}</span>
    </div>
  </div>
</template>
