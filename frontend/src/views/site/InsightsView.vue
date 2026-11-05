<script setup lang="ts">
import { computed, onMounted, ref } from 'vue'
import { publicMantras, publicNews } from '../../api/public'
import { numClass, timeStr } from '../../utils/format'

const news = ref<any[]>([])
const sentiment = ref<any>(null)
const mantras = ref<any[]>([])
const mantraStats = ref<any>(null)
const loading = ref(true)

const positivePct = computed(() => {
  const total = sentiment.value?.total_24h || 0
  if (!total) return 0
  return Math.round(((sentiment.value?.positive || 0) / total) * 100)
})

async function load() {
  loading.value = true
  try {
    const [n, m] = await Promise.allSettled([publicNews(50), publicMantras(12)])
    if (n.status === 'fulfilled') {
      news.value = n.value.items || []
      sentiment.value = n.value.sentiment || null
    }
    if (m.status === 'fulfilled') {
      mantras.value = m.value.items || []
      mantraStats.value = m.value.stats || null
    }
  } finally {
    loading.value = false
  }
}

onMounted(load)
</script>

<template>
  <div class="stack" style="gap: 18px">
    <div class="grid g4" style="gap: 12px">
      <div class="kpi">
        <div class="kpi-label">24H 资讯</div>
        <div class="kpi-value">{{ sentiment?.total_24h ?? 0 }}</div>
        <div class="kpi-foot">偏多 {{ sentiment?.positive ?? 0 }} · 偏空 {{ sentiment?.negative ?? 0 }}</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">情绪指数</div>
        <div class="kpi-value" :class="numClass(sentiment?.sentiment_index)">
          {{ (sentiment?.sentiment_index ?? 0).toFixed(3) }}
        </div>
        <div class="kpi-foot">-1 极空 → +1 极多</div>
      </div>
      <div class="kpi">
        <div class="kpi-label">正向占比</div>
        <div class="kpi-value">{{ positivePct }}%</div>
        <div class="bar-track" style="margin-top: 10px"><div class="bar-fill" :style="{ width: positivePct + '%' }" /></div>
      </div>
      <div class="kpi">
        <div class="kpi-label">分析心法</div>
        <div class="kpi-value">{{ mantraStats?.active ?? 0 }}</div>
        <div class="kpi-foot">累计 {{ mantraStats?.total ?? 0 }} 条 · 已替代 {{ mantraStats?.superseded ?? 0 }}</div>
      </div>
    </div>

    <div class="grid g2">
      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">市场资讯</span>
          <div class="right"><span class="tag tag-dim">{{ news.length }} 条</span></div>
        </div>
        <div class="panel-body" style="max-height: 640px; overflow-y: auto">
          <div v-if="loading" class="empty">加载中…</div>
          <div v-else-if="!news.length" class="empty">暂无资讯</div>
          <div v-for="n in news" :key="n.id" class="list-item">
            <div class="row" style="align-items: flex-start; gap: 10px">
              <span class="tag" :class="n.sentiment > 0.15 ? 'tag-long' : n.sentiment < -0.15 ? 'tag-short' : 'tag-dim'" style="margin-top: 2px">
                {{ n.sentiment > 0 ? '+' : '' }}{{ n.sentiment.toFixed(2) }}
              </span>
              <div style="flex: 1; min-width: 0">
                <a v-if="n.url" :href="n.url" target="_blank" rel="noreferrer" class="item-title">{{ n.title }}</a>
                <div v-else class="item-title">{{ n.title }}</div>
                <div class="item-meta">
                  <span class="tag tag-dim">{{ n.source_label }}</span>
                  <span>{{ timeStr(n.ts, false) }}</span>
                  <span v-for="s in n.symbols" :key="s" class="tag tag-blue">{{ s }}</span>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>

      <div class="panel">
        <div class="panel-hd">
          <span class="panel-title">自进化心法（脱敏）</span>
          <div class="right"><span class="tag tag-blue">仅展示标题与健康度</span></div>
        </div>
        <div class="panel-body">
          <div v-if="!mantras.length" class="empty">尚无公开心法</div>
          <div v-for="m in mantras" :key="m.id" class="list-item">
            <div class="row" style="gap: 10px">
              <div style="flex: 1; min-width: 0">
                <div class="item-title">{{ m.title }}</div>
                <div class="item-meta">
                  <span class="tag tag-dim">{{ m.category }}</span>
                  <span class="mono">v{{ m.version }}</span>
                </div>
              </div>
              <div style="width: 92px">
                <div class="row" style="justify-content: space-between; font-size: 10px">
                  <span class="muted">健康度</span>
                  <span class="mono">{{ (m.health * 100).toFixed(0) }}</span>
                </div>
                <div class="bar-track" style="margin-top: 4px">
                  <div class="bar-fill" :style="{ width: Math.round(m.health * 100) + '%' }" />
                </div>
              </div>
            </div>
          </div>
          <div class="hint" style="margin-top: 12px">
            心法由每笔交易的复盘自动生成，经「防污染审计」过滤后按历史有效性排序；后台可查看完整规则与注入实况。
          </div>
        </div>
      </div>
    </div>
  </div>
</template>
