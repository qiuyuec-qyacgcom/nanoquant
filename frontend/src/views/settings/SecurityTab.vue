<script setup lang="ts">
defineProps<{
  pwd: { old_password: string; new_password: string }
}>()

const emit = defineEmits<{
  (e: 'change'): void
}>()
</script>

<template>
  <div class="section">
    <div class="section-hd">
      <span class="section-title">修改密码</span>
      <span class="section-hint">修改后已登录会话立即失效</span>
    </div>
    <div class="cred-grid">
      <div class="field">
        <span class="label">原密码</span>
        <input v-model="pwd.old_password" type="password" class="input" autocomplete="current-password" />
      </div>
      <div class="field">
        <span class="label">新密码</span>
        <input v-model="pwd.new_password" type="password" class="input" autocomplete="new-password" placeholder="至少 6 位" />
      </div>
    </div>
    <div class="row">
      <button class="btn btn-sm btn-primary" @click="emit('change')">更新密码</button>
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
.cred-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(190px, 1fr));
  gap: 10px;
  margin-bottom: 10px;
}
</style>