import { expect, test } from '@playwright/test'

/** 图表是按需引入 echarts 的：少注册一个 chart/component，页面文字照样在，
 *  但 canvas 会静默空白。这条用例专门盯"真的画出来了"和"控制台没有缺组件报错"。
 *  同时顺带确认总览的「委托流水」面板存在（订单状态收敛之后新增的）。 */

const USER = process.env.E2E_USER || 'admin'
const PASS = process.env.E2E_PASS || 'admin123'

async function login(page: import('@playwright/test').Page) {
  await page.goto('/#/login')
  await page.locator('input[autocomplete="username"]').fill(USER)
  await page.locator('input[type="password"]').fill(PASS)
  await page.getByRole('button', { name: '进入后台' }).click()
  await expect(page).toHaveURL(/#\/admin\/dashboard/, { timeout: 15_000 })
}

test('总览图表真的渲染出来了', async ({ page }) => {
  const errors: string[] = []
  page.on('console', (msg) => {
    if (msg.type() === 'error') errors.push(msg.text())
  })
  page.on('pageerror', (err) => errors.push(String(err)))

  await login(page)

  const canvas = page.locator('canvas').first()
  await expect(canvas).toBeVisible({ timeout: 15_000 })
  const box = await canvas.boundingBox()
  expect(box?.width ?? 0).toBeGreaterThan(100)
  expect(box?.height ?? 0).toBeGreaterThan(60)

  // echarts 缺注册时会打印 "Component series.line not exists" / "There is a chart instance..."
  const joined = errors.join('\n')
  expect(joined).not.toContain('not exists')
  expect(joined).not.toContain('is not a function')

  await expect(page.locator('body')).toContainText('委托流水')
})
