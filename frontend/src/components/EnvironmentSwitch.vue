<script setup lang="ts">
/**
 * 交易环境切换 (模拟盘 / 实盘)
 * ---------------------------------------------------------------
 * 与 astra-quant-agent 的「统一交易环境」一致:
 * - 一个分段控件决定 demo(模拟盘) / live(实盘), 其余参数不再各自为政;
 * - 切实盘必须二次确认并输入确认短语 LIVE;
 * - 行情源/测试网只作为状态展示, 不在这里改 (由「参数配置 → 数据源」维护)。
 */
import { computed, ref } from 'vue'
import { useAppStore } from '../stores/app'
import Modal from './Modal.vue'

withDefaults(defineProps<{ compact?: boolean; showMeta?: boolean }>(), {
  compact: false,
  showMeta: true,
})
const emit = defineEmits<{ (e: 'changed', mode: 'demo' | 'live'): void }>()

const app = useAppStore()
const busy = ref(false)
const error = ref('')
const pending = ref<'demo' | 'live' | ''>('')
const phrase = ref('')

const mode = computed<'demo' | 'live'>(() => (app.status?.mode === 'live' ? 'live' : 'demo'))
const isLive = computed(() => mode.value === 'live')
const envLabel = computed(
  () => app.status?.env_label || (isLive.value ? '实盘 · 币安主网' : '模拟盘 · 币安测试网')
)
const accountError = computed(() => (app.status?.account as any)?.error || '')
const modalOpen = computed(() => pending.value === 'live')

function request(target: 'demo' | 'live') {
  if (busy.value || target === mode.value) return
  error.value = ''
  if (target === 'live') {
    phrase.value = ''
    pending.value = 'live'
    return
  }
  void commit('demo', '')
}

function closeModal() {
  if (busy.value) return
  pending.value = ''
  phrase.value = ''
  error.value = ''
}

async function commit(target: 'demo' | 'live', confirmText: string) {
  busy.value = true
  error.value = ''
  try {
    const res = await app.control({ mode: target, confirm: confirmText })
    pending.value = ''
    phrase.value = ''
    app.notify(target === 'live' ? '已切换到实盘' : '已切换到模拟盘', 'ok')
    if (res?.warning) app.notify(res.warning, 'info')
    emit('changed', target)
  } catch (err: any) {
    if (target === 'live') error.value = err.message
    else app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

async function confirmLive() {
  if (phrase.value.trim().toUpperCase() !== 'LIVE') {
    error.value = '请输入确认短语 LIVE'
    return
  }
  await commit('live', 'LIVE')
}
</script>

<template>
  <div class="env" :class="{ compact }">
    <div class="env-seg" role="group" aria-label="交易环境">
      <button
        type="button"
        class="env-opt"
        :class="{ on: !isLive }"
        :disabled="busy"
        :aria-pressed="!isLive"
        @click="request('demo')"
      >
        <span class="env-dot demo" />
        <span class="env-main">模拟盘</span>
        <span class="env-sub">DEMO</span>
      </button>
      <button
        type="button"
        class="env-opt live"
        :class="{ on: isLive }"
        :disabled="busy"
        :aria-pressed="isLive"
        @click="request('live')"
      >
        <span class="env-dot live" />
        <span class="env-main">实盘</span>
        <span class="env-sub">LIVE</span>
      </button>
    </div>

    <div v-if="showMeta" class="env-meta">
      <span class="tag" :class="isLive ? 'tag-short' : 'tag-dim'">{{ envLabel }}</span>
      <span v-if="accountError" class="tag tag-warn" :title="accountError">账户不可用</span>
    </div>

    <Modal v-if="modalOpen" title="切换到实盘 LIVE" @close="closeModal">
      <div class="stack" style="gap: 12px">
        <div class="danger-box">
          <b>真实资金将开始下单。</b>
          引擎会使用已配置的<b>实盘 API Key</b> 在 <b>币安主网</b> 执行市价单，
          盈亏不可撤销。请确认标的池、杠杆、单笔风险与止损参数均已核对。
        </div>
        <div class="field">
          <span class="label">确认短语</span>
          <input
            v-model="phrase"
            class="input mono"
            placeholder="输入 LIVE"
            autocomplete="off"
            @keyup.enter="confirmLive"
          />
          <span class="hint">输入大写 <span class="mono">LIVE</span> 才能提交，避免误触。</span>
        </div>
        <div v-if="error" class="down" style="font-size: 12px">{{ error }}</div>
      </div>
      <template #footer>
        <button class="btn btn-sm" :disabled="busy" @click="closeModal">取消</button>
        <button class="btn btn-sm btn-danger" :disabled="busy" @click="confirmLive">
          {{ busy ? '切换中…' : '确认切换实盘' }}
        </button>
      </template>
    </Modal>
  </div>
</template>

<style scoped>
.env {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 12px;
}
.env-seg {
  display: inline-flex;
  padding: 3px;
  gap: 3px;
  background: #070d18;
  border: 1px solid var(--line);
  border-radius: 10px;
}
.env-opt {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  padding: 7px 14px;
  border: 1px solid transparent;
  border-radius: 8px;
  background: transparent;
  color: var(--text-dim);
  font-size: 12.5px;
  font-family: var(--sans);
  cursor: pointer;
  transition: all 0.15s ease;
}
.env-opt:hover:not(:disabled) {
  color: var(--text);
  background: rgba(59, 130, 246, 0.06);
}
.env-opt:disabled {
  opacity: 0.55;
  cursor: not-allowed;
}
.env-opt.on {
  background: rgba(59, 130, 246, 0.14);
  border-color: rgba(59, 130, 246, 0.4);
  color: #dbe9ff;
}
.env-opt.live.on {
  background: rgba(240, 75, 90, 0.14);
  border-color: rgba(240, 75, 90, 0.42);
  color: #ffd9dd;
}
.env-main {
  font-weight: 600;
}
.env-sub {
  font-size: 9.5px;
  letter-spacing: 0.6px;
  opacity: 0.7;
  font-family: var(--mono);
}
.env-dot {
  width: 7px;
  height: 7px;
  border-radius: 999px;
  background: var(--muted);
}
.env-dot.demo {
  background: var(--up);
  box-shadow: 0 0 6px var(--up);
}
.env-dot.live {
  background: var(--down);
  box-shadow: 0 0 6px var(--down);
}
.env-meta {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  font-size: 11px;
}
.env-src {
  color: var(--text-dim);
}
.env-src.warn {
  color: var(--warn);
}
.env.compact .env-opt {
  padding: 5px 10px;
  font-size: 11.5px;
}
.danger-box {
  padding: 10px 12px;
  border: 1px solid rgba(240, 75, 90, 0.35);
  background: rgba(240, 75, 90, 0.08);
  border-radius: 8px;
  font-size: 12px;
  line-height: 1.75;
  color: #ffd9dd;
}
</style>
