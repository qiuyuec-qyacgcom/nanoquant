<script setup lang="ts">
/**
 * 参数配置中心
 * ---------------------------------------------------------------
 * 排版原则:
 * - 交易环境 (模拟盘/实盘) 是唯一开关, 固定在最上方, 不混在分组参数里;
 * - 左侧分组导航 + 右侧分区表单, 每区只放同语义的字段;
 * - 字段按语义选择控件 (开关/分段/数字+单位/标签/文本), 并给出口径说明;
 * - 顶部保留「未保存」提示与保存/恢复默认, 避免改完不知道有没有生效。
 */
import { computed, onMounted, reactive, ref, watch } from 'vue'
import { useRoute } from 'vue-router'
import { http } from '../api/client'
import EnvironmentSwitch from '../components/EnvironmentSwitch.vue'
import { useAppStore } from '../stores/app'
import { useAuthStore } from '../stores/auth'

type Widget = 'switch' | 'number' | 'text' | 'seg' | 'select' | 'tags' | 'json'

interface FieldMeta {
  label: string
  hint?: string
  unit?: string
  widget?: Widget
  options?: [string, string][]
}

interface SectionDef {
  title: string
  hint?: string
  fields: string[]
}

interface Row extends FieldMeta {
  key: string
  value: any
  widget: Widget
}

const groups = [
  { id: 'engine', label: '引擎', hint: '开关、节奏与时段' },
  { id: 'risk', label: '风控', hint: '仓位、熔断与费用' },
  { id: 'strategy', label: '策略', hint: '信号、过滤与出场' },
  { id: 'data', label: '数据源', hint: '行情源、网络与密钥' },
  { id: 'news', label: '资讯', hint: '抓取与情绪' },
  { id: 'evolution', label: '自进化', hint: '复盘与心法' },
  { id: 'backup', label: '备份', hint: '保留与加密' },
  { id: 'security', label: '安全', hint: '登录与会话' },
  { id: 'system', label: '系统', hint: '日志与保留' },
  { id: 'public', label: '公开站点', hint: '前台展示' },
]

