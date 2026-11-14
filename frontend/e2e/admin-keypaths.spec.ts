import { expect, test } from '@playwright/test'

/**
 * 后台关键路径：参数检索 / 跳转 / 未保存保护 / 只读页面渲染。
 *
 * 刻意**不写**任何真正保存或开关引擎的用例：
 * 这套 e2e 默认打本机正在跑的服务，改配置或停引擎会直接影响正在交易的账户。
 * 需要验证写路径时，用「改动 → 放弃」来覆盖脏状态与保存条，不落库。
 */

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

test('参数配置：跨分组搜索能定位并高亮字段', async ({ page }) => {
  await login(page)
  await page.goto('/#/admin/strategy')
  // 等首屏参数渲染完再操作：配置是异步拉的，抢跑会点到空分组
  await expect(page.locator('.field-row').first()).toBeVisible()

  const search = page.getByPlaceholder(/搜索参数/)
  await expect(search).toBeVisible()
  // 用"周期"而不是"杠杆"：杠杆类字段是后加的，旧后端可能还没有，
  // 会让用例把"服务端版本旧"误报成"搜索坏了"。
  await search.fill('周期')

  const firstHit = page.locator('.search-hit').first()
  await expect(firstHit).toBeVisible()
  await firstHit.click()

  // 跳转后目标字段会短暂带 focus 类（2 秒高亮）
  await expect(page.locator('.field-row.focus')).toBeVisible({ timeout: 3_000 })
})

test('参数配置：常用直达一键跳到指定参数', async ({ page }) => {
  await login(page)
  await page.goto('/#/admin/strategy')
  await expect(page.locator('.field-row').first()).toBeVisible()

  await page.getByRole('button', { name: 'K 线周期' }).click()
  await expect(page.locator('#fld-strategy-timeframe')).toBeVisible()
  await expect(page.locator('.field-row.focus')).toBeVisible({ timeout: 3_000 })
})

test('参数配置：改动后出现保存条，放弃修改后消失（不落库）', async ({ page }) => {
  await login(page)
  await page.goto('/#/admin/strategy')
  await expect(page.locator('.field-row').first()).toBeVisible()

  await page.getByRole('button', { name: /风控/ }).first().click()
  const field = page.locator('#fld-risk-max_drawdown_pct input')
  await expect(field).toBeVisible()

  // 值必须与当前不同，否则 v-model 不产生改动、保存条不会出现（线上配置可能已经是 18）
  const current = await field.inputValue()
  await field.fill(current === '18' ? '19' : '18')
  const bar = page.locator('.save-bar')
  await expect(bar).toBeVisible()
  await expect(bar).toContainText('未保存')

  await page.getByRole('button', { name: '放弃修改' }).click()
  await expect(page.locator('.save-bar')).toHaveCount(0)
})

test('风控分组：日亏基准区块与重置入口存在', async ({ page }) => {
  await login(page)
  await page.goto('/#/admin/strategy?group=risk')

  await expect(page.locator('body')).toContainText('日亏基准')
  await expect(page.getByRole('button', { name: /重置基准/ })).toBeVisible()
})

test('信号跟单与总览：关键面板可渲染', async ({ page }) => {
  await login(page)

  await page.goto('/#/admin/signal-follow')
  await expect(page.locator('body')).toContainText(/跟仓|跳过|演练/)

  await page.goto('/#/admin/dashboard')
  await expect(page.locator('body')).toContainText('引擎控制')
})

test('交易环境提供紧急全平入口，取消确认不会执行', async ({ page }) => {
  const dialogs: string[] = []
  page.on('dialog', async (d) => {
    dialogs.push(d.message())
    await d.dismiss() // 点取消：绝不能真的清仓
  })
  await login(page)
  await page.goto('/#/admin/strategy')

  await page.getByRole('button', { name: '紧急全平' }).click()
  await expect(page.getByRole('button', { name: '紧急全平' })).toBeVisible()
  expect(dialogs.join(' ')).toContain('FLATTEN')
})
