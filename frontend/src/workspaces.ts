/**
 * 后台工作区单一配置源。
 *
 * 侧边栏、工作区页签、命令面板与路由守卫都从这里取导航，
 * 避免同一批页面在三个地方各维护一份清单导致漂移。
 * 新增页面时先在这里登记 tab，再在 router 里挂对应组件。
 */

export interface WorkspaceTab {
  /** 完整路径，供页签、命令面板与深链直接跳转 */
  path: string
  /** 子路由名称，便于按 name 跳转与测试 */
  name: string
  label: string
}

export interface Workspace {
  id: string
  label: string
  hint: string
  ico: string
  /** 工作区根路径：侧边栏高亮与默认落点都按它判断 */
  root: string
  tabs: WorkspaceTab[]
}

export const WORKSPACES: readonly Workspace[] = [
  {
    id: 'overview',
    label: '总览',
    hint: '引擎状态、风险提示与开仓前检查',
    ico: '◈',
    root: '/admin/overview',
    tabs: [
      { path: '/admin/overview/dashboard', name: 'overview-dashboard', label: '总览' },
      { path: '/admin/overview/system', name: 'overview-system', label: '系统诊断' },
      { path: '/admin/overview/onboarding', name: 'overview-onboarding', label: '开仓检查' },
    ],
  },
  {
    id: 'trading',
    label: '交易',
    hint: '从信号审计到标的池，回答「为什么开 / 为什么没开」',
    ico: '▥',
    root: '/admin/trading',
    tabs: [
      { path: '/admin/trading/signals', name: 'trading-signals', label: '信号审计' },
      { path: '/admin/trading/symbols', name: 'trading-symbols', label: '标的池' },
    ],
  },
  {
    id: 'research',
    label: '研究',
    hint: '策略规则、回测验证与资讯情绪',
    ico: '◇',
    root: '/admin/research',
    tabs: [
      { path: '/admin/research/strategies', name: 'research-strategies', label: '全局策略' },
      { path: '/admin/research/backtest', name: 'research-backtest', label: '回测' },
      { path: '/admin/research/news', name: 'research-news', label: '资讯情绪' },
    ],
  },
  {
    id: 'ai',
    label: 'AI 治理',
    hint: '模型接入、复核参数、调参与经验沉淀',
    ico: '◉',
    root: '/admin/ai',
    tabs: [
      { path: '/admin/ai/models', name: 'ai-models', label: 'AI 模型' },
      { path: '/admin/ai/tuning', name: 'ai-tuning', label: 'AI 调参' },
      { path: '/admin/ai/evolution', name: 'ai-evolution', label: '自进化心法' },
      { path: '/admin/ai/journal', name: 'ai-journal', label: '决策经验' },
    ],
  },
  {
    id: 'settings',
    label: '设置与运维',
    hint: '参数配置、备份还原与落库补偿',
    ico: '⚙',
    root: '/admin/settings',
    tabs: [
      { path: '/admin/settings/params', name: 'settings-params', label: '参数配置' },
      { path: '/admin/settings/backup', name: 'settings-backup', label: '备份还原' },
      { path: '/admin/settings/dead-letters', name: 'settings-dead-letters', label: '落库补偿' },
    ],
  },
]

/** 按当前路径定位工作区；根路径本身或任意子路径都命中。 */
export function workspaceByPath(path: string): Workspace | undefined {
  return WORKSPACES.find(
    (workspace) => path === workspace.root || path.startsWith(`${workspace.root}/`),
  )
}

export function workspaceById(id: string): Workspace | undefined {
  return WORKSPACES.find((workspace) => workspace.id === id)
}

/** 工作区默认落点：第一个页签。 */
export function workspaceHome(workspace: Workspace): string {
  return workspace.tabs[0]?.path || workspace.root
}
