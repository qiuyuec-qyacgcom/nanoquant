<script setup lang="ts">
import { computed, onMounted, reactive, ref } from 'vue'
import { http } from '../api/client'
import EChart from '../components/EChart.vue'
import Modal from '../components/Modal.vue'
import { useAppStore } from '../stores/app'
import type { AiProvider } from '../types'
import { timeStr } from '../utils/format'

interface AiCallLogLike { id: number; ts: number; provider: string; model: string; purpose: string; ok: boolean; latency_ms: number; prompt_tokens: number; completion_tokens: number; cost_usd: number; error: string }

const app = useAppStore()
const providers = ref<AiProvider[]>([])
const active = ref<any>({})
const usage = ref<any>(null)
const logs = ref<AiCallLogLike[]>([])
const showForm = ref(false)
const editing = ref<AiProvider | null>(null)
const diagnoseResult = ref<any>(null)
const diagnosing = ref(false)
const testResult = ref<any>(null)
const probing = ref(false)
const probe = ref<{ ok: boolean; endpoint: string; error?: string; models: { id: string; name: string; context?: number }[] } | null>(null)
const probeShown = computed(() => (probe.value?.models || []).slice(0, 40))
const testingId = ref(0)

const aiCfg = reactive<any>({
  enabled: true,
  mode: 'single',
  provider_id: 0,
  fallback_provider_ids: [] as number[],
  min_confidence: 0.45,
  require_agreement: true,
  allow_ai_override: false,
  max_calls_per_hour: 30,
  monthly_budget_usd: 3,
  timeout_sec: 45,
  temperature: 0.2,
  max_position_leverage: 20,
  inject_news: true,
  inject_mantras: true,
  personas: [] as any[],
  decision_schema_version: 2,
})

/** 决策演练：用真实行情跑一次 AI，不下单；对照原始输出与后台中文转译 */
const previewSymbol = ref('BTCUSDT')
const previewResult = ref<any>(null)
const previewBusy = ref(false)

async function runPreview() {
  previewBusy.value = true
  previewResult.value = null
  try {
    previewResult.value = await http.post('/ai/decision-preview', { symbol: previewSymbol.value.trim().toUpperCase() })
  } catch (err: any) {
    previewResult.value = { ok: false, error: err?.message || '演练失败' }
  } finally {
    previewBusy.value = false
  }
}

const form = reactive<any>({
  name: '',
  protocol: 'openai_chat',
  base_url: 'https://api.deepseek.com/v1',
  api_key: '',
  model: 'deepseek-chat',
  temperature: 0.2,
  max_tokens: 3000,
  timeout_sec: 45,
  json_mode: true,
  enabled: true,
  priority: 100,
  role: 'primary',
})

const presets = [
  { label: 'DeepSeek', base: 'https://api.deepseek.com/v1', model: 'deepseek-chat' },
  { label: 'OpenAI', base: 'https://api.openai.com/v1', model: 'gpt-4o-mini' },
  { label: '通义千问', base: 'https://dashscope.aliyuncs.com/compatible-mode/v1', model: 'qwen-plus' },
  { label: '智谱 GLM', base: 'https://open.bigmodel.cn/api/paas/v4', model: 'glm-4-flash' },
  { label: 'Kimi', base: 'https://api.moonshot.cn/v1', model: 'moonshot-v1-8k' },
  { label: '本地 vLLM', base: 'http://127.0.0.1:8000/v1', model: 'local-model' },
]