const BASE_META: Record<string, FieldMeta> = {
  enabled: { label: '启用', hint: '关闭后该模块不参与运行' },
  cycle_interval_sec: { label: '轮询间隔', unit: '秒', hint: '越小越灵敏，但会消耗更多 API 权重' },
  max_symbols_per_cycle: { label: '每轮扫描标的', unit: '个', hint: '按信号分从标的池取前 N 个' },
  max_positions: { label: '最大同时持仓', unit: '个' },
  trading_windows: { label: '交易时段', widget: 'tags', hint: 'HH:MM-HH:MM，可多个；00:00-24:00 表示全天' },
  timezone_offset: { label: '时区偏移', unit: '小时', hint: '交易时段所用时区，中国为 +8' },
  demo_start_balance: { label: '模拟盘初始资金', unit: 'USDT', hint: '仅模拟盘生效；改动不会重置已有余额' },
  order_type: {
    label: '下单类型',
    widget: 'seg',
    options: [
      ['market', '市价'],
      ['limit_post_only', '只做 Maker'],
    ],
    hint: '市价优先成交；只做 Maker 挂单失败即放弃，省手续费',
  },
  recv_window_ms: { label: '签名有效窗口', unit: 'ms', hint: '建议 5000，系统时钟漂移时调大' },
  close_on_halt: { label: '熔断时平仓', hint: '关闭则只锁新开仓，不动已有持仓' },

  risk_per_trade_pct: { label: '单笔风险', unit: '%', hint: '止损打掉时的最大亏损占权益比例' },
  margin_pct: { label: '单仓保证金', unit: '%', hint: '与杠杆共同决定名义额' },
  default_leverage: { label: '默认杠杆', unit: 'x' },
  max_leverage: { label: '杠杆上限', unit: 'x', hint: '1–100x，单标的配置只能收紧不能突破' },
  max_exposure_pct: { label: '总敞口上限', unit: '%', hint: '所有持仓名义额 / 权益' },
  daily_loss_limit_pct: { label: '日内亏损上限', unit: '%', hint: '触发后当日停止开新仓' },
  max_drawdown_pct: { label: '最大回撤', unit: '%', hint: '基于权益峰值' },
  consecutive_loss_limit: { label: '连亏熔断', unit: '笔' },
  cooldown_minutes: { label: '熔断冷静期', unit: '分钟' },
  symbol_cooldown_minutes: { label: '单标的冷却', unit: '分钟', hint: '亏损后该标的的冷却时间' },
  stoploss_guard_count: { label: '止损熔断阈值', unit: '次', hint: '窗口内触发这么多次止损就全局暂停开仓；0=关闭' },
  stoploss_guard_window_min: { label: '止损熔断窗口', unit: '分钟', hint: '统计窗口' },
  stoploss_guard_pause_min: { label: '止损熔断暂停', unit: '分钟', hint: '暂停时长' },
  min_equity_usdt: { label: '最低运行权益', unit: 'USDT', hint: '低于该值停止开仓' },
  max_slippage_pct: { label: '最大滑点', unit: '%', hint: '模拟盘撮合与实盘保护价参考' },
  min_notional_buffer: { label: '最小名义额缓冲', unit: '×', hint: '交易所下限的倍数，防止 -1013' },
  fee_rate_taker: { label: '吃单费率', hint: '0.0005 = 0.05%' },
  fee_rate_maker: { label: '挂单费率', hint: '0.0002 = 0.02%' },

  timeframe: {
    label: 'K 线周期',
    widget: 'seg',
    options: [
      ['5m', '5m'],
      ['15m', '15m'],
      ['30m', '30m'],
      ['1h', '1h'],
    ],
    hint: '5–30 分钟短线主周期',
  },
  ema_fast: { label: '快线 EMA' },
  ema_slow: { label: '慢线 EMA' },
  rsi_period: { label: 'RSI 周期' },
  atr_period: { label: 'ATR 周期' },
  entry_threshold: { label: '开仓信号阈值', hint: '综合分绝对值高于该值才考虑开仓' },
  exit_threshold: { label: '反转平仓阈值' },
  min_signal_gap: { label: '信号分歧缓冲', hint: '多空分数接近时直接忽略，避免来回打脸' },
  volume_ratio_min: { label: '最小量比', hint: '低于该值的信号被过滤' },
  atr_pct_min: { label: 'ATR 下限', unit: '%', hint: '波动太小不足以覆盖手续费' },
  atr_pct_max: { label: 'ATR 上限', unit: '%', hint: '波动过大不参与' },
  min_24h_quote_volume: { label: '24H 最小成交额', unit: 'USDT' },
  sl_atr_mult: { label: '止损 = ATR ×' },
  tp_atr_mult: { label: '止盈 = ATR ×' },
  trailing_pct: { label: '移动止损回撤', unit: '%', hint: '0 表示关闭' },
  breakeven_at_r: { label: '保本触发', unit: 'R', hint: '浮盈达到该 R 倍数后止损上移到开仓价' },
  max_hold_minutes: { label: '最长持仓', unit: '分钟', hint: '超时强制离场' },
  partial_tp_at_r: { label: '部分止盈触发', unit: 'R', hint: '浮盈达到该 R 倍数先平一部分；0=关闭' },
  partial_tp_ratio: { label: '部分止盈比例', unit: '比例', hint: '0.5 = 平一半，剩余交给移动止损' },
  max_spread_bps: { label: '盘口点差上限', unit: 'bps', hint: '买一卖一价差超过该值就放弃开仓；0=关闭。短线建议 5–12bps' },
  roi_table: {
    label: '时间阶梯止盈',
    widget: 'json',
    hint: '[[持仓分钟, 收益率%]]: 持仓越久要求越低；空数组=关闭。例 [[0,0.55],[2,0.4],[5,0.28]]',
  },
  weight_trend: { label: '权重 · 趋势' },
  weight_momentum: { label: '权重 · 动能' },
  weight_volume: { label: '权重 · 量能' },
  weight_funding: { label: '权重 · 资金费率' },
  weight_oi: { label: '权重 · 持仓量' },

  live_base_url: { label: '实盘域名', hint: '币安 U 本位主网 fapi 地址' },
  demo_base_url: { label: '模拟盘域名', hint: '币安测试网地址（模拟盘真实下单环境）' },
  backup_base_urls: { label: '实盘备用域名', widget: 'tags', hint: '主域名不可达时按顺序尝试' },
  proxy: { label: 'HTTP 代理', hint: '形如 http://127.0.0.1:7890，留空不使用' },
  kline_limit: { label: 'K 线长度', unit: '根', hint: '引擎内部取价使用，不作为对外行情接口' },
  ticker_ttl_sec: { label: '行情缓存', unit: '秒', hint: '引擎内部 ticker 缓存，减少 API 权重' },
  kline_ttl_sec: { label: 'K 线缓存', unit: '秒' },
  request_timeout_sec: { label: '请求超时', unit: '秒' },
  max_pool_size: { label: '标的池上限', unit: '个', hint: '与 astra 规则一致，超限拒绝添加' },

  refresh_minutes: { label: '刷新间隔', unit: '分钟' },
  sources: { label: '资讯源', widget: 'tags', hint: 'coindesk / binance_square / binance_announcement 等' },
  keep_days: { label: '保留天数', unit: '天' },
  ai_summary: { label: 'AI 摘要' },
  sentiment_threshold: { label: '情绪阈值', hint: '绝对值超过该值才标记为显著情绪' },
  max_items_per_source: { label: '单源最多条数', unit: '条' },
  rsshub_base: { label: 'RSSHub 地址', hint: '可选，自建实例用于抓取币安公告' },

  review_on_close: { label: '平仓即复盘' },
  max_injected: { label: '最多注入心法', unit: '条' },
  ttl_days: { label: '心法有效期', unit: '天', hint: '过期自动退出注入' },
  min_confidence: { label: '心法最低置信度' },
  global_review_every_trades: { label: '全局复盘间隔', unit: '笔' },
  guard_strict: { label: '严格防污染', hint: '拦截放宽风控类结论' },
  auto_disable_negative: { label: '负向心法自动降权' },
  max_rule_chars: { label: '心法最大字数', unit: '字' },

  retention: { label: '备份保留份数', unit: '份' },
  encrypt: { label: '备份加密', hint: 'Fernet 对称加密' },
  auto_daily: { label: '每日自动备份' },
  include_logs: { label: '包含日志文件' },

  username: { label: '管理员账号' },
  require_login: { label: '强制登录' },
  session_ttl_hours: { label: '会话有效期', unit: '小时' },
  login_fail_limit: { label: '登录失败锁定阈值', unit: '次' },

  log_level: {
    label: '日志级别',
    widget: 'seg',
    options: [
      ['DEBUG', 'DEBUG'],
      ['INFO', 'INFO'],
      ['WARNING', 'WARN'],
      ['ERROR', 'ERROR'],
    ],
  },
  heartbeat_sec: { label: '心跳间隔', unit: '秒' },
  event_retention_days: { label: '事件保留', unit: '天' },
  signal_retention_days: { label: '信号保留', unit: '天', hint: '过期先归档到 data/archive/ 再删除' },
  equity_retention_days: { label: '权益曲线保留', unit: '天' },
  ui_refresh_sec: { label: '面板刷新', unit: '秒' },
  risk_disclaimer_accepted: { label: '已确认风险声明' },
  alert_enabled: { label: '启用告警', hint: '事件级别达到阈值时推送，失败不影响交易' },
  alert_min_level: {
    label: '最低级别',
    widget: 'seg',
    options: [
      ['info', 'INFO'],
      ['warn', 'WARN'],
      ['error', 'ERROR'],
    ],
  },
  alert_webhook_url: { label: 'Webhook 地址', hint: '钉钉 / 飞书 / 企业微信 文本机器人 URL' },
  alert_error_webhook_url: { label: 'Error 专用 Webhook', hint: '值班群；留空则回落到上面的默认 Webhook' },
  alert_telegram_token: { label: 'Telegram Bot Token' },
  alert_telegram_chat_id: { label: 'Telegram Chat ID' },
  alert_cooldown_sec: { label: '同类告警冷却', unit: '秒', hint: '同一类别在窗口内只发一次' },
  alert_quiet_hours: { label: '静默时段', hint: '如 23:00-07:00，期间只放行 error；留空=不静默' },
  alert_template: { label: '消息模板', hint: '支持 {level} {category} {message} {time}；留空=默认格式' },
  alert_digest_enabled: { label: '摘要模式', hint: '开启后 warn 攒到间隔合并成一条，error 仍然即时' },
  alert_digest_minutes: { label: '摘要间隔', unit: '分钟' },

  site_enabled: { label: '启用公开前台', hint: '关闭后访客只能看到维护页，后台不受影响' },
  site_title: { label: '站点名称' },
  site_tagline: { label: '站点副标题' },
  hero_note: { label: '首页备注', hint: '可选，展示在首页英雄区' },
  show_engine_status: { label: '展示引擎状态', hint: '仅开关与模式，不含资金数据' },
  show_mantras: { label: '展示分析心法', hint: '脱敏展示：仅标题/分类/健康度' },
  show_performance: { label: '展示绩效指标', hint: '胜率/盈亏比等比例数据，不含金额' },
}

