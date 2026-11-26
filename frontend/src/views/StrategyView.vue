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
import { useRoute, useRouter } from 'vue-router'
import { http } from '../api/client'
import EnvironmentSwitch from '../components/EnvironmentSwitch.vue'
import { useAppStore } from '../stores/app'
import { useAuthStore } from '../stores/auth'
import { useParamCatalogStore } from '../stores/paramCatalog'

import SettingsSection from './settings/SettingsSection.vue'
import { SETTINGS_GROUPS, SETTINGS_SECTIONS, visibleSettingKeys } from './settingsCatalog'

type Widget = 'switch' | 'number' | 'text' | 'seg' | 'select' | 'tags' | 'json' | 'roi'

interface FieldMeta {
  label: string
  hint?: string
  unit?: string
  widget?: Widget
  options?: [string, string][]
  min?: number
  max?: number
  step?: number
}


interface Row extends FieldMeta {
  key: string
  value: any
  widget: Widget
}

const groups = SETTINGS_GROUPS

//: 面向普通用户的白话说明：覆盖原始技术描述（仅影响展示）
const PLAIN_HINTS: Record<string, string> = {
  cycle_interval_sec: '多久扫描一次行情；15 秒以上，间隔越短越耗交易所额度',
  max_symbols_per_cycle: '每轮最多检查几个标的（按标的池顺序）',
  scan_concurrency: '同时拉几路 K 线；只影响信号新旧，不增加请求数',
  max_positions: '最多同时持有几个仓位',

  margin_pct: '单仓保证金中心值；AI 按波动率在 ±5 个百分点内调整',
  local_leverage_base: '波动与评分都中性时的杠杆；实际值再按 ATR 与评分加减并夹在上下限内',
  local_leverage_vol_target: 'ATR% 等于该值时杠杆不缩放；波动越小杠杆越高，波动越大杠杆越低',
  local_leverage_score_weight: '评分每高出中性值 1.0 就多加这么多倍杠杆；0=评分不影响杠杆',
  max_exposure_pct: '所有持仓名义额合计最多是权益的几倍',
  max_same_direction: '同一方向最多同时持有几个仓；0=不限制',
  daily_loss_limit_pct: '当天亏到这个比例就停止开新仓',
  max_drawdown_pct: '权益从最高点回撤到这个比例就停机',
  drawdown_peak_window_days: '回撤基准只看最近几天的最高权益；0=用历史最高',
  entry_threshold: '信号分超过多少才开仓；越高越保守',
  volume_ratio_min: '成交量低于该倍数时不参与',
  atr_pct_min: '波动太小（赚不回手续费）时不做',
  atr_pct_max: '波动太大（容易被扫止损）时不做',

  trailing_pct: '从浮盈高点回撤这么多保证金收益率就离场（价格回撤 = 该值 ÷ 杠杆）；0=关闭',
  breakeven_at_r: '浮盈达到初始止损距离的几倍时，把止损移到开仓价',
  partial_tp_at_r: '浮盈达到初始止损距离的几倍时，先平掉一部分仓位；0=关闭',
  partial_tp_ratio: '部分止盈先平掉几成仓位（0.5 = 平一半）',
  roi_table: '按保证金收益率自动平仓：每档填「持仓分钟 + 保证金赚到百分之几就全平」，可增删档位自行配比；清空=关闭',
  max_spread_bps: '买卖价差超过该值就放弃；短线建议 5–12bps',
  min_notional_buffer: '交易所最小下单额的倍数，防止下单被拒',
  fee_rate_taker: '吃单手续费率，0.0005 = 0.05%',
  fee_rate_maker: '挂单手续费率，0.0002 = 0.02%',
  vol_target_atr_pct: '波动越大自动减仓；0=关闭',
}

