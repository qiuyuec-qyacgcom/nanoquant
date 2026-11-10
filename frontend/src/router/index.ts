import { createRouter, createWebHashHistory, type RouteRecordRaw } from 'vue-router'
import { getToken } from '../api/client'

const routes: RouteRecordRaw[] = [
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
    // 后台: 需要登录的管理终端
    path: '/admin',
    component: () => import('../layouts/ShellLayout.vue'),
    children: [
      { path: '', redirect: '/admin/dashboard' },
      { path: 'dashboard', name: 'dashboard', component: () => import('../views/DashboardView.vue'), meta: { title: '总览' } },
      { path: 'onboarding', name: 'onboarding', component: () => import('../views/OnboardingView.vue'), meta: { title: '开仓检查' } },
      { path: 'signals', name: 'signals', component: () => import('../views/SignalAuditView.vue'), meta: { title: '信号审计' } },
      { path: 'signal-follow', name: 'signal-follow', component: () => import('../views/SignalFollowView.vue'), meta: { title: '信号跟单' } },
      { path: 'strategies', name: 'strategies', component: () => import('../views/StrategiesView.vue'), meta: { title: '策略' } },
      { path: 'symbols', name: 'symbols', component: () => import('../views/SymbolsView.vue'), meta: { title: '标的池' } },
      { path: 'chart', name: 'chart', component: () => import('../views/AdminChartView.vue'), meta: { title: 'K 线工作站' } },
      { path: 'backtest', name: 'backtest', component: () => import('../views/BacktestView.vue'), meta: { title: '回测' } },
      { path: 'news', name: 'news', component: () => import('../views/NewsView.vue'), meta: { title: '资讯情绪' } },
      { path: 'ai', name: 'ai', component: () => import('../views/AiView.vue'), meta: { title: 'AI 模型' } },
      { path: 'strategy', name: 'strategy', component: () => import('../views/StrategyView.vue'), meta: { title: '参数配置' } },
      { path: 'evolution', name: 'evolution', component: () => import('../views/EvolutionView.vue'), meta: { title: '自进化心法' } },
      { path: 'backup', name: 'backup', component: () => import('../views/BackupView.vue'), meta: { title: '备份还原' } },
      { path: 'dead-letters', name: 'dead-letters', component: () => import('../views/DeadLetterView.vue'), meta: { title: '落库补偿' } },
      { path: 'system', name: 'system', component: () => import('../views/SystemView.vue'), meta: { title: '系统诊断' } },
    ],
  },
  { path: '/:pathMatch(.*)*', redirect: '/' },
]

const router = createRouter({
  history: createWebHashHistory(),
  routes,
  scrollBehavior: () => ({ top: 0 }),
})

router.beforeEach((to) => {
  const token = getToken()
  const needsAuth = to.path.startsWith('/admin')
  if (needsAuth && !token) return { path: '/login', query: { next: to.fullPath } }
  if (to.path === '/login' && token) return { path: '/admin/dashboard' }
  return true
})

export default router