const GROUP_META: Record<string, Record<string, FieldMeta>> = {}

const SECTIONS: Record<string, SectionDef[]> = {
  engine: [
    { title: '运行与节奏', hint: '引擎每轮扫描标的池，节奏决定灵敏度', fields: ['cycle_interval_sec', 'max_symbols_per_cycle', 'max_positions'] },
    { title: '交易时段', hint: '只在窗口内开新仓，持仓管理不受限', fields: ['trading_windows', 'timezone_offset'] },
    { title: '模拟盘与下单', fields: ['demo_start_balance', 'order_type', 'recv_window_ms', 'close_on_halt'] },
  ],
  risk: [
    { title: '仓位与杠杆', hint: '先由风险预算反推名义额，再被保证金与敞口截断', fields: ['risk_per_trade_pct', 'margin_pct', 'default_leverage', 'max_leverage', 'max_exposure_pct'] },
    { title: '熔断与冷却', hint: '触发后只锁新开仓', fields: ['daily_loss_limit_pct', 'max_drawdown_pct', 'consecutive_loss_limit', 'cooldown_minutes', 'symbol_cooldown_minutes', 'min_equity_usdt'] },
    { title: '止损熔断', hint: '窗口内多次止损 = 策略与行情脱节，先暂停观察', fields: ['stoploss_guard_count', 'stoploss_guard_window_min', 'stoploss_guard_pause_min'] },
    { title: '撮合与手续费', fields: ['max_slippage_pct', 'min_notional_buffer', 'fee_rate_taker', 'fee_rate_maker'] },
  ],
  strategy: [
    { title: '周期与指标', fields: ['timeframe', 'ema_fast', 'ema_slow', 'rsi_period', 'atr_period'] },
    { title: '开仓过滤', hint: '任一项不达标都不开仓', fields: ['entry_threshold', 'exit_threshold', 'min_signal_gap', 'volume_ratio_min', 'atr_pct_min', 'atr_pct_max', 'min_24h_quote_volume'] },
    { title: '出场规则', fields: ['sl_atr_mult', 'tp_atr_mult', 'trailing_pct', 'breakeven_at_r', 'max_hold_minutes'] },
    { title: '部分止盈', hint: '先落袋一部分，剩余仓位用移动止损奔跑', fields: ['partial_tp_at_r', 'partial_tp_ratio'] },
    {
      title: '短线执行',
      hint: '进场先扣点差，出场按持仓时长阶梯止盈：这两项决定 5–30 分钟级别的净收益',
      fields: ['max_spread_bps', 'roi_table'],
    },
    { title: '因子权重', hint: '加权求和后映射到 [-1, 1]', fields: ['weight_trend', 'weight_momentum', 'weight_volume', 'weight_funding', 'weight_oi'] },
  ],
  data: [
    { title: '标的池规则', hint: '添加标的时用币安在线合约表校验；池内规格随添加落库', fields: ['max_pool_size'] },
    { title: '交易所网络', hint: '模拟盘固定走测试网，实盘走主网；两者都需要各自的 API 凭证', fields: ['demo_base_url', 'live_base_url', 'backup_base_urls', 'proxy'] },
    { title: '引擎取价', hint: '引擎内部直接读取币安公共接口，不对外提供行情接口', fields: ['ticker_ttl_sec', 'kline_ttl_sec', 'kline_limit', 'request_timeout_sec'] },
  ],
  news: [
    { title: '抓取节奏', fields: ['enabled', 'refresh_minutes', 'max_items_per_source', 'keep_days'] },
    { title: '内容与打分', fields: ['ai_summary', 'sentiment_threshold', 'sources'] },
    { title: '网络', fields: ['proxy', 'request_timeout_sec', 'rsshub_base'] },
  ],
  evolution: [
    { title: '复盘', fields: ['enabled', 'review_on_close', 'global_review_every_trades'] },
    { title: '心法库', hint: '心法入库前会经过防污染审计', fields: ['max_injected', 'ttl_days', 'min_confidence', 'max_rule_chars', 'guard_strict', 'auto_disable_negative'] },
  ],
  backup: [{ title: '备份策略', fields: ['retention', 'encrypt', 'auto_daily', 'include_logs'] }],
  security: [{ title: '登录与密钥', fields: ['username', 'require_login', 'session_ttl_hours', 'login_fail_limit'] }],
  system: [
    { title: '运行与日志', fields: ['log_level', 'heartbeat_sec', 'ui_refresh_sec', 'timezone_offset'] },
    { title: '数据保留', hint: '过期数据先写 gzip 归档到 data/archive/ 再按批删除', fields: ['event_retention_days', 'signal_retention_days', 'equity_retention_days', 'risk_disclaimer_accepted'] },
    {
      title: '告警推送',
      hint: '引擎停摆、账户异常、连续止损等事件推送到 Webhook / Telegram；保存后可用 POST /system/alerts/test 自测',
      fields: [
        'alert_enabled',
        'alert_min_level',
        'alert_webhook_url',
        'alert_error_webhook_url',
        'alert_telegram_token',
        'alert_telegram_chat_id',
        'alert_cooldown_sec',
        'alert_quiet_hours',
        'alert_template',
        'alert_digest_enabled',
        'alert_digest_minutes',
      ],
    },
  ],
  public: [
    { title: '站点信息', fields: ['site_enabled', 'site_title', 'site_tagline', 'hero_note'] },
    { title: '展示开关', hint: '公开页绝不展示账户余额、持仓与密钥', fields: ['show_engine_status', 'show_mantras', 'show_performance'] },
  ],
}

