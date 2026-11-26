<script setup lang="ts">
defineProps<{
  credentials: any
  credForms: any
  proxyForm: string
  credSaving: string
}>()

const emit = defineEmits<{
  (e: 'update:proxyForm', value: string): void
  (e: 'save', env: 'demo' | 'live'): void
  (e: 'clear', env: 'demo' | 'live'): void
}>()
</script>

<template>
  <div class="section">
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
          <button class="btn btn-sm btn-primary" :disabled="credSaving === env" @click="emit('save', env)">
            {{ credSaving === env ? '保存中…' : '保存' }}
          </button>
          <button
            v-if="credentials?.[env]?.configured"
            class="btn btn-sm btn-danger"
            @click="emit('clear', env)"
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
      <input
        :value="proxyForm"
        class="input mono"
        placeholder="http://127.0.0.1:7890"
        @input="emit('update:proxyForm', ($event.target as HTMLInputElement).value)"
      />
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
</template>

<style scoped>
.section {
  display: flex;
  flex-direction: column;
  padding: 2px 0 4px;
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
</style>