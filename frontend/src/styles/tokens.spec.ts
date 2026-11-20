import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'

describe('UI v2 tokens', () => {
  it('uses blueprint blue and removes the gold brand accent', () => {
    const main = readFileSync('src/styles/main.css', 'utf8')
    const redesign = readFileSync('src/styles/redesign.css', 'utf8')
    const css = `${main}\n${redesign}`

    expect(redesign).toContain('--accent: #4da3ff')
    expect(redesign).toContain('--up: #4ad6a4')
    expect(redesign).toContain('--down: #ff5f7e')
    expect(css).not.toContain('240, 185, 11')
    expect(css).not.toContain('#f0b90b')
  })
})