const SKIP: Record<string, string[]> = {
  engine: ['mode', 'enabled'],
}

// 只读/敏感字段: 不渲染、不回写; 否则「保存」会用页面加载时的旧密文覆盖刚更新的密钥
const PROTECTED = new Set([
  'api_key_enc',
  'api_secret_enc',
  'demo_api_key_enc',
  'demo_api_secret_enc',
  'live_api_key_enc',
  'live_api_secret_enc',
  'password_hash',
  'last_run_ts',
])

const app = useAppStore()
const auth = useAuthStore()
const route = useRoute()

const current = ref('engine')
const draft = ref<Record<string, any>>({})
const baseline = ref<Record<string, any>>({})
const saving = ref(false)
const credentials = ref<any>(null)
const credForms = reactive({
  demo: { api_key: '', api_secret: '' },
  live: { api_key: '', api_secret: '' },
})
const proxyForm = ref('')
const credSaving = ref<'' | 'demo' | 'live'>('')
const pwd = reactive({ old_password: '', new_password: '' })
const tagDraft = reactive<Record<string, string>>({})
const jsonText = reactive<Record<string, string>>({})
const jsonError = reactive<Record<string, string>>({})
const engineBusy = ref(false)

const currentGroup = computed(() => groups.find((g) => g.id === current.value))
const dirty = computed(() => JSON.stringify(draft.value) !== JSON.stringify(baseline.value))
const engineRunning = computed(() => !!app.status?.enabled)
const engineLive = computed(() => app.status?.mode === 'live')

function clone<T>(v: T): T {
  return JSON.parse(JSON.stringify(v ?? {}))
}

function metaOf(group: string, key: string): FieldMeta {
  return GROUP_META[group]?.[key] || BASE_META[key] || { label: key }
}

