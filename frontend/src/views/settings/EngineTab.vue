<script setup lang="ts">
defineProps<{
  tradeMode: any
  tradeModeError: string
  isolatedOnly: boolean
  isolatedBusy: boolean
}>()

const emit = defineEmits<{
  (e: 'toggle'): void
}>()
</script>

<template>
  <div class="section">
    <div class="section-hd">
      <span class="section-title">交易模式</span>
      <span class="section-hint">系统按「双向持仓 + 逐仓」设计；开关只控制新开仓自动逐仓；开启时若无持仓/挂单会立即切换当前标的池</span>
    </div>
    <div v-if="tradeMode" class="trade-mode">
      <div class="tm-row">
        <span class="tm-label">全局逐仓开单</span>
        <div
          class="switch"
          :class="{ on: isolatedOnly, disabled: isolatedBusy }"
          role="switch"
          :aria-checked="isolatedOnly"
          aria-label="全局逐仓开单"
          :title="isolatedOnly ? '关闭全局逐仓开单' : '开启全局逐仓开单'"
          tabindex="0"
          @click="emit('toggle')"
          @keydown.enter.prevent="emit('toggle')"
          @keydown.space.prevent="emit('toggle')"
        />
        <span class="muted tm-note">
          {{ isolatedOnly ? '开启' : '关闭' }} · 只控制新开仓；已有全仓仓位需先平仓
        </span>
      </div>
      <div class="tm-row">
        <span class="tm-label">持仓模式</span>
        <span class="tag" :class="tradeMode.hedge_mode ? 'tag-long' : 'tag-warn'">
          {{ tradeMode.hedge_mode ? '双向持仓' : '单向持仓 · 建议切换' }}
        </span>
        <span class="muted tm-note">
          {{ tradeMode.hedge_mode ? '多空可同时持有，符合系统设计' : '单向模式下仍可下单，但不会同时持有多空' }}
        </span>
      </div>
      <div class="tm-row">
        <span class="tm-label">保证金模式</span>
        <span class="tag" :class="tradeMode.need_isolated?.length ? 'tag-warn' : 'tag-long'">
          {{
            tradeMode.need_isolated?.length
              ? `${tradeMode.need_isolated.length} 个标的为全仓`
              : tradeMode.margin_known
                ? '当前池全部逐仓'
                : '逐仓状态未知'
          }}
        </span>
        <span class="muted tm-note">
          {{
            tradeMode.need_isolated?.length
              ? tradeMode.need_isolated.slice(0, 5).join('、') + (tradeMode.need_isolated.length > 5 ? '…' : '')
              : '逐仓隔离单标的风险'
          }}
        </span>
      </div>
      <div v-if="!tradeMode.can_switch" class="tag tag-warn">
        有持仓：开关只影响新开仓；立即切换需先平仓
      </div>
    </div>
    <div v-else class="empty" style="padding: 14px">
      {{ tradeModeError || '正在读取账户交易模式…' }}
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

/* ------------------------------------------------------ 交易模式卡片 */
.trade-mode {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 12px 14px;
  border: 1px solid var(--line);
  border-radius: var(--radius);
  background: var(--panel-2);
}
.tm-row {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.tm-label {
  width: 84px;
  flex: 0 0 84px;
  font-size: 11.5px;
  color: var(--text-dim);
}
.tm-note {
  font-size: 11px;
  line-height: 1.5;
}
.trade-mode .switch {
  flex: 0 0 38px;
}
.switch.disabled {
  opacity: 0.45;
  cursor: not-allowed;
}
.switch:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: 2px;
}
</style>