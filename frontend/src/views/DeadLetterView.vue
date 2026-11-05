<script setup lang="ts">
/**
 * 写失败（死信）面板: 查看异步写库最终失败的记录, 支持单条/批量重放与忽略.
 * 重放带幂等检查: 同 client_order_id 已落库时直接标记处理, 不会重复记账.
 */
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { http } from '../api/client'
import { useAppStore } from '../stores/app'
import { timeStr } from '../utils/format'

interface FailureRow {
  id: number
  ts: number
  kind: string
  error: string
  attempts: number
  payload: any
}

const app = useAppStore()
const items = ref<FailureRow[]>([])
const total = ref(0)
const loading = ref(false)
const busyId = ref(0)
const replayingAll = ref(false)

const KIND_LABEL: Record<string, string> = {
  _persist_entry: '开仓落库',
  _persist_close: '平仓落库',
}

const summary = computed(() => ({ pending: total.value, shown: items.value.length }))

/** 按类型分组（接口只回当前页，这里按页内数据统计） */
const byKind = computed(() => {
  const map = new Map<string, number>()
  for (const row of items.value) map.set(row.kind, (map.get(row.kind) || 0) + 1)
  return [...map.entries()].map(([kind, count]) => ({ kind, count })).sort((a, b) => b.count - a.count)
})

function cid(row: FailureRow): string {
  const args = row.payload?.args || []
  return args[0]?.client_order_id || ''
}

async function load() {
  loading.value = true
  try {
    const res = await http.get('/system/dead-letters?limit=50')
    items.value = res.items || []
    total.value = res.total || 0
  } catch (err: any) {
    app.notify(err?.message || '加载失败', 'error')
  } finally {
    loading.value = false
  }
}

async function replay(row: FailureRow) {
  busyId.value = row.id
  try {
    const res = await http.post(`/system/dead-letters/${row.id}/replay`)
    app.notify(res.status === 'skipped' ? '已幂等跳过（记录已落库）' : '重放成功', 'ok')
    await load()
  } catch (err: any) {
    app.notify(err?.message || '重放失败', 'error')
  } finally {
    busyId.value = 0
  }
}

async function ignore(row: FailureRow) {
  busyId.value = row.id
  try {
    await http.post(`/system/dead-letters/${row.id}/ignore`)
    app.notify('已标记为处理', 'ok')
    await load()
  } catch (err: any) {
    app.notify(err?.message || '操作失败', 'error')
  } finally {
    busyId.value = 0
  }
}

async function replayAll() {
  replayingAll.value = true
  try {
    const res = await http.post('/system/dead-letters/replay-all?limit=50')
    app.notify(
      `共 ${res.total} 条：重放 ${res.replayed}、跳过 ${res.skipped}、失败 ${res.failed}`,
      res.failed ? 'error' : 'ok'
    )
    await load()
  } catch (err: any) {
    app.notify(err?.message || '批量重放失败', 'error')
  } finally {
    replayingAll.value = false
  }
}

let timer: number | undefined
onMounted(async () => {
  await load()
  timer = window.setInterval(load, 20000)
})
onBeforeUnmount(() => window.clearInterval(timer))
</script>

<template>
  <div class="stack" style="gap: 12px">
    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">写失败（死信）</span>
        <div class="right">
          <span class="tag" :class="summary.pending ? 'tag-warn' : 'tag-long'">
            {{ summary.pending ? `待处理 ${summary.pending}` : '全部已处理' }}
          </span>
          <span v-for="row in byKind" :key="row.kind" class="tag tag-dim">
            {{ KIND_LABEL[row.kind] || row.kind }} · {{ row.count }}
          </span>
          <button class="btn btn-sm" :disabled="loading" @click="load">刷新</button>
          <button class="btn btn-sm btn-primary" :disabled="replayingAll || !summary.pending" @click="replayAll">
            {{ replayingAll ? '重放中…' : '全部重放' }}
          </button>
        </div>
      </div>
      <div class="panel-body">
        <div class="hint" style="margin-bottom: 10px">
          异步写库最终失败的记录会落在这里。重放前会校验 <code>client_order_id</code>：如果同号订单已落库，
          直接标记为已处理，不会重复记账。忽略仅做标记，不写库。
        </div>
        <div v-if="!items.length" class="empty" style="padding: 40px">
          {{ loading ? '加载中…' : '没有待处理的写失败记录' }}
        </div>
        <table v-else class="table">
          <thead>
            <tr>
              <th>时间</th>
              <th>类型</th>
              <th>client_order_id</th>
              <th>错误</th>
              <th>尝试</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in items" :key="row.id">
              <td class="mono muted">{{ timeStr(row.ts) }}</td>
              <td>{{ KIND_LABEL[row.kind] || row.kind }}</td>
              <td class="mono muted">{{ cid(row) || '—' }}</td>
              <td class="mono down" style="white-space: normal; max-width: 420px">{{ row.error }}</td>
              <td class="mono">{{ row.attempts }}</td>
              <td class="row" style="gap: 6px">
                <button class="btn btn-xs" :disabled="busyId === row.id" @click="replay(row)">重放</button>
                <button class="btn btn-xs" :disabled="busyId === row.id" @click="ignore(row)">忽略</button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>
  </div>
</template>

<style scoped>
.table {
  width: 100%;
  border-collapse: collapse;
  font-size: 11.5px;
}
.table th,
.table td {
  padding: 7px 10px;
  text-align: left;
  border-bottom: 1px solid rgba(148, 170, 200, 0.07);
  vertical-align: top;
}
.table th {
  color: var(--muted);
  font-size: 10px;
  font-weight: 500;
  white-space: nowrap;
}
.table tbody tr:hover {
  background: rgba(148, 170, 200, 0.04);
}
code {
  font-size: 11px;
  color: var(--muted);
}
</style>