const usageOption = computed(() => {
  const rows = usage.value?.by_day || []
  return {
    grid: { left: 44, right: 16, top: 20, bottom: 26 },
    tooltip: { trigger: 'axis', backgroundColor: '#0d1727', borderColor: '#1f3557', textStyle: { color: '#dbe6f5', fontSize: 11 } },
    xAxis: {
      type: 'category',
      data: rows.map((r: any) => r.date.slice(5)),
      axisLine: { lineStyle: { color: '#1a2942' } },
      axisLabel: { color: '#6b7c99', fontSize: 10 },
    },
    yAxis: [
      { type: 'value', splitLine: { lineStyle: { color: 'rgba(26,41,66,0.5)' } }, axisLabel: { color: '#6b7c99', fontSize: 10 } },
    ],
    series: [
      {
        name: '调用次数',
        type: 'bar',
        barMaxWidth: 18,
        data: rows.map((r: any) => r.calls),
        itemStyle: { color: '#3b82f6', borderRadius: [4, 4, 0, 0] },
      },
      {
        name: '失败',
        type: 'bar',
        barMaxWidth: 18,
        data: rows.map((r: any) => r.calls - r.ok),
        itemStyle: { color: '#f04b5a', borderRadius: [4, 4, 0, 0] },
      },
    ],
  }
})

const budgetPct = computed(() => {
  const used = usage.value?.budget?.used_usd || 0
  const total = usage.value?.budget?.monthly_usd || 1
  return Math.min(100, (used / total) * 100)
})

async function loadAll() {
  const [p, u, l] = await Promise.allSettled([http.get('/ai/providers'), http.get('/ai/usage'), http.get('/ai/logs?limit=40')])
  if (p.status === 'fulfilled') {
    providers.value = p.value.items
    active.value = p.value.active
    if (!aiCfg.provider_id && p.value.active?.provider_id) aiCfg.provider_id = p.value.active.provider_id
  }
  if (u.status === 'fulfilled') usage.value = u.value
  if (l.status === 'fulfilled') logs.value = l.value.items
  if (!Object.keys(app.settings).length) await app.loadSettings()
  const cfg = app.settings.ai
  if (cfg) Object.assign(aiCfg, cfg)
}

function openCreate() {
  editing.value = null
  Object.assign(form, {
    name: '', protocol: 'openai_chat', base_url: 'https://api.deepseek.com/v1', api_key: '',
    model: 'deepseek-chat', temperature: 0.2, max_tokens: 3000, timeout_sec: 45,
    json_mode: true, enabled: true, priority: 100, role: 'primary',
  })
  probe.value = null
  showForm.value = true
}

function openEdit(p: AiProvider) {
  editing.value = p
  Object.assign(form, {
    name: p.name, protocol: p.protocol, base_url: p.base_url, api_key: '',
    model: p.model, temperature: p.temperature, max_tokens: p.max_tokens,
    timeout_sec: p.timeout_sec, json_mode: p.json_mode, enabled: p.enabled,
    priority: p.priority, role: p.role,
  })
  probe.value = null
  showForm.value = true
}

function applyPreset(p: { base: string; model: string }) {
  form.base_url = p.base
  form.model = p.model
}

