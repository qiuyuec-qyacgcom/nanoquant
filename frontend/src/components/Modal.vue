<script setup lang="ts">
import { onBeforeUnmount, onMounted } from 'vue'

defineProps<{ title: string; wide?: boolean }>()
const emit = defineEmits<{ (e: 'close'): void }>()

function onKey(event: KeyboardEvent) {
  if (event.key === 'Escape') emit('close')
}

onMounted(() => window.addEventListener('keydown', onKey))
onBeforeUnmount(() => window.removeEventListener('keydown', onKey))
</script>

<template>
  <teleport to="body">
    <div class="modal-mask" @click.self="emit('close')">
      <div class="modal" :class="{ wide }" role="dialog" aria-modal="true" :aria-label="title">
        <div class="modal-hd">
          <span>{{ title }}</span>
          <div class="spacer" />
          <button class="btn btn-sm btn-ghost" aria-label="关闭" @click="emit('close')">✕</button>
        </div>
        <div class="modal-bd">
          <slot />
        </div>
        <div v-if="$slots.footer" class="modal-ft">
          <slot name="footer" />
        </div>
      </div>
    </div>
  </teleport>
</template>
