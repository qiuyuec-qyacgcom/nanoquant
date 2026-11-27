import { createRouter, createWebHashHistory } from 'vue-router'
import { getToken, getMustChangePassword } from '../api/client'
import { authRedirect } from './guard'
import { routes } from './routes'

const router = createRouter({
  history: createWebHashHistory(),
  routes,
  scrollBehavior: () => ({ top: 0 }),
})

router.beforeEach((to) =>
  authRedirect({
    path: to.path,
    fullPath: to.fullPath,
    hasToken: Boolean(getToken()),
    mustChangePassword: getMustChangePassword(),
  }) ?? true,
)

export default router
