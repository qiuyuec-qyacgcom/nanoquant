<script setup lang="ts">
defineProps<{
  app: any
  unprotected: any[]
  anchorBusy: boolean
  protectBusy: boolean
}>()

const emit = defineEmits<{
  (e: 'reset-anchors'): void
  (e: 'protect-all'): void
}>()
</script>

<template>
  <div class="section">
    <div class="section-hd">
      <span class="section-title">日亏基准</span>
      <span class="section-hint">
        日亏与回撤都相对「当日初始权益」计算；充值、重置模拟盘资金、换账户后必须对齐一次
      </span>
    </div>
    <div class="row" style="gap: 10px; flex-wrap: wrap; align-items: center">
      <span class="tag tag-dim">
        当前权益 {{ (app.status?.account?.equity ?? 0).toFixed(2) }}
      </span>
      <span v-if="app.status?.halted" class="tag tag-warn" :title="app.status.halted">
        熔断中 · {{ app.status.halted.slice(0, 24) }}
      </span>
      <span v-else class="tag tag-long">未熔断</span>
      <button class="btn btn-sm" :disabled="anchorBusy" @click="emit('reset-anchors')">
        {{ anchorBusy ? '重置中…' : '重置基准到当前权益' }}
      </button>
    </div>
    <div v-if="unprotected.length" class="row" style="gap: 10px; flex-wrap: wrap; align-items: center; margin-top: 8px">
      <span class="tag tag-warn">
        {{ unprotected.length }} 个仓位没有止损/止盈：{{ unprotected.map((p) => p.symbol).slice(0, 4).join('、') }}
      </span>
      <button class="btn btn-sm" :disabled="protectBusy" @click="emit('protect-all')">
        {{ protectBusy ? '补挂中…' : '按 ATR 补挂保护单' }}
      </button>
      <span class="muted" style="font-size: 10.5px">手动开的仓不走系统下单链路，需要补挂后才受托管</span>
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
</style>