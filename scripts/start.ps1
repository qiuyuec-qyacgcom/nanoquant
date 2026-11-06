<#
.SYNOPSIS
  启动 NanoQuant (Windows)
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\start.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\start.ps1 -Port 9000 -Debug
.EXAMPLE
  # 虚拟环境不在仓库内时（例如迁移后共用 F:\cb\.venvtest）
  $env:NANOQUANT_VENV='F:\cb\.venvtest'; powershell -File scripts\start.ps1
#>
param(
  [int]$Port = 8899,
  [switch]$Debug,
  [string]$Python = ''
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)

# 解释器探测顺序: -Python 参数 → NANOQUANT_VENV → 仓库内 .venv → 上级目录的共享 venv
$Candidates = @()
if ($Python) { $Candidates += $Python }
if ($env:NANOQUANT_VENV) { $Candidates += (Join-Path $env:NANOQUANT_VENV 'Scripts\python.exe') }
$Candidates += (Join-Path $Root '.venv\Scripts\python.exe')
$Candidates += (Join-Path (Split-Path -Parent $Root) '.venvtest\Scripts\python.exe')
$VPython = $Candidates | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $VPython) {
  Write-Host "未找到 Python 解释器。已尝试:" -ForegroundColor Yellow
  $Candidates | ForEach-Object { Write-Host "  - $_" -ForegroundColor DarkGray }
  Write-Host "请先执行 install.ps1，或设置 NANOQUANT_VENV / 用 -Python 指定 python.exe" -ForegroundColor Yellow
  exit 1
}

$env:NANOQUANT_HOME = $Root
$env:NANOQUANT_PORT = "$Port"
if ($Debug) { $env:NANOQUANT_DEBUG = '1' }

foreach ($d in @('data', 'logs', 'data\backups')) {
  New-Item -ItemType Directory -Force -Path (Join-Path $Root $d) | Out-Null
}

Set-Location (Join-Path $Root 'backend')
Write-Host "NanoQuant 启动中... 访问 http://127.0.0.1:$Port" -ForegroundColor Cyan
Write-Host "解释器: $VPython" -ForegroundColor DarkGray
& $VPython -m nanoquant