const BASE_META: Record<string, FieldMeta> = {

  label_after_sec: { label: '回访窗口', unit: '秒', hint: '决策满这么久才回访打标；86400=24h。超短线可调到 3600-14400，让经验更快产生' },
  inject_experiences: { label: '注入经验', hint: '关闭后经验库照常积累，但不会进入 AI 决策上下文' },
  retrieve_top_k: { label: '注入条数', unit: '条', hint: '每次决策最多注入几条相似经验（经验库是 evolution 心法之外的场景化补充，不宜过多）' },
  correct_threshold_pct: { label: '判对阈值', unit: '%', hint: '方向收益超过该值才算判对，低于视为 partial（partial 不归档成经验）' },
  min_record_confidence: { label: '记录门槛', hint: 'AI 置信度低于该值的方向判断不留痕（journal），避免用低质量判断训练经验库' },
  min_similarity: { label: '相似度阈值', unit: '', hint: '低于该相似度的经验不注入；调低=注入更多但相关性更弱' },
  max_experiences: { label: '经验上限', unit: '条', hint: '超出后停用最旧条目（不删除）' },


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
  scan_concurrency: { label: '扫描并发', unit: '路', hint: '串行时 12 个标的约 2.2s；并发只降延迟，不增加权重' },
  max_positions: { label: '最大同时持仓', unit: '个' },
  trading_windows: { label: '交易时段', widget: 'tags', hint: 'HH:MM-HH:MM，可多个；00:00-24:00 表示全天' },
  timezone_offset: { label: '时区偏移', unit: '小时', hint: '交易时段所用时区，中国为 +8；仅此一处生效' },
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

  margin_pct: {
    label: '单仓保证金中枢',
    unit: '%',
    hint: '中心值 = 权益 × 该比例；AI 按波动率在 ±5 个百分点内调整（如 20% → 15%–25%），最终仍受可用余额与最小名义额约束',
  },
  atr_stop_mult: {
    label: '止损 ATR 倍数',
    unit: '×',
    hint: '提示 AI：止损保证金比例 ≥ 该倍数 × ATR% × 杠杆；只做提示词引导，不强制校验',
  },
  min_leverage: {
    label: '杠杆下限', unit: 'x', min: 1,
    hint: '本地杠杆规则的下限：算出的杠杆低于该值会抬到此处',
  },
  max_leverage: {
    label: '杠杆上限', unit: 'x', max: 100,
    hint: '本地杠杆规则的硬上限；单标的配置也只能收紧不能突破',
  },
  local_leverage_base: {
    label: '本地杠杆基准',
    unit: 'x',
    hint: '波动率中性（ATR% = 波动目标）且评分中性（0.5）时的杠杆；实际值再按波动与评分加减并夹在上限内',
  },
  local_leverage_vol_target: {
    label: '杠杆波动目标',
    unit: '%',
    hint: 'ATR% 等于该值时波动倍率为 1；ATR% 越低倍率越高（最高 2x），越高越低（最低 0.5x）',
  },
  local_leverage_score_weight: {
    label: '杠杆评分权重',
    unit: 'x',
    hint: '评分相对 0.5 每高 1.0 增加的杠杆；0=评分不影响杠杆',
  },
  max_exposure_pct: { label: '总敞口上限', unit: '%', hint: '所有持仓名义额 / 权益' },
  max_same_direction: {
    label: '同向持仓上限',
    unit: '个',
    hint: '0=不限制；与「最大持仓」同时生效，取更小值；币种高度相关，小资金建议设 1',
  },
  correlation_limit: {
    label: '高相关同向上限',
    unit: '个',
    hint: '同向高相关币种最多几个；BTC/ETH/SOL 视为同一簇，其余按基础资产归组；1–5',
  },
  direction_exposure_limit_pct: {
    label: '同向敞口上限',
    unit: '%',
    hint: '同一方向持仓名义额（含本次）/ 权益；与总敞口上限同时生效，取更小值',
  },
  vol_target_atr_pct: {
    label: '波动率目标',
    unit: '%',
    hint: '如 1.5：ATR% 高于它自动缩仓、低于它放大（0.25–2x）；0=关闭',
  },
  daily_loss_limit_pct: { label: '日内亏损上限', unit: '%', hint: '触发后当日停止开新仓' },
  max_drawdown_pct: { label: '最大回撤', unit: '%', hint: '基于权益峰值' },
  global_stop_loss_pct: { label: '仓位止损率', unit: '%', hint: '保证金亏损达到该比例就整仓平仓；价格距离 = 该值 ÷ 杠杆，0=关闭' },
  global_take_profit_pct: { label: '仓位止盈率', unit: '%', hint: '保证金盈利达到该比例就整仓平仓；价格距离 = 该值 ÷ 杠杆，0=关闭' },
  drawdown_peak_window_days: { label: '回撤峰值窗口', unit: '天', hint: '0=历史最高（永不遗忘）；>0 只看最近 N 天，小资金腰斩后不会永久卡死' },
  consecutive_loss_limit: { label: '连亏熔断', unit: '笔', hint: '连续亏损达到该笔数就暂停开新仓' },
  cooldown_minutes: { label: '熔断冷静期', unit: '分钟', hint: '熔断后暂停开新仓的分钟数' },
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
  ema_fast: { label: '快线 EMA', hint: '短周期均线，用于判断短期趋势方向' },
  ema_slow: { label: '慢线 EMA', hint: '长周期均线，用于判断中期趋势方向' },
  rsi_period: { label: 'RSI 周期', hint: 'RSI 计算使用的 K 线根数' },
  atr_period: { label: 'ATR 周期', hint: 'ATR 计算使用的 K 线根数' },
  entry_threshold: { label: '开仓信号阈值', hint: '综合分绝对值高于该值才考虑开仓' },
  exit_threshold: { label: '反转平仓阈值', hint: '反向信号分超过该值就平掉当前仓位' },
  volume_ratio_min: { label: '最小量比', hint: '低于该值的信号被过滤' },
  atr_pct_min: { label: 'ATR 下限', unit: '%', hint: '波动太小不足以覆盖手续费' },
  atr_pct_max: { label: 'ATR 上限', unit: '%', hint: '波动过大不参与' },
  min_24h_quote_volume: { label: '24H 最小成交额', unit: 'USDT' },

  trailing_pct: { label: '移动止损回撤', unit: '%', hint: '保证金收益率回撤；价格回撤 = 该值 ÷ 杠杆，0 表示关闭' },
  trailing_arm_r: { label: '移动止损激活', unit: 'R', hint: '浮盈先走到 N 倍风险才让移动止损接管，否则会在止损前被噪声扫掉（实盘 24 笔全亏的根因）' },
  breakeven_at_r: { label: '保本触发', unit: 'R', hint: '浮盈达到该 R 倍数后止损上移到开仓价' },
  partial_tp_at_r: { label: '部分止盈触发', unit: 'R', hint: '浮盈达到该 R 倍数先平一部分；0=关闭' },
  partial_tp_ratio: { label: '部分止盈比例', unit: '比例', hint: '0.5 = 平一半，剩余交给移动止损' },
  max_spread_bps: { label: '盘口点差上限', unit: 'bps', hint: '买一卖一价差超过该值就放弃开仓；0=关闭。短线建议 5–12bps' },
  funding_veto_bps: { label: '资金费率否决', unit: 'bps', hint: '资金费率超过该值时，禁止开同向仓（正费率禁多、负费率禁空）', min: 0, max: 50, step: 0.5 },
  mr_enabled: { label: '均值回归', hint: '只在震荡市（regime=range）启用；关闭后震荡市继续走趋势信号' },
  mr_rsi_high: { label: 'RSI 超买线', hint: 'RSI 高于该值且收盘突破布林上轨时做空', min: 50, max: 90, step: 1 },
  mr_rsi_low: { label: 'RSI 超卖线', hint: 'RSI 低于该值且收盘跌破布林下轨时做多', min: 10, max: 50, step: 1 },
  mr_band_k: { label: '布林带倍数', unit: 'σ', hint: '20 日均线 ± N 倍标准差构成上下轨；倍数越大越难触发', min: 1, max: 4, step: 0.1 },
  mr_sl_pct: { label: '均值回归止损率', unit: '%', hint: '保证金口径；价格距离 = 该值 ÷ 杠杆，不继承全局止损率', min: 5, max: 50, step: 1 },
  mr_tp_pct: { label: '均值回归止盈率', unit: '%', hint: '保证金口径；价格距离 = 该值 ÷ 杠杆，不继承全局止盈率', min: 5, max: 100, step: 1 },
  roi_table: {
    label: '时间阶梯止盈',
    widget: 'roi',
    hint: '按保证金收益率自动平仓：每档填「持仓分钟 + 保证金赚到百分之几就全平」，可增删档位自行配比',
  },

  roi_min_r: { label: 'ROI 阶梯最低 R', unit: 'R', hint: '浮盈先达到该 R 倍数，时间阶梯止盈才生效；0=不设门槛' },
  max_data_gaps: { label: '最大数据缺口', unit: '根', hint: 'K 线允许缺失的根数；超过就视为行情不可用' },
  data_stale_mult: { label: '行情过期倍数', hint: '最新 K 线超过「周期 × 该倍数」就停止开仓' },

  live_base_url: { label: '实盘域名', hint: '币安 U 本位主网 fapi 地址' },
  demo_base_url: { label: '模拟盘域名', hint: '币安测试网地址（模拟盘真实下单环境）' },
  demo_backup_urls: { label: '模拟盘备用域名', widget: 'tags', hint: '测试网主域名不可达时按顺序尝试' },
  backup_base_urls: { label: '实盘备用域名', widget: 'tags', hint: '主域名不可达时按顺序尝试' },
  proxy: { label: 'HTTP 代理', hint: '形如 http://127.0.0.1:7890，留空不使用' },
  kline_limit: { label: 'K 线长度', unit: '根', hint: '引擎内部取价使用，不作为对外行情接口' },
  ticker_ttl_sec: { label: '行情缓存', unit: '秒', hint: '引擎内部 ticker 缓存，减少 API 权重' },
  kline_ttl_sec: { label: 'K 线缓存', unit: '秒' },
  book_ws_enabled: { label: '本地盘口', hint: '开启后用 bookTicker WebSocket 维护买一/卖一；关闭则完全走 REST' },
  book_stale_ms: { label: '盘口过期', unit: '毫秒', hint: '本地盘口超过该时长未更新就回退 REST' },
  request_timeout_sec: { label: '请求超时', unit: '秒' },
  max_pool_size: { label: '标的池上限', unit: '个', hint: '与 astra 规则一致，超限拒绝添加' },

  refresh_minutes: { label: '刷新间隔', unit: '分钟', hint: '资讯抓取的时间间隔' },
  sources: { label: '资讯源', widget: 'tags', hint: 'coindesk / binance_square / binance_announcement 等' },
  keep_days: { label: '保留天数', unit: '天', hint: '超过该天数的资讯会被清理' },
  ai_summary: { label: 'AI 摘要', hint: '开启后对新资讯生成情绪摘要' },
  sentiment_threshold: { label: '情绪阈值', hint: '绝对值超过该值才标记为显著情绪' },
  max_items_per_source: { label: '单源最多条数', unit: '条' },
  veto_enabled: { label: '事件否决', hint: '开启后，最近窗口内的标的负面事件会否决同向开仓' },
  veto_window_min: { label: '事件回看窗口', unit: '分钟', hint: '只统计最近多少分钟内的资讯；范围 5–240' },
  veto_max_items: { label: '否决引用条数', unit: '条', hint: '拒绝原因里最多引用几条命中资讯；范围 1–10' },
  rsshub_base: { label: 'RSSHub 地址', hint: '可选，自建实例用于抓取币安公告' },

  review_on_close: { label: '平仓即复盘', hint: '每笔平仓后自动做一次复盘' },
  max_injected: { label: '最多注入心法', unit: '条', hint: '每次决策最多注入几条心法' },
  ttl_days: { label: '心法有效期', unit: '天', hint: '过期自动退出注入' },
  min_confidence: { label: '心法最低置信度', hint: '低于该置信度的复盘结论不进入心法库' },
  global_review_every_trades: { label: '全局复盘间隔', unit: '笔', hint: '每多少笔交易做一次全局复盘' },
  guard_strict: { label: '严格防污染', hint: '拦截放宽风控类结论' },
  auto_disable_negative: { label: '负向心法自动降权' },
  max_rule_chars: { label: '心法最大字数', unit: '字' },

  retention: { label: '备份保留份数', unit: '份', hint: '只保留最近几份备份' },
  encrypt: { label: '备份加密', hint: 'Fernet 对称加密' },
  auto_daily: { label: '每日自动备份', hint: '每天自动生成一次备份' },
  include_logs: { label: '包含日志文件', hint: '备份时是否一并打包日志' },

  username: { label: '管理员账号', hint: '后台登录使用的账号名' },
  password_is_default: { label: '仍为默认密码', hint: '仅展示；首次登录后应尽快修改密码' },
  require_login: { label: '强制登录', hint: '关闭后访问后台不再要求登录（不建议）' },
  session_ttl_hours: { label: '会话有效期', unit: '小时', hint: '登录后多久需要重新认证' },
  login_fail_limit: { label: '登录失败锁定阈值', unit: '次', hint: '同一 IP 连续失败达到该次数就锁定' },
  login_fail_window_sec: { label: '失败计数窗口', unit: '秒', hint: '窗口内同一 IP 失败达阈值即锁定；按 IP 隔离，攻击者刷错密码不会锁死管理员' },

  log_level: {
    label: '日志级别',
    widget: 'seg',
    hint: 'DEBUG 最详细；生产建议 INFO，排查问题再临时调到 DEBUG',
    options: [
      ['DEBUG', 'DEBUG'],
      ['INFO', 'INFO'],
      ['WARNING', 'WARN'],
      ['ERROR', 'ERROR'],
    ],
  },
  event_retention_days: { label: '事件保留', unit: '天', hint: '事件日志保留天数，过期清理' },
  signal_retention_days: { label: '信号保留', unit: '天', hint: '过期先归档到 data/archive/ 再删除' },
  equity_retention_days: { label: '权益曲线保留', unit: '天', hint: '权益采样保留天数' },
  ui_refresh_sec: { label: '面板刷新', unit: '秒', hint: '前端面板静默刷新间隔' },
  risk_disclaimer_accepted: { label: '已确认风险声明', hint: '首次进入实盘前需要确认；仅展示状态' },
  alert_enabled: { label: '启用告警', hint: '事件级别达到阈值时推送，失败不影响交易' },
  alert_min_level: {
    label: '最低级别',
    widget: 'seg',
    hint: '达到该级别才推送；info 最吵，error 只推严重问题',
    options: [
      ['info', 'INFO'],
      ['warn', 'WARN'],
      ['error', 'ERROR'],
    ],
  },
  alert_webhook_url: { label: 'Webhook 地址', hint: '钉钉 / 飞书 / 企业微信 文本机器人 URL' },
  alert_error_webhook_url: { label: 'Error 专用 Webhook', hint: '值班群；留空则回落到上面的默认 Webhook' },
  alert_telegram_token: { label: 'Telegram Bot Token', hint: '向 BotFather 申请；留空则不发 Telegram' },
  alert_telegram_chat_id: { label: 'Telegram Chat ID', hint: '接收告警的会话或群组 ID' },
  alert_cooldown_sec: { label: '同类告警冷却', unit: '秒', hint: '同一类别在窗口内只发一次' },
  alert_quiet_hours: { label: '静默时段', hint: '如 23:00-07:00，期间只放行 error；留空=不静默' },
  alert_template: { label: '消息模板', hint: '支持 {level} {category} {message} {time}；留空=默认格式' },
  alert_digest_enabled: { label: '摘要模式', hint: '开启后 warn 攒到间隔合并成一条，error 仍然即时' },
  alert_digest_minutes: { label: '摘要间隔', unit: '分钟', hint: 'warn 级告警合并成一条的间隔' },

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
  site_title: { label: '站点名称', hint: '公开前台顶部显示的站点名' },
  site_tagline: { label: '站点副标题', hint: '公开前台顶部显示的副标题' },
  hero_note: { label: '首页备注', hint: '可选，展示在首页英雄区' },
  show_engine_status: { label: '展示引擎状态', hint: '仅开关与模式，不含资金数据' },
  show_mantras: { label: '展示分析心法', hint: '脱敏展示：仅标题/分类/健康度' },
  show_performance: { label: '展示绩效指标', hint: '胜率/盈亏比等比例数据，不含金额' },

  epsilon: { label: '探索率 ε', unit: '', min: 0, max: 0.5, step: 0.01, hint: 'epsilon-greedy 的探索概率；上限 0.5，默认 0.05' },
  max_wait_ms: { label: '最长等待', unit: '毫秒', min: 0, max: 10000, step: 500, hint: '等待动作的上限；超过上限或执行器不支持时退化为限价挂单' },
  alpha: { label: '学习率 α', min: 0.01, max: 1, step: 0.01, hint: '离线回放更新 Q 值的步长' },
  gamma: { label: '折扣因子 γ', min: 0.5, max: 0.99, step: 0.01, hint: '未来奖励折扣；越接近 1 越看重后续滑点' },
}

