<script setup lang="ts">
/**
 * 后台 K 线工作站: 与前台同一套组件与数据源 (klinecharts),
 * 区别只在于取数走登录态接口, 且不依赖公开站点是否开启.
 */
import KlineBoard from '../components/KlineBoard.vue'
</script>

<template>
  <div class="stack" style="gap: 12px">
    <div class="panel">
      <div class="panel-body" style="padding: 16px 20px">
        <div class="row" style="gap: 10px; flex-wrap: wrap">
          <span class="panel-title" style="font-size: 14px">K 线工作站</span>
          <span class="tag tag-dim">klinecharts</span>
          <span class="tag tag-dim">WebSocket 推送</span>
          <span class="tag tag-dim">600 根历史</span>
          <div class="spacer" />
          <router-link to="/admin/symbols" class="btn btn-xs">管理标的池</router-link>
          <router-link to="/admin/signals" class="btn btn-xs">信号审计</router-link>
        </div>
        <div class="hint" style="margin-top: 8px">
          只展示标的池内的合约，顺序即添加顺序；主图支持 MA / EMA / BOLL，副图支持 VOL / MACD / RSI。
          实时数据走币安 WebSocket 推送（不占 REST 权重），顶部显示距收盘的倒计时。
        </div>
      </div>
    </div>

    <KlineBoard variant="admin" />
  </div>
</template>
