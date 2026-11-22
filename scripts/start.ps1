<#
.SYNOPSIS
  启动 NanoQuant (Windows, Rust kernel)
#>
param(
  [int]$Port = 0,
  [switch]$Debug,

  [string]$ExecAddr = '127.0.0.1:9101'
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)

if (-not $PSBoundParameters.ContainsKey('Port') -and $env:NANOQUANT_PORT) {
  $Port = [int]$env:NANOQUANT_PORT
}
if ($Port -le 0) {
  $portFile = Join-Path $Root 'data\.port'
  if (Test-Path $portFile) {
    $saved = (Get-Content $portFile -Raw).Trim()
    if ($saved -match '^\d+$') { $Port = [int]$saved }
  }
}
if ($Port -le 0) { $Port = 8899 }

$env:NANOQUANT_HOME = $Root
$env:NANOQUANT_PORT = "$Port"
if ($Debug) { $env:NANOQUANT_DEBUG = '1' }

foreach ($d in @('data', 'logs', 'data\backups')) {
  New-Item -ItemType Directory -Force -Path (Join-Path $Root $d) | Out-Null
}

# 执行内核 exec-core（默认必选）：先起内核，主服务靠 EXEC_TCP 完成握手。
# 内核起不来时主服务仍启动（HTTP/前端可用），但交易引擎不会启动。
$coreScript = Join-Path $Root 'scripts\start-core.ps1'
Remove-Item Env:EXEC_TCP -ErrorAction SilentlyContinue
if (Test-Path $coreScript) {
  & $coreScript -Addr $ExecAddr -Detach | Out-Null
  if ($LASTEXITCODE -eq 0) {
    $env:EXEC_TCP = $ExecAddr
    Write-Host "执行内核已就绪 ($ExecAddr)" -ForegroundColor DarkGray
  } else {
    Write-Host '执行内核未启动（缺交易所 Secret 或二进制）；主服务仍可启动，但交易引擎不会启动' -ForegroundColor Yellow
  }
} else {
  Write-Host '未找到 scripts\start-core.ps1；主服务启动后交易引擎不会启动' -ForegroundColor Yellow
}

$bin = Join-Path $Root 'rust\nanoquant\target\release\nanoquant.exe'
if (-not (Test-Path $bin)) {
  Write-Host "未找到 Rust 二进制: $bin" -ForegroundColor Yellow
  Write-Host "请先执行: cargo build --release --manifest-path rust/nanoquant/Cargo.toml" -ForegroundColor Yellow
  exit 1
}
Write-Host "NanoQuant (rust) 启动中... 访问 http://127.0.0.1:$Port" -ForegroundColor Cyan
& $bin
exit $LASTEXITCODE