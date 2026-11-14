import { expect, test } from '@playwright/test'

/** 只验证「页面能打开、登录能过、关键面板在」，不碰下单、不改配置。
 *  路由是 hash 模式（/#/admin/...），所以 goto 必须带 /#/ 前缀。 */

const USER = process.env.E2E_USER || 'admin'
// 初始密码不再固定（首次启动随机生成）：先登录改密，再 export E2E_PASS=<你的密码> 后运行
const PASS = process.env.E2E_PASS || ''

async function login(page: import('@playwright/test').Page) {
  if (!PASS) throw new Error('请先设置 E2E_PASS（管理员密码）再运行 e2e')
  await page.goto('/#/login')
  await page.locator('input[autocomplete="username"]').fill(USER)
  await page.locator('input[type="password"]').fill(PASS)
  await page.getByRole('button', { name: '进入后台' }).click()
  await expect(page).toHaveURL(/#\/admin\/dashboard/, { timeout: 15_000 })
}

test('公开首页可访问', async ({ page }) => {
  const res = await page.goto('/')
  expect(res?.ok()).toBeTruthy()
  await expect(page.locator('body')).toContainText('NanoQuant')
})

test('登录后进入总览', async ({ page }) => {
  await login(page)
  await expect(page.locator('body')).toContainText('引擎控制')
})

test('后台核心页面均有关键面板', async ({ page }) => {
  await login(page)

  const pages: Array<[string, string]> = [
    ['/#/admin/system', '一键诊断'],
    ['/#/admin/symbols', '当前标的池'],
    ['/#/admin/backtest', '回测（纯量化粗筛）'],
    ['/#/admin/evolution', '心法效果对照'],
    ['/#/admin/ai', '决策演练'],
  ]
  for (const [path, text] of pages) {
    await page.goto(path)
    await expect(page.locator('body')).toContainText(text)
  }
})
