import { onBeforeUnmount, onMounted } from 'vue'

export type SilentRefreshRun = () => void | Promise<void>

export class SilentRefreshScheduler {
  private timer: ReturnType<typeof setInterval> | undefined
  private running = false

  constructor(
    private readonly run: SilentRefreshRun,
    private readonly intervalMs: number,
    private readonly isVisible: () => boolean = () => true,
  ) {}

  start() {
    this.stop()
    if (this.intervalMs <= 0) return
    this.timer = setInterval(() => {
      void this.refreshIfVisible()
    }, this.intervalMs)
  }

  stop() {
    if (this.timer !== undefined) {
      clearInterval(this.timer)
      this.timer = undefined
    }
  }

  async refreshIfVisible() {
    if (this.running || !this.isVisible()) return
    try {
      const result = this.run()
      if (result && typeof (result as Promise<void>).then === 'function') {
        this.running = true
        await result
      }
    } catch {
      // 静默刷新失败不打断当前画面；下一次轮询或页面重新可见时重试。
    } finally {
      this.running = false
    }
  }
}

export function useSilentRefresh(
  run: SilentRefreshRun,
  intervalMs: number,
  options: { immediate?: boolean } = {},
) {
  const scheduler = new SilentRefreshScheduler(
    run,
    intervalMs,
    () => typeof document === 'undefined' || !document.hidden,
  )
  let listening = false
  const onVisibilityChange = () => {
    if (typeof document !== 'undefined' && !document.hidden) {
      void scheduler.refreshIfVisible()
    }
  }

  onMounted(() => {
    scheduler.start()
    if (typeof document !== 'undefined') {
      document.addEventListener('visibilitychange', onVisibilityChange)
      listening = true
    }
    if (options.immediate) void scheduler.refreshIfVisible()
  })

  onBeforeUnmount(() => {
    scheduler.stop()
    if (listening && typeof document !== 'undefined') {
      document.removeEventListener('visibilitychange', onVisibilityChange)
      listening = false
    }
  })

  return {
    refreshNow: () => scheduler.refreshIfVisible(),
  }
}