// BASE_META 是按"字段名"索引的扁平表，但有些 key 在多个分组里语义不同。
// 这里按分组覆盖，避免以后把 engine.mode / ai.min_confidence 加进参数页时
// 串成"信号模型 trend/meanrev"这种完全不对的标签和选项。
const GROUP_META: Record<string, Record<string, FieldMeta>> = {
  engine: {
    mode: {
      label: '交易环境', widget: 'select',
      options: [['demo', 'demo 模拟盘（测试网）'], ['live', 'live 实盘（主网）']],
      hint: '与「策略 → 信号模型」的 mode 不是一回事；切换前先确认对应环境的 API 凭证',
    },
  },
  strategy: {
    mode: {
      label: '信号模型', widget: 'select',
      options: [['fusion', 'NQ-Fusion（当前）']],
      hint: '当前唯一信号模型；按 regime 加权相对强弱、波动突破、资金费率与 OI/订单流',
    },
    valid_bars: { label: '信号有效期', unit: '根 K 线', min: 1, max: 24, step: 1, hint: 'AI 给出的判断有效期（K 线根数）；1 根 = 1 个当前 K 线周期，仅供审计展示，不改变开仓行为' },
    min_entry_rr: { label: '最低开仓盈亏比', unit: 'R', min: 0, max: 10, step: 0.1, hint: 'AI 预期盈亏比低于该值时不开仓' },
  },
  ai: {
    mode: { label: '模型编排', hint: 'single=单模型；多模型共识请配合 require_agreement' },
    min_confidence: { label: 'AI 最低置信度', hint: '低于该值的模型裁决不采纳' },
    use_invalidation_price: { label: '启用 AI 失效价平仓', hint: '开启后持仓触及 AI 失效价即平仓' },
    use_entry_price: { label: '启用 AI 建议限价', hint: '仅只做 Maker 模式生效；偏离现价超过 0.5% 或方向不符（BUY 高于现价 / SELL 低于现价）时忽略' },
    param_tuning_min_interval_sec: {
      label: '调参最小间隔', unit: '秒', min: 60, max: 604800,
      hint: '独立调参任务的最小间隔；0 已废弃，最低 60 秒，86400=每天一次',
    },
  },
  evolution: {
    min_confidence: { label: '心法最低置信度', hint: '低于该值的复盘结论不进入心法库' },
  },
  journal: {
    enabled: { label: '启用决策经验闭环', hint: '关闭后不再记录决策、不打标、不归档、不注入' },
  },
  news: {
    proxy: { label: '资讯代理', hint: '留空则复用「数据源 → HTTP 代理」；资讯要走单独出口时才填' },
    request_timeout_sec: { label: '资讯抓取超时', unit: '秒', hint: '只作用于资讯源，不影响行情请求' },
  },
  data: {
    request_timeout_sec: { label: '行情请求超时', unit: '秒', hint: '只作用于交易所行情接口' },
  },
  ai_tuning: {
    enabled: { label: '启用参数集', hint: '关闭后停止 regime 选择、自动启用与无候选回滚；手动启用/回滚保留' },
    auto_apply: { label: '自动启用', hint: '证据门通过后自动启用 approved 候选；关闭则只允许手动启用' },
    max_auto_apply_per_24h: { label: '24h 自动启用上限', unit: '次', min: 1, max: 100, hint: '滚动 24 小时只统计 auto_apply' },
    no_candidate_rollback_sec: { label: '无候选回滚时长', unit: '秒', min: 60, max: 86400, hint: '生效后连续无 gate 候选的实跑时长，到期回滚上一套' },
    regime_confirm_sec: { label: 'regime 确认时长', unit: '秒', min: 60, max: 86400, hint: 'market_regime 连续一致达到该时长才切换' },
    regime_cooldown_sec: { label: 'regime 冷却', unit: '秒', min: 0, max: 86400, hint: '两次 regime 切换的最小间隔' },
    min_oos_trades: { label: '样本外成交下限', unit: '笔', min: 1, max: 1000, hint: '证据门中目标 regime 的样本外成交数下限' },
    auto_apply_timeframes: { label: '自动启用周期', hint: 'v1 仅 1h；逗号分隔的白名单，其它周期需人工校准' },
  },
  symbol_scan: {
    enabled: { label: '启用自动换池', widget: 'switch', hint: '开启后按间隔自动扫描并替换标的' },
    interval_hours: { label: '扫描间隔', unit: '小时', min: 1, max: 24, hint: '范围 1–24 小时' },
    market_limit: { label: '扫描数量', unit: '个', min: 50, max: 500, hint: '范围 50–500 个；按 24h 成交额取前 N 个 USDT 永续' },
  },
  rl_exec: {
    enabled: { label: '启用 RL 执行', widget: 'switch', hint: '只决定下单方式与限价偏移；关闭时完全走原执行逻辑' },
  },
}

