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
import { computed, nextTick, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import { useRoute } from 'vue-router'
import { http } from '../api/client'
import EnvironmentSwitch from '../components/EnvironmentSwitch.vue'
import { useAppStore } from '../stores/app'
import { useAuthStore } from '../stores/auth'

type Widget = 'switch' | 'number' | 'text' | 'seg' | 'select' | 'tags' | 'json' | 'roi'

interface FieldMeta {
  label: string
  hint?: string
  unit?: string
  widget?: Widget
  options?: [string, string][]
  min?: number
  max?: number
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
  { id: 'strategy', label: '策略', hint: '机器实际执行的信号与出场（不是 AI 策略）' },
  { id: 'data', label: '数据源', hint: '行情源、网络与密钥' },
  { id: 'news', label: '资讯', hint: '抓取与情绪' },
  { id: 'evolution', label: '自进化', hint: '复盘与心法' },
  { id: 'backup', label: '备份', hint: '保留与加密' },
  { id: 'security', label: '安全', hint: '登录与会话' },
  { id: 'system', label: '系统', hint: '日志与保留' },
  { id: 'public', label: '公开站点', hint: '前台展示' },
]

//: 常用参数（默认视图）：只保留真正影响交易结果的项，其余收进「显示全部」
const BASIC_FIELDS: Record<string, string[]> = {
  engine: ['cycle_interval_sec', 'max_positions', 'trading_windows', 'order_type', 'close_on_halt'],
  risk: [
    'risk_per_trade_pct', 'margin_pct', 'default_leverage', 'max_leverage',
    'daily_loss_limit_pct', 'max_drawdown_pct', 'consecutive_loss_limit', 'max_spread_bps',
  ],
  strategy: [
    'timeframe', 'entry_threshold', 'volume_ratio_min', 'atr_pct_min', 'atr_pct_max',
    'sl_atr_mult', 'tp_atr_mult', 'trailing_pct', 'max_hold_minutes',
    'breakeven_at_r', 'partial_tp_at_r', 'partial_tp_ratio', 'roi_table',
  ],
  data: ['max_pool_size', 'demo_base_url', 'live_base_url', 'proxy'],
  news: ['enabled', 'refresh_minutes', 'keep_days'],
  evolution: ['enabled', 'max_injected', 'ttl_days', 'min_lesson_sample'],
  backup: ['retention', 'encrypt', 'auto_daily'],
  security: ['require_login', 'session_ttl_hours', 'login_fail_limit'],
  system: ['log_level', 'event_retention_days'],
  public: ['site_enabled', 'show_engine_status', 'show_mantras', 'show_performance'],
}

//: 面向普通用户的白话说明：覆盖原始技术描述（仅影响展示）
const PLAIN_HINTS: Record<string, string> = {
  cycle_interval_sec: '多久扫描一次行情；15 秒以上，间隔越短越耗交易所额度',
  max_symbols_per_cycle: '每轮最多检查几个标的（按标的池顺序）',
  max_positions: '最多同时持有几个仓位',
  risk_per_trade_pct: '每笔交易最多亏掉权益的百分之几；越大仓位越重',
  margin_pct: '单个仓位最多占用多少权益作保证金',
  max_exposure_pct: '所有持仓名义额合计最多是权益的几倍',
  max_same_direction: '同一方向最多同时持有几个仓；0=不限制',
  daily_loss_limit_pct: '当天亏到这个比例就停止开新仓',
  max_drawdown_pct: '权益从最高点回撤到这个比例就停机',
  entry_threshold: '信号分超过多少才开仓；越高越保守',
  volume_ratio_min: '成交量低于该倍数时不参与',
  atr_pct_min: '波动太小（赚不回手续费）时不做',
  atr_pct_max: '波动太大（容易被扫止损）时不做',
  sl_atr_mult: '止损距离 = ATR 的几倍；越小止损越紧',
  tp_atr_mult: '止盈距离 = ATR 的几倍',
  trailing_pct: '从浮盈高点回撤这么多就离场；0=关闭',
  max_hold_minutes: '持仓超过这个时间强制离场',
  breakeven_at_r: '浮盈达到初始止损距离的几倍时，把止损移到开仓价',
  partial_tp_at_r: '浮盈达到初始止损距离的几倍时，先平掉一部分仓位；0=关闭',
  partial_tp_ratio: '部分止盈先平掉几成仓位（0.5 = 平一半）',
  roi_table: '按盈利百分比自动平仓：每档填「持仓分钟 + 价格涨到百分之几就全平」，可增删档位自行配比；清空=关闭',
  max_spread_bps: '买卖价差超过该值就放弃；短线建议 5–12bps',
  min_notional_buffer: '交易所最小下单额的倍数，防止下单被拒',
  fee_rate_taker: '吃单手续费率，0.0005 = 0.05%',
  fee_rate_maker: '挂单手续费率，0.0002 = 0.02%',
  vol_target_atr_pct: '波动越大自动减仓；0=关闭',
}

const BASE_META: Record<string, FieldMeta> = {
  mode: {
    label: '信号模型',
    widget: 'select',
    options: [
      ['trend', 'trend 趋势跟随'],
      ['meanrev', 'meanrev 均值回归'],
    ],
    hint: '实测两者样本外均为负收益（见 docs/EVIDENCE.md），切换前先跑 walk-forward',
  },
  auto_entry: { label: '自动开仓', hint: '关闭后只落库信号供观察，不下单；跟单不受此开关影响' },
  mr_z_entry: { label: '偏离阈值', unit: 'σ', hint: '|z| 达到该值才做回归；越大越少但越极端' },
  mr_min_risk_reward: { label: '回归盈亏比', unit: 'R', hint: '回归模型胜率高但 R 小，用比趋势更低的底线' },
  mr_block_in_trend: { label: '趋势中禁止逆势', hint: '顺势的极值通常是继续走，不是回归' },
  mr_trend_gap_pct: { label: '强趋势判定', unit: '%', hint: '|EMA 间距| 超过该值视为趋势，不做逆势回归' },
  mr_window: { label: '回归窗口', unit: '根', hint: '计算中轨与标准差所用的 K 线根数' },
  htf_timeframe: {
    label: '高周期过滤', widget: 'select',
    options: [['', '关闭'], ['15m', '15m'], ['30m', '30m'], ['1h', '1h'], ['4h', '4h']],
    hint: '非空时只做与高周期趋势同向的信号；开启会每标的每轮多一次 REST',
  },
  adx_filter: { label: '趋势强度过滤', hint: 'ADX 低于阈值不放行，过滤震荡市假突破' },
  adx_period: { label: 'ADX 周期', unit: '根' },
  adx_min: { label: 'ADX 下限', unit: '', hint: '低于该值视为无趋势；25 以上才有明显趋势' },
  liquidation_warn_pct: { label: '强平价告警', unit: '%', hint: '标记价距强平价小于该百分比就告警；0=关闭' },
  category_exposure_limit_pct: { label: '相关性组敞口', unit: '%', hint: 'BTC/ETH/主流/山寨 各组名义额占权益上限；山寨彼此高度相关，0=关闭' },
  auto_protect_external: {
    label: '自动补挂保护单',
    hint: '发现没有止损/止盈的外部（手动）仓位时，自动按 ATR 挂保护单；默认关，先告警由人决定更稳',
  },
  enabled: { label: '启用', hint: '关闭后该模块不参与运行' },
  cycle_interval_sec: {
    label: '轮询间隔',
    unit: '秒',
    hint: '最小 15 秒；策略跑 15m K 线，更快只会增加 REST 权重与信号表写入，不会更灵敏',
    min: 15,
  },
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
  default_leverage: {
    label: '杠杆下限', unit: 'x', min: 1,
    hint: '最低 1x；全局硬下限，AI 与外部信号给出的杠杆低于下限时自动抬到下限',
  },
  max_leverage: {
    label: '杠杆上限', unit: 'x', max: 100,
    hint: '最高 100x；全局硬上限，AI 开仓杠杆与单标的配置都只能收紧不能突破',
  },
  leverage_min: {
    label: '策略杠杆下限', unit: 'x', min: 0,
    hint: '策略自主开仓的目标杠杆下限，独立于风控分组的全局杠杆下限；0=沿用全局',
  },
  leverage_max: {
    label: '策略杠杆上限', unit: 'x', min: 0,
    hint: '策略自主开仓的目标杠杆上限，仍受全局硬上限与交易所上限约束；0=沿用全局',
  },
  max_exposure_pct: { label: '总敞口上限', unit: '%', hint: '所有持仓名义额 / 权益' },
  max_same_direction: {
    label: '同向持仓上限',
    unit: '个',
    hint: '0=不限制；币种高度相关，小资金建议设 1，避免同涨同跌时吃双份风险',
  },
  vol_target_atr_pct: {
    label: '波动率目标',
    unit: '%',
    hint: '如 1.5：ATR% 高于它自动缩仓、低于它放大（0.25–2x）；0=关闭',
  },
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
      ['15m', '15m'],
      ['30m', '30m'],
      ['1h', '1h'],
    ],
    hint: '15–60 分钟短线主周期',
  },
  ema_fast: { label: '快线 EMA' },
  ema_slow: { label: '慢线 EMA' },
  rsi_period: { label: 'RSI 周期' },
  atr_period: { label: 'ATR 周期' },
  entry_threshold: { label: '开仓信号阈值', hint: '综合分绝对值高于该值才考虑开仓' },
  exit_threshold: { label: '反转平仓阈值' },
  volume_ratio_min: { label: '最小量比', hint: '低于该值的信号被过滤' },
  atr_pct_min: { label: 'ATR 下限', unit: '%', hint: '波动太小不足以覆盖手续费' },
  atr_pct_max: { label: 'ATR 上限', unit: '%', hint: '波动过大不参与' },
  min_24h_quote_volume: { label: '24H 最小成交额', unit: 'USDT' },
  sl_atr_mult: { label: '止损 = ATR ×' },
  tp_atr_mult: { label: '止盈 = ATR ×' },
  trailing_pct: { label: '移动止损回撤', unit: '%', hint: '0 表示关闭' },
  trailing_arm_r: { label: '移动止损激活', unit: 'R', hint: '浮盈先走到 N 倍风险才让移动止损接管，否则会在止损前被噪声扫掉（实盘 24 笔全亏的根因）' },
  breakeven_at_r: { label: '保本触发', unit: 'R', hint: '浮盈达到该 R 倍数后止损上移到开仓价' },
  max_hold_minutes: { label: '最长持仓', unit: '分钟', hint: '超时强制离场' },
  partial_tp_at_r: { label: '部分止盈触发', unit: 'R', hint: '浮盈达到该 R 倍数先平一部分；0=关闭' },
  partial_tp_ratio: { label: '部分止盈比例', unit: '比例', hint: '0.5 = 平一半，剩余交给移动止损' },
  max_spread_bps: { label: '盘口点差上限', unit: 'bps', hint: '买一卖一价差超过该值就放弃开仓；0=关闭。短线建议 5–12bps' },
  roi_table: {
    label: '时间阶梯止盈',
    widget: 'roi',
    hint: '按盈利百分比自动平仓：每档填「持仓分钟 + 价格涨到百分之几就全平」，可增删档位自行配比',
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
  exec_mode: {
    label: '执行内核模式',
    widget: 'seg',
    hint: 'off 只用 Python 签名；shadow 旁路比对只统计；enforce 不一致直接拒绝下单',
    options: [
      ['off', 'OFF'],
      ['shadow', 'SHADOW'],
      ['enforce', 'ENFORCE'],
    ],
  },
  exec_socket: { label: 'sidecar 地址', hint: 'unix:/run/nanoquant/exec.sock 或 tcp:127.0.0.1:9101；留空读 EXEC_SOCKET' },
  exec_sample_pct: { label: '比对抽样', unit: '%', hint: '下单高峰时可只比对一部分，enforce 模式建议 100' },
  exec_timeout_ms: { label: '比对超时', unit: 'ms', hint: '超时视为 sidecar 不可用并降级为 Python 签名' },
  api_rate_limit_per_min: {
    label: 'API 限频',
    unit: '次/分/IP',
    hint: '0=关闭；防爆破与刷接口，健康检查与 /metrics 不计数',
  },
  metrics_enabled: {
    label: 'Prometheus 指标',
    hint: '开放只读 /metrics（仅队列、死信、比对计数与状态位，无账户数据）',
  },

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
    {
      title: '仓位与杠杆',
      hint: '先由风险预算反推名义额，再被保证金、敞口与同向集中度截断；AI 开仓杠杆统一按下面的下限/上限钳制，不跟随心法',
      fields: [
        'risk_per_trade_pct',
        'margin_pct',
        'default_leverage',
        'max_leverage',
        'max_exposure_pct',
        'max_same_direction',
        'vol_target_atr_pct',
      ],
    },
    { title: '熔断与冷却', hint: '触发后只锁新开仓', fields: ['daily_loss_limit_pct', 'max_drawdown_pct', 'consecutive_loss_limit', 'cooldown_minutes', 'symbol_cooldown_minutes', 'min_equity_usdt'] },
    { title: '止损熔断', hint: '窗口内多次止损 = 策略与行情脱节，先暂停观察', fields: ['stoploss_guard_count', 'stoploss_guard_window_min', 'stoploss_guard_pause_min'] },
    { title: '尾部风险', hint: '强平价监控 + 相关性组敞口（山寨多仓本质是加杠杆）+ 手动仓保护单', fields: ['liquidation_warn_pct', 'category_exposure_limit_pct', 'auto_protect_external'] },
    { title: '撮合与手续费', fields: ['max_slippage_pct', 'min_notional_buffer', 'fee_rate_taker', 'fee_rate_maker'] },
  ],
  strategy: [
    {
      title: '信号模型',
      hint: 'trend=均线动量趋势跟随；meanrev=布林极值回归。auto_entry 关闭时只落库信号、不下单（与引擎总闸不同，跟单不受影响）',
      fields: ['mode', 'auto_entry', 'timeframe', 'htf_timeframe'],
    },
    {
      title: '策略杠杆区间',
      hint: '只约束策略自主开仓，不碰风控分组的全局杠杆上下限；0=沿用全局',
      fields: ['leverage_min', 'leverage_max'],
    },
    { title: '周期与指标', fields: ['ema_fast', 'ema_slow', 'rsi_period', 'atr_period'] },
    {
      title: '趋势强度',
      hint: 'ADX 低于下限不放行：短线趋势跟随的亏损主要来自震荡市假突破',
      fields: ['adx_filter', 'adx_period', 'adx_min'],
    },
    { title: '开仓过滤', hint: '任一项不达标都不开仓', fields: ['entry_threshold', 'exit_threshold', 'volume_ratio_min', 'atr_pct_min', 'atr_pct_max', 'min_24h_quote_volume'] },
    {
      title: '出场规则',
      hint: 'min_risk_reward 是止盈/止损的硬底线：2.0 意味着只需 33% 胜率即可正期望；1.1 则要 48% 以上，扣手续费基本是负期望',
      fields: ['sl_atr_mult', 'tp_atr_mult', 'min_risk_reward', 'trailing_pct', 'trailing_arm_r', 'breakeven_at_r', 'max_hold_minutes'],
    },
    {
      title: '体制自适应',
      hint: '震荡市入场时提前释放资金；RSI 按趋势体制分区解读（上涨趋势里 65 是健康、80 才是过热）',
      fields: ['max_hold_minutes_chop', 'chop_ema_gap_pct', 'rsi_adaptive'],
    },
    { title: '部分止盈', hint: '先落袋一部分，剩余仓位用移动止损奔跑', fields: ['partial_tp_at_r', 'partial_tp_ratio'] },
    {
      title: '短线执行',
      hint: '进场先扣点差，出场按持仓时长阶梯止盈：这两项决定 5–30 分钟级别的净收益',
      fields: ['max_spread_bps', 'roi_table', 'roi_min_r'],
    },
    { title: '因子权重', hint: '加权求和后映射到 [-1, 1]；缺失因子会被剔除并把权重重新归一', fields: ['weight_trend', 'weight_momentum', 'weight_volume', 'weight_funding', 'weight_oi'] },
    {
      title: '均值回归（mode=meanrev 时生效）',
      hint: '|z| 不到阈值不做；强趋势里禁止逆势；放量突破打折。回归模型胜率高但 R 小，所以用更低的盈亏比底线',
      fields: ['mr_window', 'mr_z_entry', 'mr_min_risk_reward', 'mr_block_in_trend', 'mr_trend_gap_pct'],
    },
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
    { title: '运行与日志', fields: ['log_level', 'ui_refresh_sec', 'timezone_offset'] },
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
    {
      title: '执行内核（sidecar）',
      hint: 'Rust sidecar 只签名不下单：shadow 旁路比对不影响下单，enforce 不一致直接拒绝下单；sidecar 不可用时自动降级为 Python 签名',
      fields: ['exec_mode', 'exec_socket', 'exec_sample_pct', 'exec_timeout_ms'],
    },
    {
      title: '安全与观测',
      hint: 'API 限频按 IP 计，健康检查与 /metrics 不计数；指标仅含队列、死信与比对计数',
      fields: ['api_rate_limit_per_min', 'metrics_enabled'],
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
  const base = GROUP_META[group]?.[key] || BASE_META[key] || { label: key }
  const plain = PLAIN_HINTS[key]
  return plain ? { ...base, hint: plain } : base
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
  return { key, value, widget: widgetOf(value, meta), label: meta.label, hint: meta.hint || '', unit: meta.unit || '', options: meta.options, min: meta.min, max: meta.max }
}

/** 默认只看「常用参数」，一键展开全部高级项，降低第一次上手的理解成本 */
const showAll = ref(false)

function skippedKeys(): Set<string> {
  return new Set([...PROTECTED, ...(SKIP[current.value] || []), 'symbols'])
}

function allowField(key: string): boolean {
  if (showAll.value) return true
  const basic = BASIC_FIELDS[current.value]
  return !basic || basic.includes(key)
}

const hiddenCount = computed(() => {
  const basic = BASIC_FIELDS[current.value]
  if (showAll.value || !basic) return 0
  const skip = skippedKeys()
  return Object.keys(draft.value || {}).filter((k) => !skip.has(k) && !basic.includes(k)).length
})

// ------------------------------------------------------------ 参数检索
/**
 * 参数分散在 10 个分组、上百个字段里，先让用户猜"在哪一组"是最大的摩擦。
 * 这里做三件事：
 * 1) 跨全部分组的关键词检索（同时搜中文名、字段名、说明、分区名），点击直接跳转并高亮；
 * 2) 「常用直达」——最高频的十来个参数一键定位，不必记分组；
 * 3) 「只看已改」——保存前确认到底动了哪几项。
 */
const query = ref('')
const hitsOpen = ref(false)
const focusKey = ref('')
const onlyDirty = ref(false)
const searchInput = ref<HTMLInputElement | null>(null)

interface Hit {
  group: string
  groupLabel: string
  section: string
  key: string
  label: string
  hint: string
  value: any
}

/** 常用直达：出现频率最高的参数，跨分组一键定位 */
const QUICK_LINKS: { label: string; group: string; key: string }[] = [
  { label: '扫描间隔', group: 'engine', key: 'cycle_interval_sec' },
  { label: '下单方式', group: 'engine', key: 'order_type' },
  { label: '最多持仓', group: 'engine', key: 'max_positions' },
  { label: '信号模式', group: 'strategy', key: 'mode' },
  { label: 'K 线周期', group: 'strategy', key: 'timeframe' },
  { label: '杠杆区间', group: 'strategy', key: 'leverage_min' },
  { label: '止损止盈', group: 'strategy', key: 'sl_atr_mult' },
  { label: '点差闸门', group: 'strategy', key: 'max_spread_bps' },
  { label: '单笔风险', group: 'risk', key: 'risk_per_trade_pct' },
  { label: '回撤熔断', group: 'risk', key: 'max_drawdown_pct' },
  { label: '日亏上限', group: 'risk', key: 'daily_loss_limit_pct' },
  { label: '手续费率', group: 'risk', key: 'fee_rate_taker' },
]

function sectionMapOf(group: string): Map<string, string> {
  const m = new Map<string, string>()
  for (const def of SECTIONS[group] || []) for (const k of def.fields) m.set(k, def.title)
  return m
}

function skipOf(group: string): Set<string> {
  return new Set([...PROTECTED, ...(SKIP[group] || []), 'symbols'])
}

/** 每个分组有多少可编辑参数（导航上显示，避免"点进去才知有没有"） */
const groupCounts = computed<Record<string, number>>(() => {
  const out: Record<string, number> = {}
  for (const g of groups) {
    const cfg = (app.settings as Record<string, any>)[g.id] || {}
    const skip = skipOf(g.id)
    out[g.id] = Object.keys(cfg).filter((k) => !skip.has(k)).length
  }
  return out
})

const searchHits = computed<Hit[]>(() => {
  const q = query.value.trim().toLowerCase()
  if (!q) return []
  const out: Hit[] = []
  for (const g of groups) {
    const cfg = (app.settings as Record<string, any>)[g.id] || {}
    const skip = skipOf(g.id)
    const secs = sectionMapOf(g.id)
    for (const [key, value] of Object.entries(cfg)) {
      if (skip.has(key)) continue
      const meta = metaOf(g.id, key)
      const hay = [meta.label, key, meta.hint || '', PLAIN_HINTS[key] || '', secs.get(key) || '']
        .join(' ')
        .toLowerCase()
      if (!hay.includes(q)) continue
      out.push({
        group: g.id,
        groupLabel: g.label,
        section: secs.get(key) || '其他参数',
        key,
        label: meta.label,
        hint: meta.hint || PLAIN_HINTS[key] || '',
        value,
      })
    }
  }
  return out.slice(0, 30)
})

function preview(v: any): string {
  if (typeof v === 'boolean') return v ? '开' : '关'
  if (Array.isArray(v)) return v.length ? `${v.length} 项` : '空'
  if (v && typeof v === 'object') return '…'
  const s = String(v ?? '')
  return s.length > 18 ? `${s.slice(0, 18)}…` : s
}

function clearSearch() {
  query.value = ''
  hitsOpen.value = false
}

/** 跳到某个参数：必要时切分组、展开高级项、滚动到视野中央并闪一下 */
async function jumpTo(group: string, key: string) {
  // 首屏还在拉配置时点「常用直达」会没有任何反应：先等配置到位再跳
  if (!Object.keys(app.settings || {}).length) await app.loadSettings()
  if (group !== current.value) {
    if (dirty.value && !confirm('当前分组有未保存的修改，切换后会被丢弃，继续？')) return
    loadGroup(group)
  }
  showAll.value = true
  onlyDirty.value = false
  clearSearch()
  focusKey.value = key
  await nextTick()
  const el = document.getElementById(`fld-${group}-${key}`)
  if (!el) return
  el.scrollIntoView({ block: 'center', behavior: 'smooth' })
  window.setTimeout(() => {
    if (focusKey.value === key) focusKey.value = ''
  }, 2200)
}

/** 当前分组里相对"已保存值"有哪些改动 */
const changedKeys = computed(() => {
  const skip = skippedKeys()
  return Object.keys(draft.value || {}).filter(
    (k) => !skip.has(k) && JSON.stringify(draft.value[k]) !== JSON.stringify((baseline.value as any)[k]),
  )
})

function onHotkey(e: KeyboardEvent) {
  const tag = (e.target as HTMLElement)?.tagName
  const typing = tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT'
  if ((e.key === '/' && !typing) || ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k')) {
    e.preventDefault()
    searchInput.value?.focus()
    hitsOpen.value = true
  }
  if (e.key === 'Escape') clearSearch()
}

/** 只看改动：长表单保存前核对到底动了哪几项（顺手展开高级项，避免改动项被折叠看不见） */
function toggleOnlyDirty() {
  onlyDirty.value = !onlyDirty.value
  if (onlyDirty.value) showAll.value = true
}

const currentSections = computed(() => {
  const skip = skippedKeys()
  const keep = onlyDirty.value ? new Set(changedKeys.value) : null
  const rows = new Map<string, Row>()
  for (const [key, value] of Object.entries(draft.value || {})) {
    if (skip.has(key) || !allowField(key)) continue
    if (keep && !keep.has(key)) continue
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
    if (value && typeof value === 'object' && metaOf(current.value, key).widget !== 'roi') {
      jsonText[key] = JSON.stringify(value, null, 2)
      jsonError[key] = ''
    }
  }
}

// ---------------------------------------------------- 交易模式（双向逐仓）
const tradeMode = ref<any>(null)
const tradeModeBusy = ref(false)
const tradeModeResults = ref<any[]>([])
const tradeModeError = ref('')

const tradeModeLabel = computed(() => {
  if (tradeModeBusy.value) return '切换中…'
  const m = tradeMode.value
  if (!m) return '读取中…'
  if (m.hedge_mode && !m.need_isolated?.length) return '已是双向逐仓'
  if (!m.hedge_mode && !m.need_isolated?.length) return '一键开启双向持仓'
  return '一键切换为双向逐仓'
})

async function loadTradeMode() {
  try {
    tradeMode.value = await http.get('/engine/trade-mode', 8000)
    tradeModeError.value = ''
  } catch (err: any) {
    tradeMode.value = null
    tradeModeError.value = err?.message || '交易模式读取失败'
  }
}

async function fixTradeMode() {
  const m = tradeMode.value
  if (!m) return
  // 只发送真正需要变更的项：网络差时避免把 8 个标的的逐仓请求也一起打出去
  const payload = {
    hedge: !m.hedge_mode,
    isolated: Boolean(m.need_isolated?.length),
  }
  if (!payload.hedge && !payload.isolated) return
  if (!confirm('将账户切换为「双向持仓 + 逐仓」？有持仓或挂单时交易所会拒绝，需要先全部平仓。')) return
  tradeModeBusy.value = true
  try {
    const res = await http.post('/engine/trade-mode', payload, 15000)
    tradeModeResults.value = res.results || []
    const firstFail = (res.results || []).find((r: any) => !r.ok)
    app.notify(
      res.ok ? '切换完成' : firstFail?.detail || '切换未完成，请查看下方原因',
      res.ok ? 'ok' : 'error',
    )
    await loadTradeMode() // 不再顺带刷新全量状态，避免二次 REST 卡顿
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    tradeModeBusy.value = false
  }
}

function loadGroup(id: string) {
  current.value = id
  draft.value = clone(app.settings[id] || {})
  baseline.value = clone(app.settings[id] || {})
  onlyDirty.value = false
  focusKey.value = ''
  syncJsonText()
  if (id === 'engine') loadTradeMode()
  if (id === 'risk') loadUnprotected()
  loadAudit()
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
    onlyDirty.value = false
    app.notify('配置已保存并热生效', 'ok')
    if (current.value === 'engine' || current.value === 'data') await app.loadStatus()
    await loadAudit()
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
    onlyDirty.value = false
    if (current.value === 'engine' || current.value === 'data') await app.loadStatus()
    app.notify('已恢复默认值', 'ok')
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}

/** 放弃本次改动：退回上次保存的值，不用重新加载页面 */
function discard() {
  draft.value = clone(baseline.value)
  syncJsonText()
  onlyDirty.value = false
  app.notify('已放弃未保存的修改', 'ok')
}

// ---------------------------------------------------- 日亏基准重置
/**
 * 充值 / 重置模拟盘资金 / 换账户之后必须做一次：旧基准还在的时候，系统会拿新
 * 资金和旧基准比，算出 -99% 的假日内亏损并熔断（这个坑已经踩过两次）。
 */
const anchorBusy = ref(false)

async function resetAnchors() {
  if (!confirm('把日亏基准与回撤峰值重置为当前权益？\n只在充值、重置模拟盘资金或换账户后使用。')) return
  anchorBusy.value = true
  try {
    const res = await http.post('/engine/reset-anchors')
    await app.loadStatus()
    app.notify(
      res.cleared_halt ? `基准已对齐到 ${res.equity}；已解除「${res.cleared_halt}」` : `基准已对齐到 ${res.equity}`,
      'ok',
    )
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    anchorBusy.value = false
  }
}

/** 当前分组的最近变更（谁改的、改前改后） */
const auditRows = ref<any[]>([])

async function loadAudit() {
  try {
    const res = await http.get(`/settings/audit?group=${current.value}&limit=6`)
    auditRows.value = res.items || []
  } catch {
    auditRows.value = []
  }
}

function tsText(ts: number): string {
  if (!ts) return '-'
  const d = new Date(ts * 1000)
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

function fieldLabel(key: string): string {
  return metaOf(current.value, key).label || key
}

function shortValue(v: any): string {
  if (v === null || v === undefined || v === '') return '空'
  if (typeof v === 'boolean') return v ? '开' : '关'
  if (Array.isArray(v)) return v.length ? `${v.length} 项` : '空'
  if (typeof v === 'object') return '…'
  const s = String(v)
  return s.length > 14 ? `${s.slice(0, 14)}…` : s
}

// ---------------------------------------------------- 无保护单仓位
/** 手动开的仓常常没挂止损，系统只能显示 sl=0。这里列出并支持一键按 ATR 补挂。 */
const unprotected = ref<{ id: number; symbol: string }[]>([])
const protectBusy = ref(false)

async function loadUnprotected() {
  try {
    const res = await http.get('/engine/positions?limit=50')
    unprotected.value = (res.items || [])
      .filter((p: any) => !p.sl_price && !p.tp_price)
      .map((p: any) => ({ id: p.id, symbol: p.symbol }))
  } catch {
    unprotected.value = []
  }
}

async function protectAll() {
  if (!unprotected.value.length) return
  if (!confirm(`按 ATR 给 ${unprotected.value.length} 个无保护单仓位补挂止损/止盈？`)) return
  protectBusy.value = true
  let ok = 0
  let fail = 0
  for (const p of unprotected.value) {
    try {
      await http.post(`/engine/positions/${p.id}/protect`)
      ok += 1
    } catch {
      fail += 1
    }
  }
  protectBusy.value = false
  await loadUnprotected()
  await app.loadStatus()
  app.notify(`已补挂 ${ok} 个${fail ? `，失败 ${fail} 个` : ''}`, fail ? 'error' : 'ok')
}

// ---------------------------------------------------- 紧急全平
/**
 * 事故处置只该有一个动作：停引擎 + 撤挂单 + 市价平掉当前环境全部仓位。
 * 后端要求确认短语 FLATTEN（与切实盘的 LIVE 同款约定），避免误触清仓。
 */
const flattenBusy = ref(false)

async function flattenAll() {
  const phrase = prompt(
    '紧急全平：将停止引擎、撤销全部挂单，并以市价平掉当前交易环境的全部仓位。\n\n输入 FLATTEN 确认：',
  )
  if (phrase === null) return
  if (phrase.trim().toUpperCase() !== 'FLATTEN') {
    app.notify('确认短语不正确，已取消', 'error')
    return
  }
  flattenBusy.value = true
  try {
    const res = await http.post('/engine/flatten', { confirm: 'FLATTEN', stop_engine: true })
    await app.loadStatus()
    const closed = (res.closed || []).length
    const failed = (res.failed || []).length
    app.notify(
      failed ? `已平 ${closed} 笔，${failed} 笔失败，请手动处理` : `已平 ${closed} 笔，引擎已停止`,
      failed ? 'error' : 'ok',
    )
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    flattenBusy.value = false
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

/** 阶梯止盈：两列档位编辑器，保存时仍是 [[分钟, 收益率%], ...] */
function roiRows(key: string): number[][] {
  const raw = draft.value[key]
  return Array.isArray(raw) ? raw.map((r: any) => [Number(r?.[0] ?? 0), Number(r?.[1] ?? 0)]) : []
}

function setRoiCell(key: string, idx: number, col: 0 | 1, event: Event) {
  const rows = draft.value[key]
  if (!Array.isArray(rows) || !rows[idx]) return
  const num = Number((event.target as HTMLInputElement).value)
  const next = Array.isArray(rows[idx]) ? [...rows[idx]] : [0, 0]
  next[col] = Number.isFinite(num) ? num : 0
  rows[idx] = next // 只替换当前行，输入过程中不重建整表，避免输入框失焦
}

function addRoiTier(key: string) {
  const rows = roiRows(key)
  if (rows.length >= 12) return
  const last = rows[rows.length - 1]
  rows.push([
    last ? Math.min(2880, Number(last[0]) + 15) : 0,
    last ? Number(Math.max(0.05, Number(last[1]) * 0.75).toFixed(2)) : 0.8,
  ])
  draft.value[key] = rows
}

function removeRoiTier(key: string, idx: number) {
  const rows = roiRows(key)
  rows.splice(idx, 1)
  draft.value[key] = rows
}

function clearRoiTiers(key: string) {
  draft.value[key] = []
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
  window.addEventListener('keydown', onHotkey)
  if (!Object.keys(app.settings).length) await app.loadSettings()
  await app.loadStatus()
  const wanted = String(route.query.group || '')
  const group = groups.some((g) => g.id === wanted) ? wanted : 'engine'
  loadGroup(group)
  await loadCredentials()
  // 支持 ?group=risk&key=max_drawdown_pct 直达某个参数（文档与提示可以精确指路）
  const key = String(route.query.key || '')
  if (key) await jumpTo(group, key)
})

onUnmounted(() => window.removeEventListener('keydown', onHotkey))
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
          <button class="btn btn-sm btn-danger" :disabled="flattenBusy" @click="flattenAll" title="停引擎 + 撤单 + 市价平掉全部仓位">
            {{ flattenBusy ? '全平中…' : '紧急全平' }}
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

    <!-- 参数检索：跨分组搜索 + 常用直达。参数上百个，先解决"找不到" -->
    <div class="panel">
      <div class="panel-body stack" style="gap: 10px">
        <div class="search-row">
          <input
            ref="searchInput"
            v-model="query"
            class="input search-input"
            placeholder="搜索参数：杠杆 / 止损 / 周期 / 手续费 / 熔断 …"
            @focus="hitsOpen = true"
            @input="hitsOpen = true"
            @keyup.enter="searchHits.length && jumpTo(searchHits[0].group, searchHits[0].key)"
          />
          <span class="search-kbd" title="按 / 或 Ctrl+K 聚焦搜索">/</span>
          <button v-if="query" class="btn btn-sm btn-ghost" @click="clearSearch">清空</button>
        </div>

        <div class="quick-row">
          <span class="quick-label">常用直达</span>
          <button
            v-for="q in QUICK_LINKS"
            :key="q.group + q.key"
            type="button"
            class="chip-btn"
            @click="jumpTo(q.group, q.key)"
          >
            {{ q.label }}
          </button>
        </div>

        <div v-if="query && hitsOpen" class="search-results">
          <div v-if="!searchHits.length" class="search-empty">
            没有匹配「{{ query }}」的参数。试试：杠杆 / 止损 / 周期 / 手续费 / 熔断
          </div>
          <button
            v-for="h in searchHits"
            :key="h.group + h.key"
            type="button"
            class="search-hit"
            @click="jumpTo(h.group, h.key)"
          >
            <span class="hit-where">{{ h.groupLabel }} · {{ h.section }}</span>
            <span class="hit-label">{{ h.label }}</span>
            <span class="hit-hint">{{ h.hint }}</span>
            <span class="hit-value">{{ preview(h.value) }}</span>
          </button>
          <div v-if="searchHits.length" class="search-foot">
            {{ searchHits.length }} 个结果 · 点击跳到该参数并高亮（Esc 关闭）
          </div>
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
          <span class="nav-name">
            {{ g.label }}
            <span class="nav-count">{{ groupCounts[g.id] ?? 0 }}</span>
          </span>
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
            <div class="right" style="flex-wrap: wrap">
              <span v-if="!showAll && hiddenCount" class="tag tag-dim">已隐藏 {{ hiddenCount }} 项高级参数</span>
              <button class="btn btn-sm btn-ghost" @click="showAll = !showAll">
                {{ showAll ? '只看常用' : '显示全部参数' }}
              </button>
              <button
                v-if="changedKeys.length"
                class="btn btn-sm btn-ghost"
                :class="{ 'btn-primary': onlyDirty }"
                :title="onlyDirty ? '显示本组全部参数' : '只显示本组改动过的参数'"
                @click="toggleOnlyDirty"
              >
                {{ onlyDirty ? '显示全部' : `只看改动 ${changedKeys.length}` }}
              </button>
              <span v-if="dirty" class="tag tag-warn">未保存</span>
              <button class="btn btn-sm" @click="reset">恢复默认</button>
              <button class="btn btn-sm btn-primary" :disabled="saving" @click="save">
                {{ saving ? '保存中…' : '保存并生效' }}
              </button>
            </div>
          </div>

          <div class="panel-body stack" style="gap: 16px">
            <!-- 引擎: 交易模式（双向持仓 + 逐仓） -->
            <div v-if="current === 'engine'" class="section">
              <div class="section-hd">
                <span class="section-title">交易模式</span>
                <span class="section-hint">系统按「双向持仓 + 逐仓」设计；切换需无持仓/挂单，各标的逐仓由开仓前自动保证</span>
              </div>
              <div v-if="tradeMode" class="trade-mode">
                <div class="tm-row">
                  <span class="tm-label">持仓模式</span>
                  <span class="tag" :class="tradeMode.hedge_mode ? 'tag-long' : 'tag-warn'">
                    {{ tradeMode.hedge_mode ? '双向持仓' : '单向持仓 · 建议切换' }}
                  </span>
                  <span class="muted tm-note">
                    {{ tradeMode.hedge_mode ? '多空可同时持有，符合系统设计' : '单向模式下仍可下单，但不会同时持有多空' }}
                  </span>
                </div>
                <div class="tm-row">
                  <span class="tm-label">保证金模式</span>
                  <span class="tag" :class="tradeMode.need_isolated?.length ? 'tag-warn' : 'tag-long'">
                    {{
                      tradeMode.need_isolated?.length
                        ? `${tradeMode.need_isolated.length} 个标的为全仓`
                        : tradeMode.margin_known
                          ? '全部逐仓'
                          : '开仓时自动逐仓'
                    }}
                  </span>
                  <span class="muted tm-note">
                    {{
                      tradeMode.need_isolated?.length
                        ? tradeMode.need_isolated.slice(0, 5).join('、') + (tradeMode.need_isolated.length > 5 ? '…' : '')
                        : '逐仓隔离单标的风险'
                    }}
                  </span>
                </div>
                <div class="tm-hint">{{ tradeMode.hint }}</div>
                <div class="row" style="gap: 8px; flex-wrap: wrap">
                  <button
                    class="btn btn-sm btn-primary"
                    :disabled="tradeModeBusy || (tradeMode.hedge_mode && !tradeMode.need_isolated?.length)"
                    @click="fixTradeMode"
                  >
                    {{ tradeModeLabel }}
                  </button>
                  <button class="btn btn-sm" :disabled="tradeModeBusy" @click="loadTradeMode">刷新</button>
                  <span v-if="!tradeMode.can_switch" class="tag tag-warn">有持仓，切换前需先平仓</span>
                </div>
                <div v-if="tradeModeResults.length" class="stack" style="gap: 6px; margin-top: 8px">
                  <div v-for="r in tradeModeResults" :key="r.target" class="stack" style="gap: 2px">
                    <div class="row" style="font-size: 11.5px">
                      <span class="tag" :class="r.ok ? 'tag-long' : 'tag-short'">{{ r.ok ? '成功' : '部分失败' }}</span>
                      <span class="muted">{{ r.target === 'hedge' ? '双向持仓' : '逐仓' }} · {{ r.detail }}</span>
                    </div>
                    <div
                      v-for="f in r.failed || []"
                      :key="f.symbol"
                      class="muted"
                      style="font-size: 10.5px; padding-left: 4px"
                    >
                      {{ f.symbol }} · {{ f.detail }}
                    </div>
                  </div>
                </div>
              </div>
              <div v-else class="empty" style="padding: 14px">
                {{ tradeModeError || '正在读取账户交易模式…' }}
              </div>
            </div>

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

            <div v-if="current === 'strategy'" class="note">
              这里是<b>机器真正执行的策略参数</b>：模式、周期、杠杆区间、准入闸门、止损止盈，改完直接影响下单。
              想改「AI 怎么分析」的文本指令，去
              <router-link to="/admin/strategies">AI 策略</router-link>。
            </div>

            <!-- 风控: 日亏基准（充值/重置资金后必须对齐，否则会算出假 -99%） -->
            <div v-if="current === 'risk'" class="section">
              <div class="section-hd">
                <span class="section-title">日亏基准</span>
                <span class="section-hint">
                  日亏与回撤都相对「当日初始权益」计算；充值、重置模拟盘资金、换账户后必须对齐一次
                </span>
              </div>
              <div class="row" style="gap: 10px; flex-wrap: wrap; align-items: center">
                <span class="tag tag-dim">
                  当前权益 {{ (app.status?.account?.equity ?? 0).toFixed(2) }}
                </span>
                <span v-if="app.status?.halted" class="tag tag-warn" :title="app.status.halted">
                  熔断中 · {{ app.status.halted.slice(0, 24) }}
                </span>
                <span v-else class="tag tag-long">未熔断</span>
                <button class="btn btn-sm" :disabled="anchorBusy" @click="resetAnchors">
                  {{ anchorBusy ? '重置中…' : '重置基准到当前权益' }}
                </button>
              </div>
              <div v-if="unprotected.length" class="row" style="gap: 10px; flex-wrap: wrap; align-items: center; margin-top: 8px">
                <span class="tag tag-warn">
                  {{ unprotected.length }} 个仓位没有止损/止盈：{{ unprotected.map((p) => p.symbol).slice(0, 4).join('、') }}
                </span>
                <button class="btn btn-sm" :disabled="protectBusy" @click="protectAll">
                  {{ protectBusy ? '补挂中…' : '按 ATR 补挂保护单' }}
                </button>
                <span class="muted" style="font-size: 10.5px">手动开的仓不走系统下单链路，需要补挂后才受托管</span>
              </div>
            </div>

            <div v-for="sec in currentSections" :key="sec.title" class="section">
              <div class="section-hd">
                <span class="section-title">{{ sec.title }}</span>
                <span v-if="sec.hint" class="section-hint">{{ sec.hint }}</span>
              </div>

              <div
                v-for="f in sec.rows"
                :key="f.key"
                :id="`fld-${current}-${f.key}`"
                class="field-row"
                :class="{ focus: focusKey === f.key, changed: changedKeys.includes(f.key) }"
              >
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
                    <input v-model.number="draft[f.key]" type="number" step="any" :min="f.min" :max="f.max" class="input mono" />
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

                  <div v-else-if="f.widget === 'roi'" class="roi-editor">
                    <div class="roi-head">
                      <span>持仓分钟</span>
                      <span>盈利达 % 自动平仓</span>
                      <span></span>
                    </div>
                    <div v-for="(tier, idx) in draft[f.key] || []" :key="idx" class="roi-row">
                      <input
                        type="number" min="0" max="2880" step="1" class="input mono"
                        :value="tier[0]"
                        @input="setRoiCell(f.key, idx, 0, $event)"
                      />
                      <input
                        type="number" min="0" max="50" step="0.05" class="input mono"
                        :value="tier[1]"
                        @input="setRoiCell(f.key, idx, 1, $event)"
                      />
                      <button class="btn btn-xs btn-danger" title="删除该档" @click="removeRoiTier(f.key, idx)">✕</button>
                    </div>
                    <div v-if="!(draft[f.key] || []).length" class="hint">没有档位 = 关闭按盈利百分比自动平仓</div>
                    <div class="row" style="gap: 8px; margin-top: 6px; flex-wrap: wrap">
                      <button
                        class="btn btn-xs"
                        :disabled="(draft[f.key] || []).length >= 12"
                        @click="addRoiTier(f.key)"
                      >
                        + 添加一档
                      </button>
                      <button
                        class="btn btn-xs btn-ghost"
                        :disabled="!(draft[f.key] || []).length"
                        @click="clearRoiTiers(f.key)"
                      >
                        清空（关闭）
                      </button>
                      <span class="hint">最多 12 档；保存后自动按分钟升序生效</span>
                    </div>
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

            <div v-if="auditRows.length" class="section">
              <div class="section-hd">
                <span class="section-title">最近变更</span>
                <span class="section-hint">谁在什么时候把这一组改成了什么；密钥类字段只记录"改过"，不记录内容</span>
              </div>
              <div v-for="row in auditRows" :key="row.id" class="audit-row">
                <span class="audit-time">{{ tsText(row.ts) }}</span>
                <span class="audit-user">{{ row.user || '-' }}</span>
                <span class="audit-diff">
                  <span v-for="(val, key) in row.changes" :key="key" class="audit-item">
                    {{ fieldLabel(String(key)) }}
                    <s>{{ shortValue(val.from) }}</s>
                    →
                    {{ shortValue(val.to) }}
                  </span>
                </span>
              </div>
            </div>

            <div v-if="!currentSections.length && current !== 'engine' && current !== 'data' && current !== 'security'" class="empty">
               该分组暂无可编辑字段
            </div>
          </div>
        </div>
      </div>
    </div>

    <!-- 有未保存改动时才出现的悬浮保存条：长表单滚到下面也能直接保存 -->
    <div v-if="dirty" class="save-bar">
      <span class="save-bar-text">
        {{ currentGroup?.label }} · {{ changedKeys.length }} 项未保存
      </span>
      <button class="btn btn-sm btn-ghost" @click="discard">放弃修改</button>
      <button class="btn btn-sm" @click="reset">恢复默认</button>
      <button class="btn btn-sm btn-primary" :disabled="saving" @click="save">
        {{ saving ? '保存中…' : '保存并生效' }}
      </button>
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

/* 改过但还没保存的字段：左边一条黄线 + "已改"角标，滚动时也能一眼看到 */
.field-row.changed {
  box-shadow: inset 2px 0 0 var(--warn);
  padding-left: 8px;
}
.field-row.changed .field-name::after {
  content: '已改';
  margin-left: 6px;
  padding: 0 3px;
  font-size: 9px;
  font-weight: 400;
  color: var(--warn);
  border: 1px solid rgba(240, 169, 42, 0.35);
  border-radius: 4px;
  vertical-align: middle;
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

/* ------------------------------------------------------ 交易模式卡片 */
.trade-mode {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 12px 14px;
  border: 1px solid var(--line);
  border-radius: var(--radius);
  background: var(--panel-2);
}
.tm-row {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.tm-label {
  width: 84px;
  flex: 0 0 84px;
  font-size: 11.5px;
  color: var(--text-dim);
}
.tm-note {
  font-size: 11px;
  line-height: 1.5;
}
.tm-hint {
  font-size: 11.5px;
  color: var(--muted);
  line-height: 1.6;
}

/* ------------------------------------------------ 阶梯止盈档位编辑器 */
.roi-editor {
  display: flex;
  flex-direction: column;
  gap: 6px;
  width: 100%;
}
.roi-head,
.roi-row {
  display: grid;
  grid-template-columns: minmax(88px, 1fr) minmax(110px, 1fr) 28px;
  gap: 8px;
  align-items: center;
}
.roi-head {
  font-size: 10.5px;
  color: var(--muted);
}
.roi-row .input {
  padding: 5px 8px;
}
@media (max-width: 560px) {
  .roi-head,
  .roi-row {
    grid-template-columns: minmax(70px, 1fr) minmax(88px, 1fr) 26px;
    gap: 5px;
  }
}

/* ------------------------------------------------ 参数变更记录 */
.audit-row {
  display: grid;
  grid-template-columns: 84px 72px minmax(0, 1fr);
  gap: 10px;
  align-items: baseline;
  padding: 6px 0;
  border-top: 1px dashed rgba(26, 41, 66, 0.6);
  font-size: 11px;
}
.audit-row:first-of-type {
  border-top: none;
}
.audit-time {
  color: var(--muted);
  font-family: var(--mono);
  font-size: 10.5px;
}
.audit-user {
  color: var(--text-dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.audit-diff {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 10px;
  color: var(--text);
}
.audit-item s {
  color: var(--muted);
  text-decoration-color: rgba(240, 75, 90, 0.6);
}

/* ------------------------------------------------ 参数检索 / 常用直达 */
.search-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.search-input {
  flex: 1;
  min-width: 0;
}
.search-kbd {
  flex: 0 0 auto;
  width: 20px;
  height: 20px;
  display: grid;
  place-items: center;
  font-family: var(--mono);
  font-size: 11px;
  color: var(--muted);
  border: 1px solid var(--line);
  border-radius: 5px;
  background: var(--panel-2);
}

.quick-row {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
}
.quick-label {
  font-size: 10.5px;
  color: var(--muted);
  margin-right: 2px;
}
.chip-btn {
  padding: 3px 9px;
  font-size: 11px;
  font-family: var(--sans);
  color: var(--text-dim);
  background: transparent;
  border: 1px solid var(--line);
  border-radius: 999px;
  cursor: pointer;
  transition: all 0.15s ease;
}
.chip-btn:hover {
  color: var(--text);
  border-color: rgba(59, 130, 246, 0.5);
  background: rgba(59, 130, 246, 0.08);
}

.search-results {
  display: flex;
  flex-direction: column;
  gap: 1px;
  max-height: 300px;
  overflow: auto;
  padding-top: 8px;
  border-top: 1px solid var(--line);
}
.search-empty {
  padding: 12px 8px;
  font-size: 11.5px;
  color: var(--muted);
}
.search-hit {
  display: grid;
  grid-template-columns: 132px 140px minmax(0, 1fr) auto;
  gap: 10px;
  align-items: baseline;
  padding: 7px 8px;
  border: none;
  border-radius: 7px;
  background: transparent;
  color: inherit;
  text-align: left;
  cursor: pointer;
  font-family: var(--sans);
}
.search-hit:hover {
  background: rgba(59, 130, 246, 0.1);
}
.hit-where {
  font-size: 10.5px;
  color: var(--muted);
}
.hit-label {
  font-size: 12px;
  color: var(--text);
  font-weight: 500;
}
.hit-hint {
  font-size: 10.5px;
  color: var(--text-dim);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.hit-value {
  font-family: var(--mono);
  font-size: 10.5px;
  color: var(--accent);
}
.search-foot {
  padding: 7px 8px 2px;
  font-size: 10.5px;
  color: var(--muted);
}

/* 导航上的字段数：点进去之前就知道这一组有多少项 */
.nav-count {
  margin-left: 4px;
  padding: 0 4px;
  font-size: 9.5px;
  font-family: var(--mono);
  color: var(--muted);
  border: 1px solid var(--line);
  border-radius: 4px;
}
.nav-item.on .nav-count {
  color: var(--accent);
  border-color: rgba(59, 130, 246, 0.4);
}

/* 搜索跳转后闪一下，解决"点完不知道跳到哪了" */
.field-row.focus {
  animation: field-flash 2s ease;
}@keyframes field-flash {
  0%,
  55% {
    background: rgba(59, 130, 246, 0.16);
    box-shadow: inset 2px 0 0 var(--accent);
  }
  100% {
    background: transparent;
    box-shadow: inset 2px 0 0 transparent;
  }
}

/* 未保存时悬浮的保存条：长表单滚到最下面也不用回去找按钮 */
.save-bar {
  position: fixed;
  right: 22px;
  bottom: 18px;
  z-index: 30;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 10px 8px 14px;
  background: var(--panel-3);
  border: 1px solid rgba(59, 130, 246, 0.4);
  border-radius: 10px;
  box-shadow: 0 10px 30px rgba(0, 0, 0, 0.45);
}
.save-bar-text {
  font-size: 11px;
  color: var(--text-dim);
  white-space: nowrap;
}

@media (max-width: 760px) {
  .search-hit {
    grid-template-columns: 1fr auto;
    row-gap: 2px;
  }
  .hit-where,
  .hit-hint {
    display: none;
  }
  .save-bar {
    left: 12px;
    right: 12px;
    bottom: 12px;
    justify-content: space-between;
  }
}
</style>
