<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { getToken, http } from '../api/client'
import { useAppStore } from '../stores/app'
import { fmt, timeStr } from '../utils/format'

const app = useAppStore()
const options = ref<any>(null)
const jobs = ref<any[]>([])
const usage = ref<any>(null)
const busy = ref(false)
const name = ref('')
const encrypt = ref(true)
const selectedGroups = ref<string[]>([])
const selectedTables = ref<string[]>([])
const selectedFiles = ref<string[]>([])

const sizeText = computed(() => {
  const bytes = usage.value?.bytes || 0
  if (bytes > 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(2)} MB`
  return `${(bytes / 1024).toFixed(1)} KB`
})

const GROUP_LABELS: Record<string, string> = {
  engine: '引擎', risk: '风控', strategy: '策略', ai: 'AI', data: '数据源',
  news: '资讯', evolution: '自进化', backup: '备份', security: '安全', system: '系统',
}

async function load() {
  try {
    const [opt, job] = await Promise.all([http.get('/backup/options'), http.get('/backup/jobs')])
    options.value = opt
    jobs.value = job.items
    usage.value = job.usage
    if (!selectedGroups.value.length) selectedGroups.value = [...opt.settings_groups]
    if (!selectedTables.value.length) selectedTables.value = [...opt.default_tables]
    if (encrypt.value === undefined) encrypt.value = true
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

function toggle(list: string[], value: string) {
  const idx = list.indexOf(value)
  if (idx >= 0) list.splice(idx, 1)
  else list.push(value)
}

async function create() {
  busy.value = true
  try {
    const res = await http.post('/backup/create', {
      name: name.value,
      settings_groups: selectedGroups.value,
      tables: selectedTables.value,
      files: selectedFiles.value,
      encrypt: encrypt.value,
    })
    app.notify(`备份完成 (${(res.size / 1024).toFixed(1)} KB)`, 'ok')
    name.value = ''
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

async function restore(job: any) {
  if (!confirm(`确认从备份 #${job.id} 还原? 当前数据会被覆盖, 系统会自动先做一次还原前备份。`)) return
  busy.value = true
  try {
    const res = await http.post(`/backup/${job.id}/restore`, { restore_settings: true, restore_tables: true })
    app.notify(`还原完成: ${Object.keys(res.restored || {}).length} 项`, 'ok')
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    busy.value = false
  }
}

async function remove(job: any) {
  if (!confirm(`删除备份 #${job.id}?`)) return
  try {
    await http.del(`/backup/${job.id}`)
    await load()
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

async function download(job: any) {
  try {
    const res = await fetch(`/api/v1/backup/${job.id}/download`, {
      headers: { Authorization: `Bearer ${getToken()}` },
    })
    if (!res.ok) throw new Error(`HTTP ${res.status}`)
    const blob = await res.blob()
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = `nanoquant-backup-${job.id}.zip${job.encrypted ? '.enc' : ''}`
    a.click()
    URL.revokeObjectURL(url)
  } catch (err: any) {
    app.notify(`下载失败: ${err.message}`, 'error')
  }
}

onMounted(load)
</script>

<template>
  <div class="stack">
    <div class="kpi-grid">
      <div class="kpi">
        <div class="kpi-label">备份文件</div>
        <div class="kpi-value">{{ usage?.files ?? 0 }}</div>
        <div class="kpi-foot">占用 {{ sizeText }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">保留策略</div>
        <div class="kpi-value">{{ app.settings.backup?.retention ?? 10 }}</div>
        <div class="kpi-foot">超出后自动清理最旧备份</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">默认加密</div>
        <div class="kpi-value">{{ app.settings.backup?.encrypt ? '开启' : '关闭' }}</div>
        <div class="kpi-foot">Fernet 对称加密, 主密钥在本机 data/.master_key</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">每日自动备份</div>
        <div class="kpi-value" :style="{ color: options?.auto?.enabled ? 'var(--up)' : 'var(--muted)' }">
          {{ options?.auto?.enabled ? '开启' : '关闭' }}
        </div>
        <div class="kpi-foot">
          <template v-if="!options?.auto?.enabled">到「参数配置 → 备份策略」打开</template>
          <template v-else-if="!options?.auto?.last_run_ts">尚未执行，下一轮检查（≤15 分钟）自动跑</template>
          <template v-else>上次 {{ timeStr(options.auto.last_run_ts, false) }}</template>
        </div>
      </div>
      <div class="kpi">
        <div class="kpi-label">可用空间</div>
        <div class="kpi-value">{{ ((usage?.free_bytes || 0) / 1024 ** 3).toFixed(1) }} GB</div>
        <div class="kpi-foot">数据盘剩余</div>
      </div>
    </div>

    <div class="grid g2">
      <div class="panel">
        <div class="panel-hd"><span class="panel-title">新建备份</span></div>
        <div class="panel-body stack" style="gap: 12px">
          <div class="field"><span class="label">备份名称 (可选)</span><input v-model="name" class="input" placeholder="留空则用时间戳" /></div>

          <div class="field">
            <span class="label">配置分组 <span class="muted">({{ selectedGroups.length }}/{{ options?.settings_groups?.length || 0 }})</span></span>
            <div class="row row-wrap" style="gap: 5px">
              <button
                v-for="g in options?.settings_groups || []"
                :key="g"
                class="btn btn-xs"
                :class="{ 'btn-primary': selectedGroups.includes(g) }"
                @click="toggle(selectedGroups, g)"
              >
                {{ GROUP_LABELS[g] || g }}
              </button>
            </div>
          </div>

          <div class="field">
            <span class="label">数据表 <span class="muted">({{ selectedTables.length }})</span></span>
            <div class="row row-wrap" style="gap: 5px">
              <button
                v-for="t in options?.tables || []"
                :key="t"
                class="btn btn-xs"
                :class="{ 'btn-primary': selectedTables.includes(t) }"
                @click="toggle(selectedTables, t)"
              >
                {{ t }}
              </button>
            </div>
          </div>

          <div class="field" v-if="options?.files?.length">
            <span class="label">附加文件 <span class="muted">(可选)</span></span>
            <div class="scroll-y" style="max-height: 110px; border: 1px solid var(--line); border-radius: 7px; padding: 6px">
              <div v-for="f in options.files" :key="f" class="row" style="padding: 2px 0; font-size: 11px">
                <div class="switch" :class="{ on: selectedFiles.includes(f) }" style="transform: scale(0.7)" @click="toggle(selectedFiles, f)" />
                <span class="mono muted ellipsis">{{ f }}</span>
              </div>
            </div>
          </div>

          <div class="row">
            <div class="switch" :class="{ on: encrypt }" @click="encrypt = !encrypt" />
            <span class="label">加密备份</span>
            <div class="spacer" />
            <button class="btn btn-primary" :disabled="busy" @click="create">{{ busy ? '处理中…' : '立即备份' }}</button>
          </div>
          <div class="hint">备份范围可选到任意配置分组与数据表, 还原前会自动创建 pre-restore 快照。</div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">备份记录</span>
          <div class="right"><button class="btn btn-sm" @click="load">刷新</button></div>
        </div>
        <div class="panel-body flush table-scroll" style="max-height: 460px">
          <table class="table">
            <thead><tr><th>#</th><th>时间</th><th>名称</th><th>大小</th><th>校验</th><th>操作</th></tr></thead>
            <tbody>
              <tr v-for="j in jobs" :key="j.id">
                <td class="mono">{{ j.id }}</td>
                <td class="mono muted">{{ timeStr(j.ts, false) }}</td>
                <td>
                  {{ j.name }}
                  <span v-if="j.encrypted" class="tag tag-blue" style="margin-left: 4px">加密</span>
                </td>
                <td class="mono">{{ (j.size / 1024).toFixed(1) }} KB</td>
                <td>
                  <span v-if="!j.exists" class="tag tag-warn">文件丢失</span>
                  <span v-else-if="j.sha256_ok" class="tag tag-long">校验通过</span>
                  <span v-else class="tag tag-short">校验失败</span>
                </td>
                <td>
                  <div class="row" style="gap: 4px">
                    <button class="btn btn-xs" :disabled="!j.exists" @click="download(j)">下载</button>
                    <button class="btn btn-xs" :disabled="busy || !j.exists" @click="restore(j)">还原</button>
                    <button class="btn btn-xs btn-danger" @click="remove(j)">删</button>
                  </div>
                </td>
              </tr>
              <tr v-if="!jobs.length"><td colspan="6" class="empty">暂无备份</td></tr>
            </tbody>
          </table>
        </div>
        <div class="panel-body" style="padding-top: 0">
          <span class="hint">已完成 {{ jobs.length }} 次备份 · 文件位于 data/backups · 单文件 SHA256 校验，防止静默损坏</span>
        </div>
      </div>
    </div>
  </div>
</template>