const SECTIONS = SETTINGS_SECTIONS

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
  'token_version',
])

const app = useAppStore()
const paramCatalog = useParamCatalogStore()
const auth = useAuthStore()
const route = useRoute()
const router = useRouter()

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
  return { key, value, widget: widgetOf(value, meta), label: meta.label, hint: meta.hint || '', unit: meta.unit || '', options: meta.options, min: meta.min, max: meta.max, step: meta.step }
}

function skippedKeys(): Set<string> {
  return skipOf(current.value)
}

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
  { label: '交易间隔', group: 'engine', key: 'cycle_interval_sec' },
  { label: '下单方式', group: 'engine', key: 'order_type' },
  { label: '最大持仓', group: 'engine', key: 'max_positions' },

  { label: 'K 线周期', group: 'strategy', key: 'timeframe' },
  { label: '单仓保证金中枢', group: 'risk', key: 'margin_pct' },
  { label: '最大杠杆', group: 'risk', key: 'max_leverage' },
  { label: '本地杠杆基准', group: 'risk', key: 'local_leverage_base' },
  { label: '日亏上限', group: 'risk', key: 'daily_loss_limit_pct' },
  { label: '最大回撤', group: 'risk', key: 'max_drawdown_pct' },

  { label: '点差闸门', group: 'strategy', key: 'max_spread_bps' },
  { label: '手续费率', group: 'risk', key: 'fee_rate_taker' },
]

