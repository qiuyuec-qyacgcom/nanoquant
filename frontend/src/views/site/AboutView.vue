<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { publicConfig, type PublicConfig } from '../../api/public'

const cfg = ref<PublicConfig | null>(null)

const chain = [
  { step: '行情', desc: '按 1m K 线计算 EMA / RSI / ATR / 量比 / 资金费率，合成方向分。' },
  { step: '信号', desc: '方向分与波动率、量能三重过滤，只有达标才进入下一关。' },
  { step: '风控闸门', desc: '权益、日内亏损、回撤、连亏、敞口、冷却、交易时段逐项校验。' },
  { step: '仓位', desc: '风险预算反推名义额，再被保证金上限截断并按步进取整。' },
  { step: 'AI 复核', desc: '模型只能同意/否决/降杠杆/收窄止损，输出不合规按 hold 处理。' },
  { step: '下单与持仓', desc: '市价开仓 + 止损止盈保护单，之后由循环持续管理至平仓。' },
]

onMounted(async () => {
  try {
    cfg.value = await publicConfig()
  } catch {
    /* 忽略 */
  }
})
</script>

<template>
  <div class="stack" style="gap: 20px">
    <div class="panel">
      <div class="panel-body" style="padding: 26px">
        <h1 style="font-size: 22px; margin: 0 0 10px">关于 {{ cfg?.site_title || 'NanoQuant' }}</h1>
        <p class="muted" style="font-size: 12.5px; line-height: 1.9; max-width: 760px; margin: 0">
          面向极小资金（最低 1 USDT）的币安 U 本位永续合约量化系统。将量化信号、风控闸门、AI 复核与自进化复盘
          串成一条可解释、可审计的执行链；默认模拟盘模式，所有参数热更新，支持选择性备份与还原。
        </p>
        <div class="row" style="gap: 8px; margin-top: 16px; flex-wrap: wrap">
          <span class="tag tag-blue">v{{ cfg?.version || '-' }}</span>
          <span class="tag" :class="cfg?.mode === 'live' ? 'tag-short' : 'tag-long'">
            {{ cfg?.mode === 'live' ? '实盘模式' : '模拟盘模式' }}
          </span>
          <span class="tag tag-dim">FastAPI + SQLite</span>
          <span class="tag tag-dim">Vue 3 + ECharts</span>
        </div>
      </div>
    </div>

    <div>
      <div class="section-title">开仓链路</div>
      <div class="grid g3" style="gap: 12px">
        <div v-for="(c, i) in chain" :key="c.step" class="panel">
          <div class="panel-body">
            <div class="row" style="gap: 8px; margin-bottom: 6px">
              <span class="step-idx">{{ i + 1 }}</span>
              <b style="font-size: 12.5px">{{ c.step }}</b>
            </div>
            <div class="muted" style="font-size: 11.5px; line-height: 1.8">{{ c.desc }}</div>
          </div>
        </div>
      </div>
    </div>

    <div class="panel">
      <div class="panel-hd"><span class="panel-title">风险声明</span></div>
      <div class="panel-body" style="font-size: 12px; line-height: 1.95">
        <p style="margin: 0 0 10px">
          合约交易具有高杠杆特性，价格小幅波动即可造成本金全部损失。本站展示的行情、信号、AI 建议与历史复盘
          均不构成投资建议，也不承诺任何收益。
        </p>
        <p style="margin: 0 0 10px">
          系统默认以模拟盘模式运行；切换到实盘需要管理员显式配置交易所密钥并二次确认。任何策略都可能失效，
          请自行评估风险并使用可承受损失的资金。
        </p>
        <p style="margin: 0" class="muted">
          公开页面仅展示聚合行情与脱敏信息，不展示账户余额、持仓、订单与密钥。完整功能请登录后台。
        </p>
        <div class="row" style="margin-top: 16px">
          <router-link to="/admin/dashboard" class="btn btn-primary">进入后台</router-link>
          <router-link to="/insights" class="btn">浏览观点</router-link>
        </div>
      </div>
    </div>
  </div>
</template>
