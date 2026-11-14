<script setup lang="ts">
/**
 * 前台首页 (落地页)
 * -----------------------------------------------------------------
 * 与「行情 / 观点 / 关于」三个模块做职责切分，不再重复堆数据：
 * - 行情：K 线工作站（标的池驱动）
 * - 观点：资讯流 / 情绪 / 心法
 * - 关于：开仓链路 / 技术栈 / 风险声明
 * 首页只负责：品牌与定位、实时运行状态、能力概览、脱敏绩效、入口引导。
 *
 * 版式参考 astra-quant-agent 的 LandingView（Hero + 状态卡 + 能力网格 + CTA）。
 */
import { computed, onBeforeUnmount, onMounted, ref } from 'vue'
import { publicOverview, publicPerformance } from '../../api/public'
import { numClass, timeStr } from '../../utils/format'
import KlineBoard from '../../components/KlineBoard.vue'

const overview = ref<any>(null)
const perf = ref<any>(null)

const engineOn = computed(() => !!overview.value?.engine?.enabled)
const envLabel = computed(() => (overview.value?.engine?.mode === 'live' ? '实盘 · 币安主网' : '模拟盘 · 币安测试网'))
const poolCount = computed(() => overview.value?.symbol_count ?? 0)
const sentiment = computed(() => overview.value?.news?.sentiment_index ?? 0)

const capabilities = [
  { title: '5–30 分钟短线执行', desc: '主周期 5m/15m/30m，5 秒一轮扫描；盘口点差超限直接放弃，不让滑点吃掉本就微薄的利润。' },
  { title: '五道风控闸门', desc: '权益 / 日内亏损 / 回撤 / 连亏 / 敞口 / 冷却 / 时段，任一不通过即拒绝，并记录可读原因。' },
  { title: 'AI 只收紧不放宽', desc: '大模型只能同意、否决、降杠杆或收窄止损；输出不合规按 hold 处理，不参与资金管理。' },
  { title: '模拟盘 / 实盘同构', desc: '模拟盘对接币安测试网，实盘对接主网，同一套风控与执行链路，切换环境不改策略。' },
]

async function load() {
  const [o, p] = await Promise.allSettled([publicOverview(), publicPerformance()])
  if (o.status === 'fulfilled') overview.value = o.value
  if (p.status === 'fulfilled') perf.value = p.value
}

let timer: number | undefined
onMounted(async () => {
  await load()
  timer = window.setInterval(load, 20000)
})
onBeforeUnmount(() => window.clearInterval(timer))
</script>

<template>
  <div class="stack" style="gap: 18px">
    <!-- K 线工作台 (前台首页): 标的池驱动, 公开数据源, 无需登录 -->
    <section>
      <KlineBoard />
    </section>

    <!-- Hero: 定位 + 实时状态卡 -->
    <section class="landing-hero panel">
      <div class="landing-grid">
        <div class="landing-copy">
          <div class="row" style="gap: 8px; flex-wrap: wrap">
            <span class="tag tag-blue">MICRO CAPITAL · BINANCE FUTURES</span>
            <span class="tag tag-dim">5–30m 短线</span>
          </div>
          <h1 class="landing-title">
            把极小资金跑成<br />一条可解释的超短线执行链
          </h1>
          <p class="landing-desc">
            信号 → 五道风控闸门 → AI 复核 → 币安永续下单 → 自动复盘。
            每一笔“为什么没开仓”都能在信号审计里找到原因，而不是黑盒调参。
          </p>
          <div class="row" style="gap: 8px; flex-wrap: wrap">
            <router-link to="/insights" class="btn btn-primary">观点与心法</router-link>
            <router-link to="/admin/dashboard" class="btn">进入后台</router-link>
          </div>
          <div class="landing-stats">
            <div class="landing-stat"><b>5m</b><span>主周期</span></div>
            <div class="landing-stat"><b>{{ poolCount }}</b><span>跟踪标的</span></div>
            <div class="landing-stat"><b>{{ overview?.engine?.mode === 'live' ? 'LIVE' : 'DEMO' }}</b><span>交易环境</span></div>
            <div class="landing-stat"><b>{{ perf?.enabled ? perf.trades : '—' }}</b><span>闭环交易</span></div>
          </div>
        </div>

        <aside class="status-card">
          <div class="row" style="gap: 8px">
            <span class="dot" :class="engineOn ? 'on' : 'off'" />
            <b style="font-size: 12.5px">引擎{{ engineOn ? '运行中' : '待命' }}</b>
            <div class="spacer" />
            <span class="tag" :class="overview?.engine?.mode === 'live' ? 'tag-short' : 'tag-long'">{{ envLabel }}</span>
          </div>

          <div class="status-line"><span>跟踪标的</span><b class="mono">{{ poolCount }} 个</b></div>
          <div class="status-line"><span>标的顺序</span><b class="mono">按添加顺序</b></div>
          <div class="status-line"><span>最近一轮</span><b class="mono">{{ timeStr(overview?.engine?.last_cycle_ts || 0) }}</b></div>
          <div class="status-line"><span>资讯情绪</span><b class="mono" :class="numClass(sentiment)">{{ sentiment.toFixed(3) }}</b></div>

          <router-link to="/charts" class="status-cta">
            打开行情终端
            <span>→</span>
          </router-link>
        </aside>
      </div>
    </section>

    <!-- 能力概览 -->
    <section>
      <div class="section-title">为什么是这套系统</div>
      <div class="grid g4" style="gap: 12px">
        <div v-for="c in capabilities" :key="c.title" class="panel feature">
          <div class="panel-body">
            <div style="font-size: 13px; font-weight: 600; margin-bottom: 7px">{{ c.title }}</div>
            <div class="muted" style="font-size: 11.5px; line-height: 1.85">{{ c.desc }}</div>
          </div>
        </div>
      </div>
    </section>

    <!-- 脱敏绩效 -->
    <section v-if="perf?.enabled" class="panel">
      <div class="panel-hd">
        <span class="panel-title">策略绩效（脱敏）</span>
        <div class="right"><span class="tag tag-dim">不展示金额</span></div>
      </div>
      <div class="panel-body grid g4" style="gap: 12px">
        <div class="metric"><span>闭环交易</span><span class="mono">{{ perf.trades }}</span></div>
        <div class="metric"><span>胜率</span><span class="mono">{{ perf.win_rate }}%</span></div>
        <div class="metric"><span>盈亏比</span><span class="mono">{{ perf.profit_factor }}</span></div>
        <div class="metric"><span>平均持仓</span><span class="mono">{{ perf.avg_hold_seconds }}s</span></div>
      </div>
    </section>

    <!-- 入口分流 -->
    <section class="grid g3">
      <router-link to="/charts" class="panel entry">
        <div class="panel-body">
          <div class="entry-ico">▤</div>
          <div class="entry-title">行情终端</div>
          <div class="entry-desc">标的池驱动的 K 线工作站：MA / BOLL / MACD / RSI，3 秒级刷新。</div>
        </div>
      </router-link>
      <router-link to="/insights" class="panel entry">
        <div class="panel-body">
          <div class="entry-ico">✦</div>
          <div class="entry-title">观点与心法</div>
          <div class="entry-desc">资讯流、情绪指数与自进化心法，解释每一次进出场的依据。</div>
        </div>
      </router-link>
      <router-link to="/about" class="panel entry">
        <div class="panel-body">
          <div class="entry-ico">ⓘ</div>
          <div class="entry-title">关于系统</div>
          <div class="entry-desc">开仓链路、技术栈与风险声明；先看清楚，再决定是否接入。</div>
        </div>
      </router-link>
    </section>

    <!-- 风险提示 -->
    <section class="note" style="margin: 0">
      合约杠杆可能造成本金全部损失。本站展示的信号、复盘与绩效均不构成投资建议；
      模拟盘与实盘都需要自行配置币安 API 凭证，实盘切换需二次确认。
    </section>
  </div>
