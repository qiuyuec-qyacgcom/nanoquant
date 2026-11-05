<#
.SYNOPSIS
  启动 NanoQuant (Windows)
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\start.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\start.ps1 -Port 9000 -Debug
#>
param(
  [int]$Port = 8899,
  [switch]$Debug
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$VPython = Join-Path $Root '.venv\Scripts\python.exe'

if (-not (Test-Path $VPython)) {
  Write-Host "未找到虚拟环境, 请先执行: powershell -ExecutionPolicy Bypass -File scripts\install.ps1" -ForegroundColor Yellow
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
& $VPython -m nanoquant