function widgetOf(value: any, meta: FieldMeta): Widget {
  if (meta.widget) return meta.widget
  if (typeof value === 'boolean') return 'switch'
  if (typeof value === 'number') return 'number'
  if (Array.isArray(value)) return value.some((v) => typeof v === 'object') ? 'json' : 'tags'
  if (value && typeof value === 'object') return 'json'
  return 'text'
}

function rowOf(group: string, key: string, value: any): Row {
  const meta = metaOf(group, key)
  return { key, value, widget: widgetOf(value, meta), label: meta.label, hint: meta.hint || '', unit: meta.unit || '', options: meta.options }
}

const currentSections = computed(() => {
  const skip = new Set([...PROTECTED, ...(SKIP[current.value] || []), 'symbols'])
  const rows = new Map<string, Row>()
  for (const [key, value] of Object.entries(draft.value || {})) {
    if (skip.has(key)) continue
    rows.set(key, rowOf(current.value, key, value))
  }
  const out: { title: string; hint: string; rows: Row[] }[] = []
  for (const def of SECTIONS[current.value] || []) {
    const list = def.fields.filter((k) => rows.has(k)).map((k) => rows.get(k)!)
    if (!list.length) continue
    list.forEach((r) => rows.delete(r.key))
    out.push({ title: def.title, hint: def.hint || '', rows: list })
  }
  if (rows.size) out.push({ title: '其他参数', hint: '未归类的字段，保持后端默认语义', rows: Array.from(rows.values()) })
  return out
})

function syncJsonText() {
  for (const key of Object.keys(jsonText)) delete jsonText[key]
  for (const [key, value] of Object.entries(draft.value || {})) {
    if (value && typeof value === 'object') {
      jsonText[key] = JSON.stringify(value, null, 2)
      jsonError[key] = ''
    }
  }
}

function loadGroup(id: string) {
  current.value = id
  draft.value = clone(app.settings[id] || {})
  baseline.value = clone(app.settings[id] || {})
  syncJsonText()
}

function onGroupClick(id: string) {
  if (!dirty.value || id === current.value) {
    loadGroup(id)
    return
  }
  if (confirm('当前分组有未保存的修改，切换后会被丢弃，继续？')) loadGroup(id)
}

async function save() {
  saving.value = true
  try {
    const payload: Record<string, any> = { ...draft.value }
    for (const key of PROTECTED) delete payload[key]
    // 交易环境的 mode/enabled 由顶部开关与引擎控制接口管理, 不随表单回写
    if (current.value === 'engine') {
      delete payload.mode
      delete payload.enabled
    }
    const data = await app.saveGroup(current.value, payload)
    draft.value = clone(data)
    baseline.value = clone(data)
    syncJsonText()
    app.notify('配置已保存并热生效', 'ok')
    if (current.value === 'engine' || current.value === 'data') await app.loadStatus()
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    saving.value = false
  }
}

