<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { filterCommands, type CommandItem } from './commandPalette'

const props = defineProps<{
  open: boolean
  items: CommandItem[]
}>()

const emit = defineEmits<{
  close: []
  select: [path: string]
}>()

const query = ref('')
const active = ref(0)
const input = ref<HTMLInputElement | null>(null)

const results = computed(() => filterCommands(props.items, query.value))

watch(
  () => props.open,
  async (open) => {
    if (!open) return
    query.value = ''
    active.value = 0
    await nextTick()
    input.value?.focus()
  },
)

watch(results, () => {
  active.value = 0
})

function move(delta: number) {
  if (!results.value.length) return
  active.value = (active.value + delta + results.value.length) % results.value.length
}

function choose(path?: string) {
  const target = path || results.value[active.value]?.path
  if (target) emit('select', target)
}

function onKeydown(event: KeyboardEvent) {
  if (event.key === 'Escape') {
    emit('close')
    return
  }
  if (event.key === 'ArrowDown') {
    event.preventDefault()
    move(1)
    return
  }
  if (event.key === 'ArrowUp') {
    event.preventDefault()
    move(-1)
    return
  }
  if (event.key === 'Enter') {
    event.preventDefault()
    choose()
  }
}
</script>

<template>
  <div v-if="open" class="command-mask" @click.self="emit('close')">
    <section class="command-box" role="dialog" aria-label="页面导航">
      <header class="command-head">
        <span class="prompt">›</span>
        <input
          ref="input"
          v-model="query"
          class="command-input"
          placeholder="搜索页面 / 路径"
          @keydown="onKeydown"
        />
        <kbd>ESC</kbd>
      </header>
      <div class="command-list">
        <button
          v-for="(item, index) in results"
          :key="item.path"
          type="button"
          class="command-item"
          :class="{ active: index === active }"
          @mouseenter="active = index"
          @click="choose(item.path)"
        >
          <span class="ico">{{ item.ico || '›' }}</span>
          <span>{{ item.label }}</span>
          <span class="path">{{ item.path }}</span>
        </button>
        <div v-if="!results.length" class="command-empty">没有匹配页面</div>
      </div>
      <footer class="command-foot">
        <span>↑↓ 选择</span>
        <span>Enter 跳转</span>
        <span class="safe">仅页面导航 · 不执行交易操作</span>
      </footer>
    </section>
  </div>
</template>

<style scoped>
.command-mask {
  position: fixed;
  inset: 0;
  z-index: 90;
  display: flex;
  justify-content: center;
  padding-top: 13vh;
  background: rgba(2, 5, 10, 0.68);
  backdrop-filter: blur(4px);
}

.command-box {
  width: min(560px, calc(100vw - 32px));
  align-self: flex-start;
  border: 1px solid var(--line-strong);
  border-radius: 2px;
  background: var(--panel);
  box-shadow: 0 28px 80px rgba(0, 0, 0, 0.55);
  color: var(--text);
}

.command-head {
  display: flex;
  align-items: center;
  gap: 9px;
  height: 46px;
  padding: 0 13px;
  border-bottom: 1px solid var(--line);
}

.prompt {
  color: var(--accent);
  font-family: var(--mono);
  font-size: 17px;
}

.command-input {
  flex: 1;
  border: 0;
  outline: 0;
  background: transparent;
  color: var(--text);
  font-family: var(--mono);
  font-size: 12px;
}

.command-input::placeholder {
  color: var(--muted);
}

.command-head kbd,
.command-foot kbd {
  border: 1px solid var(--line);
  padding: 1px 5px;
  color: var(--muted);
  font-family: var(--mono);
  font-size: 8px;
}

.command-list {
  max-height: 320px;
  overflow: auto;
  padding: 6px;
}

.command-item {
  width: 100%;
  display: flex;
  align-items: center;
  gap: 9px;
  min-height: 34px;
  padding: 0 9px;
  border: 1px solid transparent;
  background: transparent;
  color: var(--text-dim);
  font-size: 11px;
  text-align: left;
}

.command-item.active {
  border-color: color-mix(in srgb, var(--accent) 30%, transparent);
  background: var(--accent-soft);
  color: var(--text);
}

.command-item .ico {
  width: 18px;
  color: var(--accent);
  font-family: var(--mono);
  text-align: center;
}

.command-item .path {
  margin-left: auto;
  color: var(--muted);
  font-family: var(--mono);
  font-size: 8px;
}

.command-empty {
  padding: 18px 12px;
  color: var(--muted);
  font-family: var(--mono);
  font-size: 10px;
}

.command-foot {
  display: flex;
  gap: 14px;
  padding: 9px 12px;
  border-top: 1px solid var(--line);
  color: var(--muted);
  font-family: var(--mono);
  font-size: 8px;
}

.command-foot .safe {
  margin-left: auto;
  color: var(--dim);
}
</style>