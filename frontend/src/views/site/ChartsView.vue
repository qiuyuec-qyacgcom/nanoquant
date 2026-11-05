<script setup lang="ts">
/**
 * 前台行情终端: 只展示后台标的池内的合约.
 * 顺序、空池提示、增删同步全部由 KlineBoard 负责.
 */
import KlineBoard from '../../components/KlineBoard.vue'
</script>

<template>
  <div class="stack" style="gap: 14px">
    <div class="panel">
      <div class="panel-body" style="padding: 18px 20px">
        <div class="row" style="gap: 10px; flex-wrap: wrap">
          <h1 style="font-size: 18px; margin: 0">行情终端</h1>
          <span class="tag tag-dim">仅标的池内合约</span>
          <span class="tag tag-dim">按添加顺序</span>
          <div class="spacer" />
          <span class="muted" style="font-size: 11px">数据：币安 U 本位永续 · WebSocket 推送</span>
        </div>
        <div class="muted" style="font-size: 11.5px; line-height: 1.85; margin-top: 8px">
          前台不展示全市场行情：只有后台「标的池」里已添加的合约才会出现在这里。
          池为空时模块显示“未添加标的”；新增或删除标的会在 10 秒内自动同步，
          并严格按添加顺序排列。
        </div>
      </div>
    </div>

    <KlineBoard />
  </div>
</template>
