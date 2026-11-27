<script setup lang="ts">
import { computed } from 'vue'
import { useRoute } from 'vue-router'
import { workspaceById, type Workspace } from '../workspaces'

const route = useRoute()
const workspace = computed<Workspace | undefined>(() =>
  workspaceById(String(route.meta.workspace || '')),
)
</script>

<template>
  <section class="workspace">
    <header v-if="workspace" class="workspace-head">
      <div class="workspace-copy">
        <div class="workspace-title">{{ workspace.label }}</div>
        <div class="workspace-hint">{{ workspace.hint }}</div>
      </div>
      <nav class="workspace-tabs" aria-label="工作区页面">
        <router-link
          v-for="tab in workspace.tabs"
          :key="tab.path"
          :to="tab.path"
          class="workspace-tab"
          :class="{ active: route.path === tab.path }"
        >
          {{ tab.label }}
        </router-link>
      </nav>
    </header>
    <router-view />
  </section>
</template>

<style scoped>
.workspace {
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.workspace-head {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-end;
  justify-content: space-between;
  gap: 12px;
  padding: 12px 14px;
  border: 1px solid var(--line);
  border-radius: 10px;
  background: var(--panel);
}

.workspace-title {
  font-size: 15px;
  font-weight: 600;
  letter-spacing: 0.04em;
}

.workspace-hint {
  margin-top: 4px;
  font-size: 11px;
  color: var(--muted);
}

.workspace-tabs {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}

.workspace-tab {
  padding: 6px 12px;
  border: 1px solid var(--line);
  border-radius: 999px;
  color: var(--muted);
  font-size: 12px;
  text-decoration: none;
  transition: color 0.15s ease, border-color 0.15s ease, background 0.15s ease;
}

.workspace-tab:hover {
  color: var(--accent-2);
  border-color: var(--accent);
}

.workspace-tab.active {
  color: var(--accent);
  border-color: var(--accent);
  background: var(--accent-soft, rgba(77, 163, 255, 0.09));
}
</style>
