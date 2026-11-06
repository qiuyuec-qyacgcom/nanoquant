import { defineConfig } from '@playwright/test'

/**
 * 冒烟 e2e：默认打本机已启动的服务（http://127.0.0.1:8899）。
 *
 * 本地 Windows：E2E_CHANNEL=msedge（用系统 Edge，免下载浏览器）
 * CI Ubuntu：默认 chromium，需先 npx playwright install chromium
 */
export default defineConfig({
  testDir: './e2e',
  timeout: 30_000,
  expect: { timeout: 10_000 },
  fullyParallel: false,
  workers: 1,
  reporter: 'list',
  use: {
    baseURL: process.env.E2E_BASE_URL || 'http://127.0.0.1:8899',
    channel: (process.env.E2E_CHANNEL as any) || undefined,
    headless: true,
    trace: 'off',
    screenshot: 'only-on-failure',
  },
})