async function reset() {
  if (!confirm('恢复当前分组为默认值？')) return
  try {
    const data = await app.resetGroup(current.value)
    draft.value = clone(data)
    baseline.value = clone(data)
    syncJsonText()
    if (current.value === 'engine' || current.value === 'data') await app.loadStatus()
    app.notify('已恢复默认值', 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

async function toggleEngine() {
  if (engineBusy.value) return
  const target = !engineRunning.value
  if (target && !confirm('启动引擎后将按当前交易环境运行，确认启动？')) return
  engineBusy.value = true
  try {
    await app.control({ enabled: target })
    app.notify(target ? '引擎已启动' : '引擎已停止', 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    engineBusy.value = false
  }
}

async function onEnvChanged() {
  await app.loadStatus()
}

function addTag(key: string) {
  const raw = (tagDraft[key] || '').trim()
  if (!raw) return
  const list: string[] = Array.isArray(draft.value[key]) ? [...draft.value[key]] : []
  for (const part of raw.split(/[\s,，]+/)) {
    if (part && !list.includes(part)) list.push(part)
  }
  draft.value[key] = list
  tagDraft[key] = ''
}

function removeTag(key: string, value: string) {
  draft.value[key] = (draft.value[key] || []).filter((v: string) => v !== value)
}

function applyJson(key: string) {
  try {
    draft.value[key] = JSON.parse(jsonText[key] || 'null')
    jsonError[key] = ''
  } catch {
    jsonError[key] = 'JSON 格式有误，保存前请修正'
  }
}

async function loadCredentials() {
  try {
    credentials.value = await http.get('/settings/credentials/exchange')
    proxyForm.value = credentials.value?.proxy || ''
  } catch {
    credentials.value = null
  }
}

async function saveCredentials(env: 'demo' | 'live') {
  const form = credForms[env]
  if (!form.api_key && !form.api_secret) {
    app.notify('请至少填写 API Key 与 Secret', 'error')
    return
  }
  credSaving.value = env
  try {
    const payload: any = { env }
    if (form.api_key) payload.api_key = form.api_key
    if (form.api_secret) payload.api_secret = form.api_secret
    if (proxyForm.value !== '') payload.proxy = proxyForm.value
    await http.post('/settings/credentials/exchange', payload)
    form.api_key = ''
    form.api_secret = ''
    await Promise.all([loadCredentials(), app.loadSettings()])
    app.notify(`${env === 'demo' ? '模拟盘（测试网）' : '实盘（主网）'}凭据已保存`, 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    credSaving.value = ''
  }
}

async function clearCredentials(env: 'demo' | 'live') {
  const label = env === 'demo' ? '模拟盘（测试网）' : '实盘（主网）'
  if (!confirm(`清空${label}的 API 密钥？`)) return
  try {
    await http.del(`/settings/credentials/exchange?env=${env}`)
    await Promise.all([loadCredentials(), app.loadSettings()])
    app.notify('已清空', 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

async function changePassword() {
  if (!pwd.old_password || pwd.new_password.length < 6) {
    app.notify('请填写原密码，新密码至少 6 位', 'error')
    return
  }
  try {
    await auth.changePassword(pwd.old_password, pwd.new_password)
    pwd.old_password = ''
    pwd.new_password = ''
    await app.loadSettings()
    app.notify('密码已更新', 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

watch(current, (id) => {
  draft.value = clone(app.settings[id] || {})
  baseline.value = clone(app.settings[id] || {})
  syncJsonText()
})

onMounted(async () => {
  if (!Object.keys(app.settings).length) await app.loadSettings()
  await app.loadStatus()
  const wanted = String(route.query.group || '')
  loadGroup(groups.some((g) => g.id === wanted) ? wanted : 'engine')
  await loadCredentials()
})
</script>

<template>
  <div class="stack">
    <div class="panel">
      <div class="panel-hd">
        <span class="panel-title">交易环境</span>
        <div class="right">
          <span class="tag" :class="engineLive ? 'tag-short' : 'tag-long'">{{ engineLive ? '实盘' : '模拟盘' }}</span>
          <span class="dot" :class="engineRunning ? 'on' : 'off'" />
          <span class="muted" style="font-size: 11px">{{ engineRunning ? '引擎已启动' : '引擎未启动' }}</span>
          <button class="btn btn-sm" :disabled="engineBusy" @click="toggleEngine">
            {{ engineBusy ? '…' : engineRunning ? '停止引擎' : '启动引擎' }}
          </button>
        </div>
      </div>
      <div class="panel-body stack" style="gap: 10px">
        <EnvironmentSwitch @changed="onEnvChanged" />
        <div class="hint">
          模拟盘对接<b>币安测试网</b>，实盘对接<b>币安主网</b>，两者都需要各自配置 API 凭证并真实下单。
          切换后只管理当前环境的仓位，另一环境的仓位会保留但暂停管理。
        </div>
      </div>
    </div>

    <div class="settings-layout">
      <aside class="settings-nav">
        <button
          v-for="g in groups"
          :key="g.id"
          type="button"
          class="nav-item"
          :class="{ on: current === g.id }"
          @click="onGroupClick(g.id)"
        >
          <span class="nav-name">{{ g.label }}</span>
          <span class="nav-hint">{{ g.hint }}</span>
        </button>
      </aside>

      <div class="stack">
        <div class="panel">
          <div class="panel-hd">
            <div class="stack" style="gap: 2px">
              <span class="panel-title">{{ currentGroup?.label }}参数</span>
              <span class="muted" style="font-size: 10.5px">{{ currentGroup?.hint }}</span>
            </div>
            <div class="right">
              <span v-if="dirty" class="tag tag-warn">未保存</span>
              <button class="btn btn-sm" @click="reset">恢复默认</button>
              <button class="btn btn-sm btn-primary" :disabled="saving" @click="save">
                {{ saving ? '保存中…' : '保存并生效' }}
              </button>
            </div>
          </div>

          <div class="panel-body stack" style="gap: 16px">
            <!-- 数据源: 两套环境凭证 (模拟盘=测试网 / 实盘=主网) -->
            <div v-if="current === 'data'" class="section">
              <div class="section-hd">
                <span class="section-title">交易环境凭证</span>
                <span class="section-hint">模拟盘与实盘各自一套，Fernet 加密存储；切换环境前必须配置对应凭证</span>
              </div>
              <div class="grid g2" style="gap: 12px">
                <div v-for="env in (['demo', 'live'] as const)" :key="env" class="env-card">
                  <div class="row" style="gap: 8px">
                    <span class="tag" :class="env === 'demo' ? 'tag-long' : 'tag-short'">
                      {{ env === 'demo' ? '模拟盘 · 测试网' : '实盘 · 主网' }}
                    </span>
                    <div class="spacer" />
                    <span class="tag" :class="credentials?.[env]?.configured ? 'tag-long' : 'tag-warn'">
                      {{ credentials?.[env]?.configured ? '已配置 ' + credentials[env].api_key_masked : '未配置' }}
                    </span>
                  </div>
                  <div class="field" style="margin-top: 8px">
                    <span class="label">API Key</span>
                    <input v-model="credForms[env].api_key" class="input mono" type="password" placeholder="留空不修改" autocomplete="off" />
                  </div>
                  <div class="field" style="margin-top: 8px">
                    <span class="label">API Secret</span>
                    <input v-model="credForms[env].api_secret" class="input mono" type="password" placeholder="留空不修改" autocomplete="off" />
                  </div>
                  <div class="row" style="gap: 8px; margin-top: 10px">
                    <button class="btn btn-sm btn-primary" :disabled="credSaving === env" @click="saveCredentials(env)">
                      {{ credSaving === env ? '保存中…' : '保存' }}
                    </button>
                    <button
                      v-if="credentials?.[env]?.configured"
                      class="btn btn-sm btn-danger"
                      @click="clearCredentials(env)"
                    >
                      清空
                    </button>
                    <span class="hint">
                      {{ env === 'demo' ? '币安测试网密钥 · HMAC / Ed25519' : '币安主网密钥 · HMAC / Ed25519' }}
                    </span>
                  </div>
                </div>
              </div>
              <div class="field" style="margin-top: 10px; max-width: 420px">
                <span class="label">HTTP 代理（可选）</span>
                <input v-model="proxyForm" class="input mono" placeholder="http://127.0.0.1:7890" />
              </div>
              <div class="hint">
                只需要合约交易权限，建议绑定 IP 并关闭提币。模拟盘使用币安测试网（testnet.binancefuture.com），
                实盘使用主网（fapi.binance.com）。
              </div>
              <div class="hint">
                密钥类型两种都支持：<b>HMAC</b>（Key/Secret 均为币安生成）与 <b>Ed25519</b>。
                Ed25519 的 API Key 填<b>币安创建密钥后返回的那串 Key</b>（不是公钥），
                API Secret 填<b>本地私钥</b>（PEM 或 base64 均可，直接粘贴即可）。
              </div>
            </div>

            <!-- 安全: 改密 -->
            <div v-if="current === 'security'" class="section">
              <div class="section-hd">
                <span class="section-title">修改密码</span>
                <span class="section-hint">修改后已登录会话立即失效</span>
              </div>
              <div class="cred-grid">
                <div class="field">
                  <span class="label">原密码</span>
                  <input v-model="pwd.old_password" type="password" class="input" autocomplete="current-password" />
                </div>
                <div class="field">
                  <span class="label">新密码</span>
                  <input v-model="pwd.new_password" type="password" class="input" autocomplete="new-password" placeholder="至少 6 位" />
                </div>
              </div>
              <div class="row">
                <button class="btn btn-sm btn-primary" @click="changePassword">更新密码</button>
              </div>
            </div>

            <div v-for="sec in currentSections" :key="sec.title" class="section">
              <div class="section-hd">
                <span class="section-title">{{ sec.title }}</span>
                <span v-if="sec.hint" class="section-hint">{{ sec.hint }}</span>
              </div>

              <div v-for="f in sec.rows" :key="f.key" class="field-row">
                <div class="field-meta">
                  <div class="field-name" :title="f.key">
                    {{ f.label }}
                    <span v-if="f.unit" class="field-unit">{{ f.unit }}</span>
                  </div>
                  <div v-if="f.hint" class="field-hint">{{ f.hint }}</div>
                </div>

                <div class="field-ctrl">
                  <div v-if="f.widget === 'switch'" class="row" style="gap: 10px">
                    <div class="switch" :class="{ on: draft[f.key] }" @click="draft[f.key] = !draft[f.key]" />
                    <span class="muted" style="font-size: 11.5px">{{ draft[f.key] ? '开启' : '关闭' }}</span>
                  </div>

                  <div v-else-if="f.widget === 'seg'" class="seg" role="group">
                    <button
                      v-for="opt in f.options || []"
                      :key="opt[0]"
                      type="button"
                      class="seg-btn"
                      :class="{ on: draft[f.key] === opt[0] }"
                      @click="draft[f.key] = opt[0]"
                    >
                      {{ opt[1] }}
                    </button>
                  </div>

                  <select v-else-if="f.widget === 'select'" v-model="draft[f.key]" class="input">
                    <option v-for="opt in f.options || []" :key="opt[0]" :value="opt[0]">{{ opt[1] }}</option>
                  </select>

                  <div v-else-if="f.widget === 'number'" class="num-wrap">
                    <input v-model.number="draft[f.key]" type="number" step="any" class="input mono" />
                    <span v-if="f.unit" class="num-unit">{{ f.unit }}</span>
                  </div>

                  <div v-else-if="f.widget === 'tags'" class="tags-box">
                    <span v-for="v in draft[f.key]" :key="v" class="chip">
                      {{ v }}<b class="chip-x" @click="removeTag(f.key, v)">×</b>
                    </span>
                    <input
                      v-model="tagDraft[f.key]"
                      class="chip-input"
                      :placeholder="draft[f.key]?.length ? '继续添加…' : '输入后回车'"
                      @keyup.enter="addTag(f.key)"
                      @blur="addTag(f.key)"
                    />
                  </div>

                  <div v-else-if="f.widget === 'json'" class="stack" style="gap: 4px">
                    <textarea
                      class="input mono"
                      rows="3"
                      :value="jsonText[f.key]"
                      @input="jsonText[f.key] = ($event.target as HTMLTextAreaElement).value"
                      @change="applyJson(f.key)"
                    />
                    <span v-if="jsonError[f.key]" class="down" style="font-size: 11px">{{ jsonError[f.key] }}</span>
                  </div>

                  <input
                    v-else
                    v-model="draft[f.key]"
                    class="input"
                    :class="{ mono: f.key === 'base_url' || f.key === 'proxy' || f.key === 'rsshub_base' }"
                  />
                </div>
              </div>
            </div>

            <div v-if="!currentSections.length && current !== 'engine' && current !== 'data' && current !== 'security'" class="empty">
              该分组暂无可编辑字段
            </div>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.settings-layout {
  display: grid;
  grid-template-columns: 208px minmax(0, 1fr);
  gap: 14px;
  align-items: start;
}
.settings-nav {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 8px;
  background: linear-gradient(180deg, rgba(16, 26, 44, 0.92), rgba(9, 15, 26, 0.92));
  border: 1px solid var(--line);
  border-radius: var(--radius);
  position: sticky;
  top: 70px;
}
.nav-item {
  display: flex;
  flex-direction: column;
  gap: 2px;
  width: 100%;
  text-align: left;
  padding: 8px 10px;
  border: 1px solid transparent;
  border-radius: 8px;
  background: transparent;
  color: var(--text-dim);
  cursor: pointer;
  transition: all 0.15s ease;
  font-family: var(--sans);
}
.nav-item:hover {
  background: rgba(59, 130, 246, 0.07);
  color: var(--text);
}
.nav-item.on {
  background: rgba(59, 130, 246, 0.14);
  border-color: rgba(59, 130, 246, 0.35);
  color: var(--text);
}
.nav-name {
  font-size: 13px;
  font-weight: 600;
}
.nav-hint {
  font-size: 10px;
  color: var(--muted);
}
.section {
  display: flex;
  flex-direction: column;
  padding: 2px 0 4px;
}
.section + .section {
  border-top: 1px solid var(--line-soft);
  padding-top: 14px;
}
.section-hd {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
  margin-bottom: 4px;
}
.section-title {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text);
}
.section-hint {
  font-size: 10.5px;
  color: var(--muted);
}
.field-row {
  display: grid;
  grid-template-columns: minmax(0, 1.05fr) minmax(180px, 0.95fr);
  gap: 14px;
  align-items: center;
  padding: 9px 0;
  border-top: 1px dashed rgba(26, 41, 66, 0.6);
}
.field-row:first-of-type {
  border-top: none;
}
.field-meta {
  min-width: 0;
}
.field-name {
  display: flex;
  align-items: baseline;
  gap: 6px;
  font-size: 12.5px;
  color: var(--text);
}
.field-unit {
  font-size: 10px;
  color: var(--muted);
  font-family: var(--mono);
}
.field-hint {
  margin-top: 2px;
  font-size: 10.5px;
  color: var(--muted);
  line-height: 1.55;
}
.field-ctrl {
  min-width: 0;
}
.num-wrap {
  display: flex;
  align-items: center;
  gap: 8px;
}
.num-unit {
  font-size: 10.5px;
  color: var(--muted);
  font-family: var(--mono);
  white-space: nowrap;
}
.seg {
  display: inline-flex;
  padding: 3px;
  gap: 3px;
  background: #070d18;
  border: 1px solid var(--line);
  border-radius: 8px;
  flex-wrap: wrap;
}
.seg-btn {
  padding: 5px 11px;
  border: 1px solid transparent;
  border-radius: 6px;
  background: transparent;
  color: var(--text-dim);
  font-size: 11.5px;
  cursor: pointer;
  font-family: var(--sans);
  transition: all 0.15s ease;
}
.seg-btn:hover {
  color: var(--text);
}
.seg-btn.on {
  background: rgba(59, 130, 246, 0.16);
  border-color: rgba(59, 130, 246, 0.4);
  color: #dbe9ff;
}
.tags-box {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
  padding: 6px 8px;
  min-height: 38px;
  background: #070d18;
  border: 1px solid var(--line);
  border-radius: 7px;
}
.chip {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 2px 7px;
  border-radius: 5px;
  background: rgba(59, 130, 246, 0.12);
  border: 1px solid rgba(59, 130, 246, 0.28);
  color: #b9d1ff;
  font-size: 11px;
  font-family: var(--mono);
}
.chip-x {
  cursor: pointer;
  color: var(--muted);
  font-weight: 400;
}
.chip-x:hover {
  color: var(--down);
}
.chip-input {
  flex: 1;
  min-width: 90px;
  border: none;
  background: transparent;
  color: var(--text);
  font-size: 11.5px;
  outline: none;
  font-family: var(--mono);
}
.cred-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(190px, 1fr));
  gap: 10px;
  margin-bottom: 10px;
}
.env-card {
  padding: 12px;
  border: 1px solid var(--line);
  border-radius: 9px;
  background: rgba(10, 18, 32, 0.55);
}
@media (max-width: 980px) {
  .settings-layout {
    grid-template-columns: 1fr;
  }
  .settings-nav {
    position: static;
    flex-direction: row;
    overflow-x: auto;
  }
  .nav-item {
    width: auto;
    white-space: nowrap;
  }
  .nav-hint {
    display: none;
  }
  .field-row {
    grid-template-columns: 1fr;
    gap: 8px;
  }
}
</style>
