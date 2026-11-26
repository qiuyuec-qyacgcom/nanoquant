<script setup lang="ts">
import SettingsField from './SettingsField.vue'
import type { SectionGroup } from './types'

defineProps<{
  section: SectionGroup
  draft: Record<string, any>
  changedKeys: string[]
  focusKey: string
  groupId: string
}>()

const emit = defineEmits<{
  (e: 'update', key: string, value: any): void
}>()
</script>

<template>
  <div class="section">
    <div class="section-hd">
      <span class="section-title">{{ section.title }}</span>
      <span v-if="section.hint" class="section-hint">{{ section.hint }}</span>
    </div>

    <SettingsField
      v-for="f in section.rows"
      :key="f.key"
      :field="f"
      :model-value="draft[f.key]"
      :field-id="`fld-${groupId}-${f.key}`"
      :focus="focusKey === f.key"
      :changed="changedKeys.includes(f.key)"
      @update:model-value="(v) => emit('update', f.key, v)"
    />
  </div>
</template>

<style scoped>
.section {
  display: flex;
  flex-direction: column;
  padding: 2px 0 4px;
}
.section + .section {
  border-top: 1px solid var(--line-soft);
  padding-top: 14px;
}
.section-hd {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  margin-bottom: 4px;
}
.section-title {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text);
}
.section-hint {
  font-size: 10.5px;
  color: var(--muted);
}
</style>