function sectionMapOf(group: string): Map<string, string> {
  const m = new Map<string, string>()
  for (const def of SECTIONS[group] || []) for (const k of def.fields) m.set(k, def.title)
  return m
}

function skipOf(group: string): Set<string> {
  return new Set([...PROTECTED, ...(SKIP[group] || []), ...paramCatalog.hiddenFor(group), 'symbols'])
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

/** 跳到某个参数：必要时切分组、滚动到视野中央并闪一下 */
async function jumpTo(group: string, key: string) {
  // 首屏还在拉配置时点「常用直达」会没有任何反应：先等配置到位再跳
  if (!Object.keys(app.settings || {}).length) await app.loadSettings()
  if (group !== current.value) {
    if (dirty.value && !confirm('当前分组有未保存的修改，切换后会被丢弃，继续？')) return
    loadGroup(group)
  }
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

/** 只看改动：长表单保存前核对到底动了哪几项 */
function toggleOnlyDirty() {
  onlyDirty.value = !onlyDirty.value
}

const currentSections = computed(() => {
  const skip = skippedKeys()
  const keep = onlyDirty.value ? new Set(changedKeys.value) : null
  const allowed = new Set(
    visibleSettingKeys(current.value, Object.keys(draft.value || {}), skip, paramCatalog.failed),
  )
  const rows = new Map<string, Row>()
  for (const [key, value] of Object.entries(draft.value || {})) {
    if (!allowed.has(key)) continue
    if (keep && !keep.has(key)) continue
    rows.set(key, rowOf(current.value, key, value))
  }
  const out: { title: string; hint: string; rows: Row[] }[] = []
  for (const def of SECTIONS[current.value] || []) {
    const list = def.fields.filter((k) => rows.has(k)).map((k) => rows.get(k)!)
    if (!list.length && (def.fields.length > 0 || onlyDirty.value)) continue
    list.forEach((r) => rows.delete(r.key))
    out.push({ title: def.title, hint: def.hint || '', rows: list })
  }

  return out
})


// ---------------------------------------------------- 交易模式（双向逐仓）
const tradeMode = ref<any>(null)
const tradeModeError = ref('')
const isolatedOnly = computed(() => app.settings.engine?.isolated === true)
const isolatedBusy = ref(false)

async function loadTradeMode() {
  try {
    tradeMode.value = await http.get('/engine/trade-mode', 8000)
    tradeModeError.value = ''
  } catch (err: any) {
    tradeMode.value = null
    tradeModeError.value = err?.message || '交易模式读取失败'
  }
}

async function toggleIsolated() {
  if (isolatedBusy.value) return
  const enabled = !isolatedOnly.value
  isolatedBusy.value = true
  try {
    await app.saveGroup('engine', { isolated: enabled })
    if (current.value === 'engine') {
      // 开关是独立保存的；必须同步表单草稿，否则随后「保存并生效」会用旧值覆盖。
      draft.value = { ...draft.value, isolated: enabled }
      baseline.value = { ...baseline.value, isolated: enabled }

    }
    if (enabled && tradeMode.value?.can_switch) {
      const res = await http.post(
        '/engine/trade-mode',
        { hedge: Boolean(tradeMode.value?.hedge_mode), isolated: true },
        15000,
      )
      const firstFail = (res.results || []).find((item: any) => !item.ok)
      app.notify(
        res.ok ? '已开启全局逐仓并完成切换' : firstFail?.detail || '逐仓开关已保存，部分标的切换失败',
        res.ok ? 'ok' : 'error',
      )
      await loadTradeMode()
    } else if (enabled) {
      app.notify('已开启全局逐仓开单；现有仓位保持原模式，新开仓会自动切逐仓', 'info')
    } else {
      app.notify('已关闭全局逐仓开单', 'ok')
    }
  } catch (err: any) {
    app.notify(err.message, 'error')
  } finally {
    isolatedBusy.value = false
  }
}

function loadGroup(id: string) {
  current.value = id
  draft.value = clone(app.settings[id] || {})
  baseline.value = clone(app.settings[id] || {})
  onlyDirty.value = false
  focusKey.value = ''

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
    // 被"出厂密码"拦到这里的人, 改完就该放回原路, 而不是留在参数页
    const forced = auth.mustChangePassword
    await auth.changePassword(pwd.old_password, pwd.new_password)
    pwd.old_password = ''
    pwd.new_password = ''
    await app.loadSettings()
    app.notify('密码已更新', 'ok')
    if (forced) router.push('/admin/dashboard')
  } catch (err: any) {
    app.notify(err.message, 'error')
  }
}


watch(current, (id) => {
  draft.value = clone(app.settings[id] || {})
  baseline.value = clone(app.settings[id] || {})

})

onMounted(async () => {
  window.addEventListener('keydown', onHotkey)
  if (!Object.keys(app.settings).length) await app.loadSettings()
  await Promise.all([app.loadStatus(), paramCatalog.load()])
  const wanted = String(route.query.group || '')
  const group = groups.some((g) => g.id === wanted) ? wanted : 'engine'
  loadGroup(group)
  await loadCredentials()
  // 支持 ?group=risk&key=max_drawdown_pct 直达某个参数（文档与提示可以精确指路）
  const key = String(route.query.key || '')
  if (key) await jumpTo(group, key)
})

onUnmounted(() => {
  window.removeEventListener('keydown', onHotkey)
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
                <span class="section-hint">系统按「双向持仓 + 逐仓」设计；开关只控制新开仓自动逐仓；开启时若无持仓/挂单会立即切换当前标的池</span>
              </div>
              <div v-if="tradeMode" class="trade-mode">
                <div class="tm-row">
                  <span class="tm-label">全局逐仓开单</span>
                  <div
                    class="switch"
                    :class="{ on: isolatedOnly, disabled: isolatedBusy }"
                    role="switch"
                    :aria-checked="isolatedOnly"
                    aria-label="全局逐仓开单"
                    :title="isolatedOnly ? '关闭全局逐仓开单' : '开启全局逐仓开单'"
                    tabindex="0"
                    @click="toggleIsolated"
                    @keydown.enter.prevent="toggleIsolated"
                    @keydown.space.prevent="toggleIsolated"
                  />
                  <span class="muted tm-note">
                    {{ isolatedOnly ? '开启' : '关闭' }} · 只控制新开仓；已有全仓仓位需先平仓
                  </span>
                </div>
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
                          ? '当前池全部逐仓'
                          : '逐仓状态未知'
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
                <div v-if="!tradeMode.can_switch" class="tag tag-warn">
                  有持仓：开关只影响新开仓；立即切换需先平仓
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
              <router-link to="/admin/strategies">全局策略</router-link>。
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

            <SettingsSection
              v-for="sec in currentSections"
              :key="sec.title"
              :section="sec"
              :draft="draft"
              :changed-keys="changedKeys"
              :focus-key="focusKey"
              :group-id="current"
              @update="(key, value) => draft[key] = value"
            />


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
.trade-mode .switch {
  flex: 0 0 38px;
}
.switch.disabled {
  opacity: 0.45;
  cursor: not-allowed;
}
.switch:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: 2px;
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
