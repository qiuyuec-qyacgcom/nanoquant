import { ref } from 'vue'

export type Theme = 'dark' | 'light'

const STORAGE_KEY = 'nq-theme'

function readTheme(): Theme {
  const saved = typeof localStorage !== 'undefined' ? localStorage.getItem(STORAGE_KEY) : null
  return saved === 'light' ? 'light' : 'dark'
}

const theme = ref<Theme>(readTheme())

function apply() {
  if (typeof document !== 'undefined') {
    document.documentElement.dataset.theme = theme.value
  }
}

/** 在应用挂载前调用一次，确保首屏就是正确主题。 */
export function initTheme() {
  theme.value = readTheme()
  apply()
}

/** 后台、前台和登录页共用同一个主题状态。 */
export function useTheme() {
  function toggle() {
    theme.value = theme.value === 'dark' ? 'light' : 'dark'
    localStorage.setItem(STORAGE_KEY, theme.value)
    apply()
    window.requestAnimationFrame(() => window.dispatchEvent(new Event('nq-theme')))
  }

  return { theme, toggle }
}
