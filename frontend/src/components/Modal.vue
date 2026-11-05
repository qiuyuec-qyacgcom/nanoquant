<script setup lang="ts">
defineProps<{ title: string; wide?: boolean }>()
const emit = defineEmits<{ (e: 'close'): void }>()
</script>

<template>
  <teleport to="body">
    <div class="modal-mask" @click.self="emit('close')">
      <div class="modal" :class="{ wide }">
        <div class="modal-hd">
          <span>{{ title }}</span>
          <div class="spacer" />
          <button class="btn btn-sm" @click="emit('close')">✕</button>
        </div>
        <div class="modal-bd">
          <slot />
        </div>
        <div class="modal-ft" v-if="$slots.footer">
          <slot name="footer" />
        </div>
      </div>
    </div>
  </teleport>
</template>
