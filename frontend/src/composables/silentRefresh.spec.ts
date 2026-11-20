import { afterEach, describe, expect, it, vi } from 'vitest'
import { SilentRefreshScheduler } from './silentRefresh'

afterEach(() => {
  vi.useRealTimers()
  vi.restoreAllMocks()
})

describe('SilentRefreshScheduler', () => {
  it('只在页面可见时按间隔刷新，并支持立即刷新', async () => {
    vi.useFakeTimers()
    let visible = true
    const run = vi.fn()
    const scheduler = new SilentRefreshScheduler(run, 1000, () => visible)

    scheduler.start()
    vi.advanceTimersByTime(3000)
    expect(run).toHaveBeenCalledTimes(3)

    visible = false
    vi.advanceTimersByTime(2000)
    expect(run).toHaveBeenCalledTimes(3)
    await scheduler.refreshIfVisible()
    expect(run).toHaveBeenCalledTimes(3)

    visible = true
    await scheduler.refreshIfVisible()
    expect(run).toHaveBeenCalledTimes(4)

    scheduler.stop()
    vi.advanceTimersByTime(2000)
    expect(run).toHaveBeenCalledTimes(4)
  })

  it('上一次刷新未完成时不会重入', async () => {
    vi.useFakeTimers()
    let finish!: () => void
    const run = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          finish = resolve
        }),
    )
    const scheduler = new SilentRefreshScheduler(run, 1000)

    scheduler.start()
    await vi.advanceTimersByTimeAsync(1000)
    expect(run).toHaveBeenCalledTimes(1)

    await vi.advanceTimersByTimeAsync(2000)
    expect(run).toHaveBeenCalledTimes(1)

    finish()
    await Promise.resolve()
    await Promise.resolve()
    await vi.advanceTimersByTimeAsync(1000)
    expect(run).toHaveBeenCalledTimes(2)

    scheduler.stop()
  })
})