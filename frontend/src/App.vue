<script setup lang="ts">
import { computed } from 'vue'
import { useAppStore } from './stores/app'

const app = useAppStore()
const toastClass = computed(() => `toast toast-${app.toast.type} ${app.toast.visible ? 'show' : ''}`)
</script>

<template>
  <router-view />
  <div :class="toastClass">{{ app.toast.message }}</div>
</template>
