# NanoQuant UI v2 石墨蓝图极客终端 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 subagent-driven-development（推荐）或 executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 把前后台统一为黑底石墨蓝图 + 磷光终端细节，保留现有业务功能和 K 线工作站。

**架构：** 先改全局 CSS token 与 Shell，再改前台首页/登录、后台总览/参数配置/系统诊断，最后做 390px 响应式。业务逻辑、API、路由结构不变；风险与持仓继续留在 `DashboardView`。

**技术栈：** Vue 3、TypeScript、Vite、Vitest、Playwright（概念稿/视觉检查）、现有 ECharts/K 线组件。

**规格：** `docs/superpowers/specs/2026-11-21-nanoquant-ui-v2-geek-design.md`

## 全局约束

- 全站移除金色主视觉；品牌主色为 `#4da3ff`。
- 绿色只表示盈利/通过，红色只表示亏损/拒绝，橙色只表示警告。
- 后台标题不再使用衬线体；数字统一等宽。
- 不新增 `/admin/risk` 路由；风险与持仓保留在 `DashboardView`。
- 不新增移动端路由；只做 390px 响应式适配。
- 前台首页保留 `KlineBoard`；Hero 终端窗数据标注 `DEMO / ILLUSTRATIVE`。
- 顶栏命令栏 v1 只做页面导航/搜索，不接入启停、平仓、切换环境。
- 不修改后端 API、数据库和交易逻辑。

## 审查重点（Review Focus）

1. **1440 与 390 的横向溢出**：所有表格、图表、按钮在 390px 下不得撑破视口；预期是卡片化/横向滚动有明确容器。
2. **K 线/ECharts canvas 不继承 CSS token**：图表线和坐标轴必须显式传蓝图蓝色板；预期不是残留金色/红色品牌线。
3. **命令栏误触高危操作**：只允许路由跳转和页面搜索；预期不得出现启停、平仓、切换实盘/模拟盘命令。
4. **公开 DEMO 数据误导**：Hero 权益曲线/持仓必须显示 `DEMO / ILLUSTRATIVE`；预期不会被理解为真实账户。
5. **对比度与焦点**：深色面板上的 muted 文字、蓝色按钮、键盘 focus ring 必须可读；预期不是低对比灰字。

---

### 任务 1：全局 token 与通用组件换肤

**文件：**
- 修改：`frontend/src/styles/main.css`
- 修改：`frontend/src/styles/redesign.css`
- 测试：`frontend/src/styles/tokens.spec.ts`（新建）

- [ ] **步骤 1：编写失败的 token 测试**

```ts
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

describe("UI v2 tokens", () => {
  it("removes the gold brand accent and uses blueprint blue", () => {
    const main = readFileSync("src/styles/main.css", "utf8");
    const redesign = readFileSync("src/styles/redesign.css", "utf8");
    expect(main).toContain("--accent: #4da3ff");
    expect(redesign).toContain("--accent: #4da3ff");
    expect(`${main}\n${redesign}`).not.toContain("240, 185, 11");
  });
});
```

- [ ] **步骤 2：运行测试验证失败**

运行：`npm --prefix frontend run test -- src/styles/tokens.spec.ts`
预期：FAIL，旧 token 仍是金色。

- [ ] **步骤 3：替换全局 token 与基础组件**

在 `main.css` 和 `redesign.css` 落地设计文档的色彩/字体/圆角 token；面板、按钮、标签、表格、表单、focus ring 全部改用蓝图蓝。保留 `--up/--down/--warn` 语义色。

- [ ] **步骤 4：运行测试验证通过**

运行：`npm --prefix frontend run test -- src/styles/tokens.spec.ts`
预期：PASS。

### 任务 2：后台 Shell 与导航命令栏

**文件：**
- 修改：`frontend/src/layouts/ShellLayout.vue`
- 创建：`frontend/src/components/commandPalette.ts`
- 创建：`frontend/src/components/CommandPalette.vue`
- 测试：`frontend/src/components/commandPalette.spec.ts`

- [ ] **步骤 1：编写失败的命令过滤测试**

```ts
import { describe, expect, it } from "vitest";
import { filterCommands } from "./commandPalette";

const items = [
  { label: "总览", path: "/admin/dashboard" },
  { label: "参数配置", path: "/admin/strategy" },
];

describe("filterCommands", () => {
  it("matches Chinese labels and paths", () => {
    expect(filterCommands(items, "参数")).toHaveLength(1);
    expect(filterCommands(items, "dashboard")[0].path).toBe("/admin/dashboard");
    expect(filterCommands(items, "不存在")).toHaveLength(0);
  });
});
```

- [ ] **步骤 2：运行测试验证失败**

运行：`npm --prefix frontend run test -- src/components/commandPalette.spec.ts`
预期：FAIL，模块不存在。

- [ ] **步骤 3：实现 `filterCommands(items, query)` 与命令面板**

命令面板只显示 `nav` 中的路由项，支持 `⌘K`/`Ctrl+K` 打开、Esc 关闭、Enter 跳转；不接入任何交易操作。

- [ ] **步骤 4：改造 ShellLayout 视觉**

