import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'

describe('UI tokens', () => {
  it('admin uses the studio green tokens and removes the legacy gold accent', () => {
    const main = readFileSync('src/styles/main.css', 'utf8')
    const studio = readFileSync('src/styles/studio.css', 'utf8')
    const site = readFileSync('src/styles/site.css', 'utf8')
    const css = `${main}\n${studio}\n${site}`

    expect(main).toContain('--accent: #2ee6a6')
    expect(studio).toContain('--accent: #2ee6a6')
    expect(main).toContain('--down: #ff6b6b')
    expect(css).not.toContain('240, 185, 11')
    expect(css).not.toContain('#f0b90b')
  })

  it('public site keeps its own blue theme', () => {
    const site = readFileSync('src/styles/site.css', 'utf8')
    expect(site).toContain('--accent: #4da3ff')
    expect(site).toContain('--up: #4ad6a4')
  })
})
