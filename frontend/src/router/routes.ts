import type { RouteRecordRaw } from 'vue-router'
import { legacyRedirect as legacy } from './guard'

/**
 * 路由表保持为纯配置（不创建 history），便于用 memory history 单测。
 * 页面标题由 workspaces.ts 的页签标签提供，这里不再重复维护。
 */
export const routes: RouteRecordRaw[] = [
  { path: '/login', name: 'login', component: () => import('../views/LoginView.vue') },
  {
    // 前台: 面向访客的公开站点
    path: '/',
    component: () => import('../layouts/SiteLayout.vue'),
    children: [
      { path: '', name: 'home', component: () => import('../views/site/HomeView.vue') },
      { path: 'charts', name: 'site-charts', component: () => import('../views/site/ChartsView.vue') },
      { path: 'insights', name: 'site-insights', component: () => import('../views/site/InsightsView.vue') },
      { path: 'about', name: 'site-about', component: () => import('../views/site/AboutView.vue') },
    ],
  },
  {
    // 后台: 需要登录的管理终端，按 5 个工作区组织，工作区内用页签切换。
    path: '/admin',
    component: () => import('../layouts/ShellLayout.vue'),
    children: [
      { path: '', redirect: '/admin/overview/dashboard' },

      {
        path: 'overview',
        component: () => import('../layouts/WorkspaceLayout.vue'),
        meta: { workspace: 'overview' },
        children: [
          { path: '', redirect: '/admin/overview/dashboard' },
          { path: 'dashboard', name: 'overview-dashboard', component: () => import('../views/DashboardView.vue') },
          { path: 'system', name: 'overview-system', component: () => import('../views/SystemView.vue') },
          { path: 'onboarding', name: 'overview-onboarding', component: () => import('../views/OnboardingView.vue') },
        ],
      },
      {
        path: 'trading',
        component: () => import('../layouts/WorkspaceLayout.vue'),
        meta: { workspace: 'trading' },
        children: [
          { path: '', redirect: '/admin/trading/signals' },
          { path: 'signals', name: 'trading-signals', component: () => import('../views/SignalAuditView.vue') },
          { path: 'symbols', name: 'trading-symbols', component: () => import('../views/SymbolsView.vue') },
        ],
      },
      {
        path: 'research',
        component: () => import('../layouts/WorkspaceLayout.vue'),
        meta: { workspace: 'research' },
        children: [
          { path: '', redirect: '/admin/research/strategies' },
          { path: 'strategies', name: 'research-strategies', component: () => import('../views/StrategiesView.vue') },
          { path: 'backtest', name: 'research-backtest', component: () => import('../views/BacktestView.vue') },
          { path: 'news', name: 'research-news', component: () => import('../views/NewsView.vue') },
        ],
      },
      {
        path: 'ai',
        component: () => import('../layouts/WorkspaceLayout.vue'),
        meta: { workspace: 'ai' },
        children: [
          { path: '', redirect: '/admin/ai/models' },
          { path: 'models', name: 'ai-models', component: () => import('../views/AiView.vue') },
          { path: 'tuning', name: 'ai-tuning', component: () => import('../views/ParameterSetsView.vue') },
          { path: 'evolution', name: 'ai-evolution', component: () => import('../views/EvolutionView.vue') },
          { path: 'journal', name: 'ai-journal', component: () => import('../views/JournalView.vue') },
        ],
      },
      {
        path: 'settings',
        component: () => import('../layouts/WorkspaceLayout.vue'),
        meta: { workspace: 'settings' },
        children: [
          { path: '', redirect: '/admin/settings/params' },
          { path: 'params', name: 'settings-params', component: () => import('../views/StrategyView.vue') },
          { path: 'backup', name: 'settings-backup', component: () => import('../views/BackupView.vue') },
          { path: 'dead-letters', name: 'settings-dead-letters', component: () => import('../views/DeadLetterView.vue') },
        ],
      },

      // 旧地址兼容：历史书签、前端旧链接与命令面板缓存继续可用。
      { path: 'dashboard', redirect: legacy('/admin/overview/dashboard') },
      { path: 'system', redirect: legacy('/admin/overview/system') },
      { path: 'onboarding', redirect: legacy('/admin/overview/onboarding') },
      { path: 'signals', redirect: legacy('/admin/trading/signals') },
      { path: 'symbols', redirect: legacy('/admin/trading/symbols') },
      { path: 'strategies', redirect: legacy('/admin/research/strategies') },
      { path: 'backtest', redirect: legacy('/admin/research/backtest') },
      { path: 'news', redirect: legacy('/admin/research/news') },
      { path: 'ai-tuning', redirect: legacy('/admin/ai/tuning') },
      { path: 'parameter-sets', redirect: legacy('/admin/ai/tuning') },
      { path: 'evolution', redirect: legacy('/admin/ai/evolution') },
      { path: 'journal', redirect: legacy('/admin/ai/journal') },
      { path: 'strategy', redirect: legacy('/admin/settings/params') },
      { path: 'backup', redirect: legacy('/admin/settings/backup') },
      { path: 'dead-letters', redirect: legacy('/admin/settings/dead-letters') },
    ],
  },
  { path: '/:pathMatch(.*)*', redirect: '/' },
]
