<#
.SYNOPSIS
  停止 NanoQuant (Windows)
.DESCRIPTION
  按命令行匹配 `-m nanoquant` 的进程并结束；同时可选地结束 Rust sidecar。
  不会杀掉其它 Python 程序。
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\stop.ps1
#>
param(
  [switch]$WithSidecar
)

$ErrorActionPreference = 'Stop'

$targets = Get-CimInstance Win32_Process -Filter "Name='python.exe'" |
  Where-Object { $_.CommandLine -like '*-m nanoquant*' }

if (-not $targets) {
  Write-Host "没有正在运行的 NanoQuant 进程" -ForegroundColor DarkGray
} else {
  foreach ($p in $targets) {
    try {
      Stop-Process -Id $p.ProcessId -Force
      Write-Host "已停止 NanoQuant (PID $($p.ProcessId))" -ForegroundColor Green
    } catch {
      Write-Host "停止 PID $($p.ProcessId) 失败: $($_.Exception.Message)" -ForegroundColor Red
    }
  }
}

if ($WithSidecar) {
  $sidebar = Get-Process -Name 'exec-service' -ErrorAction SilentlyContinue
  if ($sidebar) {
    $sidebar | ForEach-Object {
      Stop-Process -Id $_.Id -Force
      Write-Host "已停止 exec-service (PID $($_.Id))" -ForegroundColor Green
    }
  } else {
    Write-Host "没有正在运行的 exec-service" -ForegroundColor DarkGray
  }
}
