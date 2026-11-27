import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'

describe('UI tokens', () => {
  it('uses one shared neutral black theme for admin, login and site', () => {
    const tokens = readFileSync('src/styles/tokens.css', 'utf8')
    const main = readFileSync('src/styles/main.css', 'utf8')
    const site = readFileSync('src/styles/site.css', 'utf8')
    const login = readFileSync('src/views/LoginView.vue', 'utf8')
    const css = `${tokens}\n${main}\n${site}`

    // 页面黑与卡片黑对齐参考站的中性黑
    expect(tokens).toContain('--bg: #0a0a0b')
    expect(tokens).toContain('--panel: #111113')
    expect(tokens).toContain('--panel-2: #18181b')
    expect(tokens).toContain('--text: #f4f4f5')
    // 登录按钮沿用项目强调色；密码显隐按钮保持参考站配色
    expect(login).toContain('background: var(--accent)')
    expect(login).toContain('color: #8a8a93')

    expect(tokens).toContain('--accent: #2ee6a6')
    expect(tokens).toContain('color-scheme: dark')
    expect(tokens).toContain("--sans: 'Inter'")
    expect(tokens).toContain("--mono: 'JetBrains Mono'")
    expect(css).not.toContain('240, 185, 11')
    expect(css).not.toContain('#f0b90b')
    // 旧蓝色前台主题必须已经移除
    expect(site).not.toContain('--accent: #4da3ff')
    expect(main).not.toContain('--accent: #4da3ff')
    // 全局只有黑色主题，不再保留浅色 token 或主题切换入口
    expect(tokens).not.toContain('data-theme="light"')
    expect(tokens).not.toContain('--accent: #0b9e6e')
    expect(css).not.toContain('theme-toggle')
  })
})