async function save() {
  try {
    if (!form.name || !form.base_url || !form.model) {
      app.notify('名称 / 接口地址 / 模型名 必填', 'error')
      return
    }
    if (editing.value) {
      const patch = { ...form }
      if (!patch.api_key) delete patch.api_key
      await http.patch(`/ai/providers/${editing.value.id}`, patch)
    } else {
      await http.post('/ai/providers', form)
    }
    showForm.value = false
    app.notify('已保存', 'ok')
    await loadAll()
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

async function removeProvider(p: AiProvider) {
  if (!confirm(`删除模型 ${p.name}?`)) return
  try {
    await http.del(`/ai/providers/${p.id}`)
    app.notify('已删除', 'ok')
    await loadAll()
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

async function toggleProvider(p: AiProvider) {
  try {
    await http.patch(`/ai/providers/${p.id}`, { enabled: !p.enabled })
    await loadAll()
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

/** 已保存供应商: 探测端点并挑一个模型直接切换 */
const modelPicker = ref<{ provider: AiProvider; ok: boolean; endpoint: string; error?: string; models: any[] } | null>(null)
const picking = ref(0)

async function probeProvider(p: AiProvider) {
  picking.value = p.id
  try {
    const res: any = await http.get(`/ai/providers/${p.id}/models`)
    modelPicker.value = { provider: p, ok: res.ok, endpoint: res.endpoint || '', error: res.error, models: res.models || [] }
    if (!res.ok) app.notify(res.error || '探测失败', 'error')
  } catch (err: any) {
    app.notify(err?.message || '探测失败', 'error')
  } finally {
    picking.value = 0
  }
}

async function pickModel(m: any) {
  if (!modelPicker.value) return
  const provider = modelPicker.value.provider
  try {
    await http.patch(`/ai/providers/${provider.id}`, { model: m.id })
    modelPicker.value = null
    app.notify(`已切换到 ${m.id}`, 'ok')
    await loadAll()
  } catch (err: any) {
    app.notify(err?.message || '切换失败', 'error')
  }
}

/** 探测端点: 自动列出可用模型, 点一下即可填入模型名 */
async function probeModels() {
  if (!form.base_url) {
    app.notify('先填接口地址再探测', 'error')
    return
  }
  probing.value = true
  probe.value = null
  try {
    const res: any = await http.post('/ai/providers/fetch-models', {
      base_url: form.base_url,
      api_key: form.api_key,
      protocol: form.protocol,
      provider_id: editing.value?.id || 0,
    })
    probe.value = res
    if (res.ok) {
      app.notify(`探测到 ${res.models.length} 个模型`, 'ok')
      if (!form.model && res.models.length) form.model = res.models[0].id
    } else {
      app.notify(res.error || '探测失败', 'error')
    }
  } catch (err: any) {
    app.notify(err?.message || '探测失败', 'error')
  } finally {
    probing.value = false
  }
}

async function testProvider(p: AiProvider) {
  testingId.value = p.id
  testResult.value = null
  try {
    testResult.value = await http.post(`/ai/providers/${p.id}/test`, { prompt: '用一句话说明你是哪个模型' })
  } catch (err: any) {
    testResult.value = { ok: false, error: err.message }
  } finally {
    testingId.value = 0
  }
}

async function diagnose(p: AiProvider) {
  diagnosing.value = true
  diagnoseResult.value = { provider: p.name, checks: [] }
  try {
    diagnoseResult.value = await http.get(`/ai/providers/${p.id}/diagnose?deep=true`)
  } catch (err: any) {
    diagnoseResult.value = { ok: false, checks: [{ name: '诊断失败', ok: false, detail: err.message, latency_ms: 0 }] }
  } finally {
    diagnosing.value = false
  }
}

async function saveAiConfig() {
  try {
    const patch = { ...aiCfg }
    await app.saveGroup('ai', patch)
    app.notify('AI 决策配置已保存', 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

function addPersona() {
  aiCfg.personas.push({ name: '新席位', weight: 0.3, focus: '', enabled: true })
}

onMounted(loadAll)
</script>

<template>
  <div class="stack">
    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">AI 决策配置</span>
        <div class="right">
          <div class="row" style="gap: 6px">
            <span class="muted" style="font-size: 11px">启用</span>
            <div class="switch" :class="{ on: aiCfg.enabled }" @click="aiCfg.enabled = !aiCfg.enabled" />
          </div>
          <button class="btn btn-sm btn-primary" @click="saveAiConfig">保存</button>
        </div>
      </div>
      <div class="panel-body stack">
        <div class="grid g4" style="gap: 12px">
          <div class="field">
            <span class="label">决策模式</span>
            <select v-model="aiCfg.mode" class="select">
              <option value="single">单模型决策</option>
              <option value="committee">多席位议会</option>
            </select>
          </div>
          <div class="field">
            <span class="label">主模型</span>
            <select v-model.number="aiCfg.provider_id" class="select">
              <option :value="0">未选择</option>
              <option v-for="p in providers" :key="p.id" :value="p.id">{{ p.name }} · {{ p.model }}</option>
            </select>
          </div>
          <div class="field">
            <span class="label">最低置信度</span>
            <input v-model.number="aiCfg.min_confidence" type="number" step="0.05" min="0" max="1" class="input mono" />
          </div>
          <div class="field">
            <span class="label">每小时调用上限</span>
            <input v-model.number="aiCfg.max_calls_per_hour" type="number" min="0" max="600" class="input mono" />
          </div>
          <div class="field">
            <span class="label">月度预算 (USD)</span>
            <input v-model.number="aiCfg.monthly_budget_usd" type="number" step="0.5" min="0" class="input mono" />
          </div>
          <div class="field">
            <span class="label">单次超时 (秒)</span>
            <input v-model.number="aiCfg.timeout_sec" type="number" min="5" max="300" class="input mono" />
          </div>
          <div class="field">
            <span class="label">温度</span>
            <input v-model.number="aiCfg.temperature" type="number" step="0.1" min="0" max="2" class="input mono" />
          </div>
          <div class="field">
            <span class="label">AI 可设最大杠杆</span>
            <input v-model.number="aiCfg.max_position_leverage" type="number" min="1" max="100" class="input mono" />
          </div>
          <div class="field">
            <span class="label">决策协议</span>
            <select v-model.number="aiCfg.decision_schema_version" class="select">
              <option :value="2">v2 紧凑英文 · 后台转中文（省 token）</option>
              <option :value="1">v1 中文详细 JSON（兼容旧模型）</option>
            </select>
          </div>
        </div>
        <div class="hint">
          v2 用英文短键进、动作码/枚举码出，后台按码表转译中文再落库；输入约省 50%、输出约省 70%。
          解析时按返回内容自动探测，自定义模型仍能回 v1 格式。
        </div>

        <div class="row row-wrap" style="gap: 18px">
          <div class="row" style="gap: 6px">
            <div class="switch" :class="{ on: aiCfg.require_agreement }" @click="aiCfg.require_agreement = !aiCfg.require_agreement" />
            <span class="label">AI 必须与量化信号同向</span>
          </div>
          <div class="row" style="gap: 6px">
            <div class="switch" :class="{ on: aiCfg.allow_ai_override }" @click="aiCfg.allow_ai_override = !aiCfg.allow_ai_override" />
            <span class="label">允许 AI 反向覆盖信号</span>
          </div>
          <div class="row" style="gap: 6px">
            <div class="switch" :class="{ on: aiCfg.inject_mantras }" @click="aiCfg.inject_mantras = !aiCfg.inject_mantras" />
            <span class="label">注入分析心法</span>
          </div>
          <div class="row" style="gap: 6px">
            <div class="switch" :class="{ on: aiCfg.inject_news }" @click="aiCfg.inject_news = !aiCfg.inject_news" />
            <span class="label">注入资讯情绪</span>
          </div>
        </div>

        <div v-if="aiCfg.mode === 'committee'" class="stack" style="gap: 8px">
          <div class="row">
            <span class="panel-title" style="font-size: 12px">议会席位</span>
            <div class="spacer" />
            <button class="btn btn-xs" @click="addPersona">+ 添加席位</button>
          </div>
          <div v-for="(p, idx) in aiCfg.personas" :key="idx" class="row" style="gap: 8px">
            <input v-model="p.name" class="input" style="width: 130px" placeholder="席位名" />
            <input v-model="p.focus" class="input" style="flex: 1" placeholder="关注点" />
            <input v-model.number="p.weight" type="number" step="0.05" min="0" max="5" class="input mono" style="width: 80px" />
            <div class="switch" :class="{ on: p.enabled }" @click="p.enabled = !p.enabled" />
            <button class="btn btn-xs btn-danger" @click="aiCfg.personas.splice(idx, 1)">删</button>
          </div>
          <div class="hint">权重决定票数占比; 多数席位一致才会给出开仓建议, 分歧时按 hold 处理。</div>
        </div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">决策演练 · 原文 / 中文对照</span>
        <div class="right" style="gap: 8px">
          <input
            v-model="previewSymbol"
            class="input mono"
            style="width: 130px; padding: 4px 8px"
            placeholder="标的，如 BTCUSDT"
            @keyup.enter="runPreview"
          />
          <button class="btn btn-sm" :disabled="previewBusy" @click="runPreview">
            {{ previewBusy ? '运行中…' : '运行演练' }}
          </button>
        </div>
      </div>
      <div class="panel-body stack" style="gap: 10px">
        <div v-if="!previewResult" class="empty" style="padding: 20px">
          用真实行情跑一次 AI 决策，不触发下单；可核对协议版本、token 估算，以及原始输出与后台中文译文。
        </div>
        <template v-else>
          <div class="row" style="gap: 8px; flex-wrap: wrap">
            <span class="tag" :class="previewResult.ok ? 'tag-long' : 'tag-short'">
              {{ previewResult.ok ? '解析成功' : '失败' }}
            </span>
            <span class="tag tag-dim">协议 v{{ previewResult.schema_version ?? '—' }}</span>
            <span class="muted" style="font-size: 11px">
              输入 ≈ {{ previewResult.prompt_tokens_est ?? '—' }} tokens ·
              {{ previewResult.action || '—' }} · conf {{ previewResult.confidence ?? '—' }} ·
              {{ previewResult.latency_ms ?? '—' }}ms
            </span>
          </div>
          <div class="grid g2">
            <div class="stack" style="gap: 6px">
              <span class="label">模型原始输出（v2 为紧凑英文 JSON）</span>
              <pre class="code mono" style="margin: 0; max-height: 200px; overflow: auto; white-space: pre-wrap; word-break: break-all">{{ previewResult.raw_text || previewResult.error || '—' }}</pre>
            </div>
            <div class="stack" style="gap: 6px">
              <span class="label">后台转译后（面板 / 审计看到的中文）</span>
              <pre class="code" style="margin: 0; max-height: 200px; overflow: auto; white-space: pre-wrap">{{ previewResult.reason || '—' }}</pre>
              <div v-if="previewResult.risk_flags?.length" class="muted" style="font-size: 11px">
                风险：{{ previewResult.risk_flags.join('；') }}
              </div>
              <div v-if="previewResult.invalidation" class="muted" style="font-size: 11px">
                证伪：{{ previewResult.invalidation }}
              </div>
            </div>
          </div>
          <details v-if="previewResult.prompt_preview">
            <summary class="muted" style="font-size: 11px; cursor: pointer">查看送入模型的上下文（截断 1200 字）</summary>
            <pre class="code mono" style="max-height: 180px; overflow: auto; white-space: pre-wrap; word-break: break-all">{{ previewResult.prompt_preview }}</pre>
          </details>
        </template>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">模型接入</span>
        <div class="right">
          <span class="tag tag-dim">{{ providers.length }} 个配置</span>
          <button class="btn btn-sm btn-primary" @click="openCreate">+ 新增模型</button>
        </div>
      </div>
      <div class="panel-body flush">
        <table class="table">
          <thead>
            <tr><th>名称</th><th>模型</th><th>协议</th><th>接口地址</th><th>Key</th><th>优先级</th><th>状态</th><th>操作</th></tr>
          </thead>
          <tbody>
            <tr v-for="p in providers" :key="p.id">
              <td><b>{{ p.name }}</b></td>
              <td class="mono">{{ p.model }}</td>
              <td class="muted">{{ p.protocol }}</td>
              <td class="mono muted ellipsis" style="max-width: 260px" :title="p.base_url">{{ p.base_url }}</td>
              <td class="mono muted">{{ p.has_key ? p.api_key_masked : '未配置' }}</td>
              <td class="mono">{{ p.priority }}</td>
              <td>
                <div class="switch" :class="{ on: p.enabled }" @click="toggleProvider(p)" />
              </td>
              <td>
                <div class="row" style="gap: 5px">
                  <button class="btn btn-xs" :disabled="testingId === p.id" @click="testProvider(p)">
                    {{ testingId === p.id ? '…' : '测试' }}
                  </button>
                  <button class="btn btn-xs" :disabled="diagnosing" @click="diagnose(p)">诊断</button>
                  <button class="btn btn-xs" @click="probeProvider(p)">探测</button>
                  <button class="btn btn-xs" @click="openEdit(p)">编辑</button>
                  <button class="btn btn-xs btn-danger" @click="removeProvider(p)">删</button>
                </div>
              </td>
            </tr>
            <tr v-if="!providers.length">
              <td colspan="8" class="empty">还没有接入模型 · 支持 OpenAI 兼容接口 (DeepSeek / Qwen / GLM / Kimi / vLLM 等)</td>
            </tr>
          </tbody>
        </table>
      </div>
      <div v-if="testResult" class="panel-body">
        <div class="note" :style="{ borderLeftColor: testResult.ok ? '#12c48b' : '#f04b5a' }">
          <div class="row" style="gap: 8px; margin-bottom: 4px">
            <span class="tag" :class="testResult.ok ? 'tag-long' : 'tag-short'">{{ testResult.ok ? '连通正常' : '调用失败' }}</span>
            <span class="muted">{{ testResult.latency_ms }}ms · {{ testResult.model }}</span>
            <span class="muted" v-if="testResult.cost_usd">${{ testResult.cost_usd.toFixed(6) }}</span>
          </div>
          <div class="mono" style="font-size: 11px; white-space: pre-wrap">{{ testResult.ok ? testResult.text : testResult.error }}</div>
        </div>
      </div>
    </div>

    <div class="grid g2">
      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">调用用量 · 30 天</span>
          <div class="right">
            <span class="tag tag-dim">总计 {{ usage?.total_30d?.calls ?? 0 }} 次 / ${{ (usage?.total_30d?.cost_usd || 0).toFixed(4) }}</span>
          </div>
        </div>
        <div class="panel-body">
          <EChart v-if="usage?.by_day?.length" :option="usageOption" height="200px" />
          <div v-else class="empty">暂无调用记录</div>
          <div style="margin-top: 10px">
            <div class="row" style="justify-content: space-between; font-size: 11px">
              <span class="muted">月度预算</span>
              <span class="mono">${{ (usage?.budget?.used_usd || 0).toFixed(4) }} / ${{ usage?.budget?.monthly_usd }}</span>
            </div>
            <div class="bar-track" style="margin-top: 5px"><div class="bar-fill" :style="{ width: budgetPct + '%' }" /></div>
          </div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-hd"><span class="panel-title">按服务商</span></div>
        <div class="panel-body flush table-scroll" style="max-height: 260px">
          <table class="table">
            <thead><tr><th>服务商</th><th>调用</th><th>失败</th><th>成本</th></tr></thead>
            <tbody>
              <tr v-for="p in usage?.by_provider || []" :key="p.provider">
                <td>{{ p.provider }}</td>
                <td class="mono">{{ p.calls }}</td>
                <td class="mono" :class="{ down: p.errors }">{{ p.errors }}</td>
                <td class="mono">${{ (p.cost || 0).toFixed(5) }}</td>
              </tr>
              <tr v-if="!usage?.by_provider?.length"><td colspan="4" class="empty">暂无数据</td></tr>
            </tbody>
          </table>
        </div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">调用日志</span>
        <div class="right"><span class="tag tag-dim">{{ logs.length }} 条</span></div>
      </div>
      <div class="panel-body flush table-scroll" style="max-height: 340px">
        <table class="table">
          <thead><tr><th>时间</th><th>服务商</th><th>用途</th><th>结果</th><th>耗时</th><th>Tokens</th><th>成本</th><th>错误</th></tr></thead>
          <tbody>
            <tr v-for="l in logs" :key="l.id">
              <td class="mono muted">{{ timeStr(l.ts) }}</td>
              <td>{{ l.provider }}</td>
              <td><span class="tag tag-dim">{{ l.purpose }}</span></td>
              <td><span class="tag" :class="l.ok ? 'tag-long' : 'tag-short'">{{ l.ok ? '成功' : '失败' }}</span></td>
              <td class="mono">{{ l.latency_ms }}ms</td>
              <td class="mono muted">{{ l.prompt_tokens }}/{{ l.completion_tokens }}</td>
              <td class="mono">${{ (l.cost_usd || 0).toFixed(5) }}</td>
              <td class="muted ellipsis" style="max-width: 300px" :title="l.error">{{ l.error || '—' }}</td>
            </tr>
            <tr v-if="!logs.length"><td colspan="8" class="empty">暂无调用日志</td></tr>
          </tbody>
        </table>
      </div>
    </div>

    <Modal v-if="showForm" :title="editing ? `编辑模型 · ${editing.name}` : '新增模型接入'" @close="showForm = false">
      <div class="stack">
        <div class="row row-wrap" style="gap: 6px">
          <button v-for="p in presets" :key="p.label" class="btn btn-xs" @click="applyPreset(p)">{{ p.label }}</button>
        </div>
        <div class="grid g2" style="gap: 12px">
          <div class="field"><span class="label">显示名称</span><input v-model="form.name" class="input" placeholder="例如 DeepSeek-V3" /></div>
          <div class="field">
            <span class="label">协议</span>
            <select v-model="form.protocol" class="select">
              <option value="openai_chat">OpenAI Chat Completions</option>
              <option value="openai_responses">OpenAI Responses</option>
              <option value="anthropic_messages">Anthropic Messages</option>
            </select>
          </div>
          <div class="field" style="grid-column: span 2"><span class="label">接口地址 (base_url)</span><input v-model="form.base_url" class="input mono" /></div>
          <div class="field">
            <span class="label">模型名</span>
            <div class="row" style="gap: 6px">
              <input v-model="form.model" class="input mono" style="flex: 1; min-width: 0" />
              <button class="btn btn-sm" :disabled="probing" @click="probeModels">
                {{ probing ? '探测中…' : '探测端点' }}
              </button>
            </div>
          </div>
          <div class="field">
            <span class="label">API Key {{ editing ? '(留空则不修改)' : '' }}</span>
            <input v-model="form.api_key" class="input mono" type="password" placeholder="sk-..." />
          </div>
          <div class="field"><span class="label">温度</span><input v-model.number="form.temperature" type="number" step="0.1" min="0" max="2" class="input mono" /></div>
          <div class="field">
            <span class="label">最大输出 tokens</span>
            <input v-model.number="form.max_tokens" type="number" min="64" max="8000" class="input mono" />
            <span class="hint">推理型模型会先消耗 thinking token，建议 ≥ 3000；过小会因 finish_reason=length 截断 JSON。</span>
          </div>
          <div class="field"><span class="label">超时 (秒)</span><input v-model.number="form.timeout_sec" type="number" min="5" max="300" class="input mono" /></div>
          <div class="field"><span class="label">优先级 (越小越先)</span><input v-model.number="form.priority" type="number" min="1" max="999" class="input mono" /></div>
        </div>
        <div class="row" style="gap: 18px">
          <div class="row" style="gap: 6px">
            <div class="switch" :class="{ on: form.json_mode }" @click="form.json_mode = !form.json_mode" />
            <span class="label">请求 JSON 模式</span>
          </div>
          <div class="row" style="gap: 6px">
            <div class="switch" :class="{ on: form.enabled }" @click="form.enabled = !form.enabled" />
            <span class="label">启用</span>
          </div>
        </div>
        <div v-if="probe" class="probe">
          <div class="row" style="gap: 8px; font-size: 11px; flex-wrap: wrap">
            <span class="muted">探测地址</span>
            <span class="mono" style="word-break: break-all">{{ probe.endpoint || '—' }}</span>
            <span class="tag" :class="probe.ok ? 'tag-long' : 'tag-warn'">
              {{ probe.ok ? `${probe.models.length} 个模型` : '探测失败' }}
            </span>
          </div>
          <div v-if="probe.ok" class="row" style="gap: 6px; flex-wrap: wrap; margin-top: 8px">
            <button
              v-for="m in probeShown"
              :key="m.id"
              class="btn btn-xs"
              :class="{ 'btn-primary': m.id === form.model }"
              :title="m.name"
              @click="form.model = m.id"
            >
              {{ m.id }}
            </button>
            <span v-if="probe.models.length > probeShown.length" class="muted" style="font-size: 11px">
              等 {{ probe.models.length }} 个，输入框可直接填
            </span>
          </div>
          <div v-else class="muted" style="font-size: 11px; color: var(--down); margin-top: 6px">
            {{ probe.error }}
          </div>
        </div>
        <div class="hint">Key 使用 Fernet 加密后落库, 接口只返回掩码。若服务端不支持 response_format, 系统会自动降级并做容错解析。</div>
      </div>
      <template #footer>
        <button class="btn" @click="showForm = false">取消</button>
        <button class="btn btn-primary" @click="save">保存</button>
      </template>
    </Modal>

    <Modal v-if="modelPicker" :title="`探测模型 · ${modelPicker.provider.name}`" wide @close="modelPicker = null">
      <div class="stack" style="gap: 10px">
        <div class="row" style="gap: 8px; font-size: 11px; flex-wrap: wrap">
          <span class="muted">探测地址</span>
          <span class="mono" style="word-break: break-all">{{ modelPicker.endpoint || '—' }}</span>
          <span class="tag" :class="modelPicker.ok ? 'tag-long' : 'tag-warn'">
            {{ modelPicker.ok ? `${modelPicker.models.length} 个模型` : '探测失败' }}
          </span>
        </div>
        <div v-if="!modelPicker.ok" class="muted" style="color: var(--down); font-size: 12px">
          {{ modelPicker.error }}
        </div>
        <template v-else>
          <div class="row" style="gap: 6px; flex-wrap: wrap">
            <button
              v-for="m in modelPicker.models"
              :key="m.id"
              class="btn btn-xs"
              :class="{ 'btn-primary': m.id === modelPicker.provider.model }"
              @click="pickModel(m)"
            >
              {{ m.id }}
            </button>
          </div>
          <div class="hint">点击任意模型立即切换并保存；当前使用 {{ modelPicker.provider.model }}</div>
        </template>
      </div>
      <template #footer><button class="btn" @click="modelPicker = null">关闭</button></template>
    </Modal>

    <Modal v-if="diagnoseResult" :title="`接口诊断 · ${diagnoseResult.provider || ''}`" wide @close="diagnoseResult = null">
      <div v-if="diagnosing" class="empty">诊断中…</div>
      <div v-else class="stack">
        <div class="row">
          <span class="tag" :class="diagnoseResult.ok ? 'tag-long' : 'tag-warn'">{{ diagnoseResult.ok ? '全部通过' : '存在异常' }}</span>
          <span class="muted" style="font-size: 11px">{{ diagnoseResult.protocol }} · {{ diagnoseResult.model }}</span>
        </div>
        <table class="table">
          <thead><tr><th>检查项</th><th>结果</th><th>耗时</th><th>详情</th></tr></thead>
          <tbody>
            <tr v-for="c in diagnoseResult.checks" :key="c.name">
              <td>{{ c.name }}</td>
              <td><span class="tag" :class="c.ok ? 'tag-long' : 'tag-short'">{{ c.ok ? '通过' : '失败' }}</span></td>
              <td class="mono">{{ c.latency_ms ? c.latency_ms + 'ms' : '-' }}</td>
              <td class="muted" style="white-space: normal">
                {{ c.detail }}
                <div v-if="c.hint" class="hint">{{ c.hint }}</div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </Modal>
  </div>
</template>