侧栏 220px、顶栏 62px、活动导航蓝线、状态灯、命令栏入口、头像；保留现有引擎开关和退出按钮的业务行为。

- [ ] **步骤 5：运行测试验证通过**

运行：`npm --prefix frontend run test -- src/components/commandPalette.spec.ts`
预期：PASS。

### 任务 3：前台 SiteLayout 与首页

**文件：**
- 修改：`frontend/src/layouts/SiteLayout.vue`
- 修改：`frontend/src/views/site/HomeView.vue`
- 修改：`frontend/src/components/KlineBoard.vue`
- 修改：`frontend/src/components/KlineChartPanel.vue`
- 测试：视觉检查 + `npm --prefix frontend run typecheck`

- [ ] **步骤 1：改 SiteLayout 顶部导航**

桌面端改为顶部横向导航；390px 保留底部 tabbar；品牌、引擎状态、进入后台按钮使用统一蓝图蓝。

- [ ] **步骤 2：重排 HomeView**

Hero 放首位，保留 `KlineBoard` 作为 Hero 下方独立行情区；终端窗使用静态 DEMO 曲线和三条 DEMO 持仓，并显示 `DEMO / ILLUSTRATIVE`。真实公开数据继续来自 `publicOverview`/`publicPerformance`。

- [ ] **步骤 3：统一 K 线色板**

K 线/ECharts 的网格、轴、主线、十字线改蓝图蓝；涨跌仍用现有绿/红语义。

- [ ] **步骤 4：验证**

运行：`npm --prefix frontend run typecheck`
预期：PASS。截图 1440/390 检查无溢出、无金色、DEMO 标注可见。

### 任务 4：登录页

**文件：**
- 修改：`frontend/src/views/LoginView.vue`

- [ ] **步骤 1：替换样式**

删除金色径向光、金色网格、衬线标题；改石墨蓝黑 + 蓝图网格 + 命令提示符式 eyebrow。保留登录、错误提示、首次登录改密、TLS 说明和路由逻辑。

- [ ] **步骤 2：验证**

运行：`npm --prefix frontend run typecheck`
预期：PASS。截图检查输入框、focus ring、错误态和按钮对比度。

### 任务 5：后台总览（含风险与持仓）

**文件：**
- 修改：`frontend/src/views/DashboardView.vue`
- 修改：`frontend/src/components/Sparkline.vue`
- 修改：`frontend/src/components/EventFeed.vue`
- 测试：现有 Vitest + 视觉检查

- [ ] **步骤 1：重排 KPI 与面板**

保留现有全部指标和业务操作；视觉上聚焦 4 个主 KPI，其余指标下移/收进次级卡片。风险预算环图、权益曲线、当前持仓使用蓝图蓝与新面板样式。

- [ ] **步骤 2：保留持仓操作**

平仓、保护单、口径说明等按钮和行为不变；390px 下持仓表改为卡片化行或横向滚动容器。

- [ ] **步骤 3：验证**

运行：`npm --prefix frontend run test`
预期：现有测试全部 PASS。截图 1440/390 检查。

### 任务 6：参数配置

**文件：**
- 修改：`frontend/src/views/StrategyView.vue`
- 修改：`frontend/src/views/settingsCatalog.ts`（仅样式相关文案/分组，不改字段）

- [ ] **步骤 1：替换参数页视觉**

左侧分组、中间卡片、右侧风险画像全部使用新 token；去掉金色标题和金色保存按钮；保存栏固定在底部，未保存数量和放弃/保存逻辑不变。

- [ ] **步骤 2：验证**

运行：`npm --prefix frontend run test -- src/views/settingsCatalog.spec.ts`
预期：PASS。截图检查分组、输入框、保存栏。

### 任务 7：系统诊断换肤

**文件：**
- 修改：`frontend/src/views/SystemView.vue`

- [ ] **步骤 1：只换肤**

保持日志、健康检查、归档、sidecar 诊断结构不变；替换面板、表格、状态灯、按钮颜色和间距。

- [ ] **步骤 2：验证**

运行：`npm --prefix frontend run typecheck`
预期：PASS。截图检查长日志/表格不溢出。

### 任务 8：390px 响应式与最终验证

**文件：**
- 修改：`frontend/src/styles/main.css`
- 修改：`frontend/src/styles/redesign.css`
- 修改：`frontend/src/layouts/ShellLayout.vue`
- 修改：`frontend/src/layouts/SiteLayout.vue`
- 修改：`frontend/src/views/DashboardView.vue`

- [ ] **步骤 1：补 390px 断点**

Shell 抽屉、顶部导航、KPI 单列、持仓卡片化、图表高度、按钮换行；禁用横向溢出。

- [ ] **步骤 2：全量验证**

运行：
`npm --prefix frontend run typecheck`
`npm --prefix frontend test`
`npm --prefix frontend run build`

预期：全部 PASS。

- [ ] **步骤 3：视觉验收**

截图 1440 与 390；检查无金色主色、无后台衬线、无绿色品牌色、命令栏无高危操作、DEMO 标注存在。

## 交接

计划完成后，执行者使用 `subagent-driven-development` 或 `executing-plans` 逐任务实现；每个任务结束保留可独立截图/构建的工作状态。