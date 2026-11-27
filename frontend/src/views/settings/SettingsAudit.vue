<script setup lang="ts">
defineProps<{
  rows: any[]
  fieldLabel: (key: string) => string
  tsText: (ts: number) => string
  shortValue: (v: any) => string
}>()
</script>

<template>
  <div class="section">
    <div class="section-hd">
      <span class="section-title">最近变更</span>
      <span class="section-hint">谁在什么时候把这一组改成了什么；密钥类字段只记录"改过"，不记录内容</span>
    </div>
    <div v-for="row in rows" :key="row.id" class="audit-row">
      <span class="audit-time">{{ tsText(row.ts) }}</span>
      <span class="audit-user">{{ row.user || '-' }}</span>
      <span class="audit-diff">
        <span v-for="(val, key) in row.changes" :key="key" class="audit-item">
          {{ fieldLabel(String(key)) }}
          <s>{{ shortValue(val.from) }}</s>
          →
          {{ shortValue(val.to) }}
        </span>
      </span>
    </div>
  </div>
</template>

<style scoped>
.section {
  display: flex;
  flex-direction: column;
  padding: 2px 0 4px;
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

/* ------------------------------------------------ 参数变更记录 */
.audit-row {
  display: grid;
  grid-template-columns: 84px 72px minmax(0, 1fr);
  gap: 10px;
  align-items: baseline;
  padding: 6px 0;
  border-top: 1px dashed var(--line);
  font-size: 11px;
}
.audit-row:first-of-type {
  border-top: none;
}
.audit-time {
  color: var(--muted);
  font-family: var(--mono);
  font-size: 10.5px;
}
.audit-user {
  color: var(--text-dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.audit-diff {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 10px;
  color: var(--text);
}
.audit-item s {
  color: var(--muted);
  text-decoration-color: rgba(240, 75, 90, 0.6);
}
</style>