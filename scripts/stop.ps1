<#
.SYNOPSIS
  停止 NanoQuant (Windows)
.DESCRIPTION
  以 data\nanoquant.pid 与 nanoquant.exe 为唯一对象；不会按解释器命令行匹配。
  可选结束 exec-core 执行内核。
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\stop.ps1
#>
param(
  [switch]$WithCore
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$PidFile = Join-Path $Root 'data\nanoquant.pid'
$stopped = $false

if (Test-Path $PidFile) {
  $pidText = (Get-Content -Path $PidFile -Raw).Trim()
  if ($pidText -match '^\d+$') {
    $proc = Get-Process -Id ([int]$pidText) -ErrorAction SilentlyContinue
    if ($proc) {
      try {
        Stop-Process -Id $proc.Id -Force
        Write-Host "已停止 NanoQuant (PID $($proc.Id))" -ForegroundColor Green
        $stopped = $true
      } catch {
        Write-Host "停止 PID $($proc.Id) 失败: $($_.Exception.Message)" -ForegroundColor Red
      }
    }
  }
  Remove-Item -Path $PidFile -Force -ErrorAction SilentlyContinue
}

if (-not $stopped) {
  $targets = Get-Process -Name 'nanoquant' -ErrorAction SilentlyContinue
  if (-not $targets) {
    Write-Host "没有正在运行的 NanoQuant 进程" -ForegroundColor DarkGray
  } else {
    foreach ($p in $targets) {
      try {
        Stop-Process -Id $p.Id -Force
        Write-Host "已停止 nanoquant (PID $($p.Id))" -ForegroundColor Green
      } catch {
        Write-Host "停止 PID $($p.Id) 失败: $($_.Exception.Message)" -ForegroundColor Red
      }
    }
  }
}

if ($WithCore) {
  $core = Get-Process -Name 'exec-core' -ErrorAction SilentlyContinue
  if ($core) {
    $core | ForEach-Object {
      Stop-Process -Id $_.Id -Force
      Write-Host "已停止 exec-core (PID $($_.Id))" -ForegroundColor Green
    }
  } else {
    Write-Host "没有正在运行的 exec-core" -ForegroundColor DarkGray
  }
}