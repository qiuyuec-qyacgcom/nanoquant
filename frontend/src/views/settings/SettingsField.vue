<script setup lang="ts">
import { ref, watch } from 'vue'
import type { FieldRow } from './types'

const props = defineProps<{
  field: FieldRow
  modelValue: any
  fieldId: string
  focus: boolean
  changed: boolean
}>()

const emit = defineEmits<{
  (e: 'update:modelValue', value: any): void
}>()

const tagDraft = ref('')
const jsonText = ref('')
const jsonError = ref('')
const numberText = ref('')

function addTag() {
  const raw = tagDraft.value.trim()
  if (!raw) return
  const list: string[] = Array.isArray(props.modelValue) ? [...props.modelValue] : []
  for (const part of raw.split(/[\s,，]+/)) {
    if (part && !list.includes(part)) list.push(part)
  }
  emit('update:modelValue', list)
  tagDraft.value = ''
}

function removeTag(value: string) {
  emit(
    'update:modelValue',
    (props.modelValue || []).filter((v: string) => v !== value),
  )
}

function applyJson() {
  try {
    emit('update:modelValue', JSON.parse(jsonText.value || 'null'))
    jsonError.value = ''
  } catch {
    jsonError.value = 'JSON 格式有误，保存前请修正'
  }
}

watch(
  () => props.modelValue,
  (value) => {
    if (props.field.widget !== 'json') return
    let parsed: any
    try {
      parsed = JSON.parse(jsonText.value || 'null')
    } catch {
      parsed = undefined
    }
    // 自身编辑成功后的回写不再重排文本，保留用户输入原样
    if (JSON.stringify(parsed) === JSON.stringify(value)) return
    jsonText.value = JSON.stringify(value ?? null, null, 2)
    jsonError.value = ''
  },
  { immediate: true },
)

watch(
  () => props.modelValue,
  (value) => {
    if (props.field.widget !== 'number') return
    // 自己输入触发的回写不重排文本，保留 "05"、"5." 等中间输入，避免光标跳动。
    if (
      numberText.value !== '' &&
      value !== null &&
      value !== undefined &&
      Number(numberText.value) === Number(value)
    ) {
      return
    }
    numberText.value = value === null || value === undefined ? '' : String(value)
  },
  { immediate: true },
)

function onNumberInput(event: Event) {
  const raw = (event.target as HTMLInputElement).value
  numberText.value = raw
  // 清空/非法中间态只留在输入框里，不污染待保存草稿，避免提交空字符串。
  if (raw.trim() === '') return
  const num = Number(raw)
  if (Number.isFinite(num)) emit('update:modelValue', num)
}

function onNumberBlur() {
  const raw = numberText.value.trim()
  if (raw !== '' && Number.isFinite(Number(raw))) {
    numberText.value = String(Number(raw))
    return
  }
  numberText.value =
    props.modelValue === null || props.modelValue === undefined ? '' : String(props.modelValue)
}

function onSelectChange(event: Event) {
  emit('update:modelValue', (event.target as HTMLSelectElement).value)
}

function onTextInput(event: Event) {
  emit('update:modelValue', (event.target as HTMLInputElement).value)
}

/** 阶梯止盈：两列档位编辑器，保存时仍是 [[分钟, 保证金收益率%], ...] */
function roiRows(): number[][] {
  const raw = props.modelValue
  return Array.isArray(raw) ? raw.map((r: any) => [Number(r?.[0] ?? 0), Number(r?.[1] ?? 0)]) : []
}

function setRoiCell(idx: number, col: 0 | 1, event: Event) {
  const rows = roiRows()
  if (!rows[idx]) return
  const num = Number((event.target as HTMLInputElement).value)
  rows[idx][col] = Number.isFinite(num) ? num : 0
  emit('update:modelValue', rows)
}

function addRoiTier() {
  const rows = roiRows()
  if (rows.length >= 12) return
  const last = rows[rows.length - 1]
  rows.push([
    last ? Math.min(2880, Number(last[0]) + 15) : 0,
    last ? Number(Math.max(0.05, Number(last[1]) * 0.75).toFixed(2)) : 0.8,
  ])
  emit('update:modelValue', rows)
}

function removeRoiTier(idx: number) {
  const rows = roiRows()
  rows.splice(idx, 1)
  emit('update:modelValue', rows)
}

