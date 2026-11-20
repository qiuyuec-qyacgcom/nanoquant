<#
.SYNOPSIS
  启动 NanoQuant (Windows, Rust kernel)
#>
param(
  [int]$Port = 0,
  [switch]$Debug
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

$bin = Join-Path $Root 'rust\nanoquant\target\release\nanoquant.exe'
if (-not (Test-Path $bin)) {
  Write-Host "未找到 Rust 二进制: $bin" -ForegroundColor Yellow
  Write-Host "请先执行: cargo build --release --manifest-path rust/nanoquant/Cargo.toml" -ForegroundColor Yellow
  exit 1
}
Write-Host "NanoQuant (rust) 启动中... 访问 http://127.0.0.1:$Port" -ForegroundColor Cyan
& $bin
exit $LASTEXITCODE