</template>

<style scoped>
.landing-hero {
  overflow: hidden;
  background:
    radial-gradient(120% 140% at 0% 0%, rgba(16, 185, 129, 0.14), transparent 55%),
    radial-gradient(90% 120% at 100% 0%, rgba(52, 211, 153, 0.1), transparent 60%),
    #0e1118;
}
.landing-grid {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 300px;
  gap: 26px;
  padding: 30px 30px 26px;
  align-items: center;
}
.landing-title {
  margin: 14px 0 10px;
  font-size: 30px;
  line-height: 1.35;
  letter-spacing: -0.01em;
  font-weight: 650;
}
.landing-desc {
  margin: 0 0 18px;
  max-width: 560px;
  font-size: 12.5px;
  line-height: 1.95;
  color: var(--muted);
}
.landing-stats {
  display: flex;
  gap: 26px;
  margin-top: 22px;
  flex-wrap: wrap;
}
.landing-stat {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.landing-stat b {
  font-family: var(--mono);
  font-size: 19px;
  font-weight: 600;
  color: var(--text);
}
.landing-stat span {
  font-size: 10.5px;
  color: var(--muted);
}
.status-card {
  display: flex;
  flex-direction: column;
  gap: 9px;
  padding: 14px;
  border: 1px solid var(--line);
  border-radius: 11px;
  background: rgba(7, 13, 24, 0.62);
  backdrop-filter: blur(4px);
}
.status-line {
  display: flex;
  align-items: center;
  justify-content: space-between;
  font-size: 11.5px;
  padding: 5px 0;
  border-bottom: 1px dashed rgba(148, 170, 200, 0.12);
}
.status-line span {
  color: var(--muted);
}
.status-line b {
  font-weight: 500;
}
.status-cta {
  display: flex;
  align-items: center;
  justify-content: space-between;
  margin-top: 4px;
  padding: 8px 10px;
  border-radius: 8px;
  background: rgba(16, 185, 129, 0.14);
  border: 1px solid rgba(16, 185, 129, 0.32);
  color: #6ee7b7;
  font-size: 11.5px;
  text-decoration: none;
}
.status-cta:hover {
  background: rgba(16, 185, 129, 0.22);
}
.entry {
  text-decoration: none;
  color: inherit;
  transition: border-color 0.15s, transform 0.15s;
}
.entry:hover {
  border-color: rgba(16, 185, 129, 0.38);
  transform: translateY(-1px);
}
.entry-ico {
  width: 30px;
  height: 30px;
  display: grid;
  place-items: center;
  border-radius: 8px;
  background: rgba(16, 185, 129, 0.12);
  color: #6ee7b7;
  margin-bottom: 10px;
}
.entry-title {
  font-size: 13px;
  font-weight: 600;
  margin-bottom: 6px;
}
.entry-desc {
  font-size: 11.5px;
  line-height: 1.8;
  color: var(--muted);
}
@media (max-width: 900px) {
  .landing-grid {
    grid-template-columns: minmax(0, 1fr);
    padding: 22px 18px;
  }
  .landing-title {
    font-size: 23px;
  }
  .status-card {
    order: 2;
  }
}
</style>