function clearRoiTiers() {
  emit('update:modelValue', [])
}

const MONO_KEYS = new Set(['base_url', 'proxy', 'rsshub_base'])
</script>

<template>
  <div :id="fieldId" class="field-row" :class="{ focus, changed }">
    <div class="field-meta">
      <div class="field-name" :title="field.key">
        {{ field.label }}
        <span v-if="field.unit" class="field-unit">{{ field.unit }}</span>
      </div>
      <div v-if="field.hint" class="field-hint">{{ field.hint }}</div>
    </div>

    <div class="field-ctrl">
      <div v-if="field.widget === 'switch'" class="row" style="gap: 10px">
        <div class="switch" :class="{ on: modelValue }" @click="emit('update:modelValue', !modelValue)" />
        <span class="muted" style="font-size: 11.5px">{{ modelValue ? '开启' : '关闭' }}</span>
      </div>

      <div v-else-if="field.widget === 'seg'" class="seg" role="group">
        <button
          v-for="opt in field.options || []"
          :key="opt[0]"
          type="button"
          class="seg-btn"
          :class="{ on: modelValue === opt[0] }"
          @click="emit('update:modelValue', opt[0])"
        >
          {{ opt[1] }}
        </button>
      </div>

      <select v-else-if="field.widget === 'select'" :value="modelValue" class="input" @change="onSelectChange">
        <option v-for="opt in field.options || []" :key="opt[0]" :value="opt[0]">{{ opt[1] }}</option>
      </select>

      <div v-else-if="field.widget === 'number'" class="num-wrap">
        <input
          :value="numberText"
          type="number"
          :step="field.step ?? 'any'"
          :min="field.min"
          :max="field.max"
          class="input mono"
          @input="onNumberInput"
          @blur="onNumberBlur"
        />
        <span v-if="field.unit" class="num-unit">{{ field.unit }}</span>
      </div>

      <div v-else-if="field.widget === 'tags'" class="tags-box">
        <span v-for="v in modelValue" :key="v" class="chip">
          {{ v }}<b class="chip-x" @click="removeTag(v)">×</b>
        </span>
        <input
          v-model="tagDraft"
          class="chip-input"
          :placeholder="modelValue?.length ? '继续添加…' : '输入后回车'"
          @keyup.enter="addTag"
          @blur="addTag"
        />
      </div>

      <div v-else-if="field.widget === 'json'" class="stack" style="gap: 4px">
        <textarea
          class="input mono"
          rows="3"
          :value="jsonText"
          @input="jsonText = ($event.target as HTMLTextAreaElement).value"
          @change="applyJson"
        />
        <span v-if="jsonError" class="down" style="font-size: 11px">{{ jsonError }}</span>
      </div>

      <div v-else-if="field.widget === 'roi'" class="roi-editor">
        <div class="roi-head">
          <span>持仓分钟</span>
          <span>盈利达 % 自动平仓</span>
          <span></span>
        </div>
        <div v-for="(tier, idx) in modelValue || []" :key="idx" class="roi-row">
          <input
            type="number" min="0" max="2880" step="1" class="input mono"
            :value="tier[0]"
            @input="setRoiCell(idx, 0, $event)"
          />
          <input
            type="number" min="0" max="50" step="0.05" class="input mono"
            :value="tier[1]"
            @input="setRoiCell(idx, 1, $event)"
          />
          <button class="btn btn-xs btn-danger" title="删除该档" @click="removeRoiTier(idx)">✕</button>
        </div>
        <div v-if="!(modelValue || []).length" class="hint">没有档位 = 关闭按盈利百分比自动平仓</div>
        <div class="row" style="gap: 8px; margin-top: 6px; flex-wrap: wrap">
          <button class="btn btn-xs" :disabled="(modelValue || []).length >= 12" @click="addRoiTier">
            + 添加一档
          </button>
          <button class="btn btn-xs btn-ghost" :disabled="!(modelValue || []).length" @click="clearRoiTiers">
            清空（关闭）
          </button>
          <span class="hint">最多 12 档；保存后自动按分钟升序生效</span>
        </div>
      </div>

      <input
        v-else
        :value="modelValue ?? ''"
        class="input"
        :class="{ mono: MONO_KEYS.has(field.key) }"
        @input="onTextInput"
      />
    </div>
  </div>
</template>

