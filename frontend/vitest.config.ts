import { defineConfig } from 'vitest/config'

// 单测只跑纯逻辑（格式化、映射表、store 计算），不拉起 jsdom / 浏览器；
// 需要真实渲染的走 Playwright e2e，两者分工不同，别混着写。
export default defineConfig({
  test: {
    include: ['src/**/*.spec.ts'],
    environment: 'node',
    reporters: 'dot',
  },
})