<style scoped>
.field-row {
  display: grid;
  grid-template-columns: minmax(0, 1.05fr) minmax(180px, 0.95fr);
  gap: 14px;
  align-items: center;
  padding: 9px 0;
  border-top: 1px dashed var(--line);
}

/* 改过但还没保存的字段：左边一条黄线 + "已改"角标，滚动时也能一眼看到 */
.field-row.changed {
  box-shadow: inset 2px 0 0 var(--warn);
  padding-left: 8px;
}
.field-row.changed .field-name::after {
  content: '已改';
  margin-left: 6px;
  padding: 0 3px;
  font-size: 9px;
  font-weight: 400;
  color: var(--warn);
  border: 1px solid rgba(240, 169, 42, 0.35);
  border-radius: 4px;
  vertical-align: middle;
}
.field-row:first-of-type {
  border-top: none;
}
.field-meta {
  min-width: 0;
}
.field-name {
  display: flex;
  align-items: baseline;
  gap: 6px;
  font-size: 12.5px;
  color: var(--text);
}
.field-unit {
  font-size: 10px;
  color: var(--muted);
  font-family: var(--mono);
}
.field-hint {
  margin-top: 2px;
  font-size: 10.5px;
  color: var(--muted);
  line-height: 1.55;
}
.field-ctrl {
  min-width: 0;
}
.num-wrap {
  display: flex;
  align-items: center;
  gap: 8px;
}
.num-unit {
  font-size: 10.5px;
  color: var(--muted);
  font-family: var(--mono);
  white-space: nowrap;
}
.seg {
  display: inline-flex;
  padding: 3px;
  gap: 3px;
  background: var(--surface-input);
  border: 1px solid var(--line);
  border-radius: 8px;
  flex-wrap: wrap;
}
.seg-btn {
  padding: 5px 11px;
  border: 1px solid transparent;
  border-radius: 6px;
  background: transparent;
  color: var(--text-dim);
  font-size: 11.5px;
  cursor: pointer;
  font-family: var(--sans);
  transition: all 0.15s ease;
}
.seg-btn:hover {
  color: var(--text);
}
.seg-btn.on {
  background: rgba(59, 130, 246, 0.16);
  border-color: rgba(59, 130, 246, 0.4);
  color: #dbe9ff;
}
.tags-box {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
  padding: 6px 8px;
  min-height: 38px;
  background: var(--surface-input);
  border: 1px solid var(--line);
  border-radius: 7px;
}
.chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 2px 7px;
  border-radius: 5px;
  background: rgba(59, 130, 246, 0.12);
  border: 1px solid rgba(59, 130, 246, 0.28);
  color: #b9d1ff;
  font-size: 11px;
  font-family: var(--mono);
}
.chip-x {
  cursor: pointer;
  color: var(--muted);
  font-weight: 400;
}
.chip-x:hover {
  color: var(--down);
}
.chip-input {
  flex: 1;
  min-width: 90px;
  border: none;
  background: transparent;
  color: var(--text);
  font-size: 11.5px;
  outline: none;
  font-family: var(--mono);
}
@media (max-width: 980px) {
  .field-row {
    grid-template-columns: 1fr;
    gap: 8px;
  }
}

/* ------------------------------------------------ 阶梯止盈档位编辑器 */
.roi-editor {
  display: flex;
  flex-direction: column;
  gap: 6px;
  width: 100%;
}
.roi-head,
.roi-row {
  display: grid;
  grid-template-columns: minmax(88px, 1fr) minmax(110px, 1fr) 28px;
  gap: 8px;
  align-items: center;
}
.roi-head {
  font-size: 10.5px;
  color: var(--muted);
}
.roi-row .input {
  padding: 5px 8px;
}
@media (max-width: 560px) {
  .roi-head,
  .roi-row {
    grid-template-columns: minmax(70px, 1fr) minmax(88px, 1fr) 26px;
    gap: 5px;
  }
}

/* 搜索跳转后闪一下，解决"点完不知道跳到哪了" */
.field-row.focus {
  animation: field-flash 2s ease;
}
@keyframes field-flash {
  0%,
  55% {
    background: rgba(59, 130, 246, 0.16);
    box-shadow: inset 2px 0 0 var(--accent);
  }
  100% {
    background: transparent;
    box-shadow: inset 2px 0 0 transparent;
  }
}
</style>