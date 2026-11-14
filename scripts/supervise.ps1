<#
.SYNOPSIS
  NanoQuant 守护进程（Windows）：进程挂了自动拉起，日志按大小/时间轮转。

.DESCRIPTION
  为什么需要它：2026-11 那次事故里，进程在 11-12 09:35 之后就再没写心跳，
  但页面还能打开、没人发现——引擎停了两天半。健康检查 + 自动重启能把这类问题
  从"靠人盯"变成"自己恢复"。

  行为：
    1. 每 IntervalSec 秒请求 /api/v1/health；
    2. 不健康且进程已退出 → 立刻重启（同一分钟内最多重启 3 次，避免崩溃风暴）；
    3. 健康但心跳文件超过 5 分钟没更新 → 只记日志，不重启（可能是引擎未启用）；
    4. 每次启动前轮转 app.out.log / app.err.log（保留最近 3 份，各限 MaxLogMB）。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\supervise.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\supervise.ps1 -Port 9000 -IntervalSec 15
.EXAMPLE
  # 只做一次检查（用于计划任务或排查）
  powershell -ExecutionPolicy Bypass -File scripts\supervise.ps1 -Once
#>
param(
  [int]$Port = 8899,
  [int]$IntervalSec = 30,
  [int]$MaxLogMB = 20,
  [switch]$Once,
  [string]$Python = ''
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$LogDir = Join-Path $Root 'logs'
$DataDir = Join-Path $Root 'data'
$SupervisorLog = Join-Path $LogDir 'supervisor.log'
$OutLog = Join-Path $LogDir 'app.out.log'
$ErrLog = Join-Path $LogDir 'app.err.log'

New-Item -ItemType Directory -Force -Path $LogDir, $DataDir | Out-Null

function Write-SupLog([string]$Message) {
  $line = "{0} {1}" -f (Get-Date -Format 'yyyy-MM-dd HH:mm:ss'), $Message
  Add-Content -Path $SupervisorLog -Value $line -Encoding UTF8
  Write-Host $line
}

function Resolve-Python {
  $candidates = @()
  if ($Python) { $candidates += $Python }
  if ($env:NANOQUANT_VENV) { $candidates += (Join-Path $env:NANOQUANT_VENV 'Scripts\python.exe') }
  $candidates += (Join-Path $Root '.venv\Scripts\python.exe')
  $candidates += (Join-Path (Split-Path -Parent $Root) '.venvtest\Scripts\python.exe')
  return ($candidates | Where-Object { Test-Path $_ } | Select-Object -First 1)
}

function Test-Health {
  try {
    $r = Invoke-WebRequest -Uri "http://127.0.0.1:$Port/api/v1/health" -UseBasicParsing -TimeoutSec 5
    return ($r.StatusCode -eq 200)
  } catch {
    return $false
  }
}

function Get-HeartbeatAgeSec {
  $hb = Join-Path $DataDir 'engine.heartbeat'
  if (-not (Test-Path $hb)) { return -1 }
  try {
    $first = (Get-Content $hb -TotalCount 1).Trim()
    if (-not $first) { return -1 }
    return [int]([DateTimeOffset]::UtcNow.ToUnixTimeSeconds() - [int64]$first)
  } catch { return -1 }
}

function Rotate-Log([string]$Path) {
  if (-not (Test-Path $Path)) { return }
  $sizeMB = (Get-Item $Path).Length / 1MB
  if ($sizeMB -lt $MaxLogMB) { return }
  $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
  $rotated = [System.IO.Path]::ChangeExtension($Path, ".$stamp.log")
  Move-Item -Path $Path -Destination $rotated -Force
  # 只留最近 3 份同前缀日志
  Get-ChildItem -Path (Split-Path $Path) -Filter ("{0}.*.log" -f [System.IO.Path]::GetFileNameWithoutExtension($Path)) |
    Sort-Object LastWriteTime -Descending |
    Select-Object -Skip 3 |
    Remove-Item -Force -ErrorAction SilentlyContinue
  Write-SupLog "日志轮转: $(Split-Path $Path -Leaf) -> $(Split-Path $rotated -Leaf)"
}

function Start-Engine($VPython) {
  Rotate-Log $OutLog
  Rotate-Log $ErrLog
  if (-not $env:NANOQUANT_HOME) { $env:NANOQUANT_HOME = $Root }
  $env:NANOQUANT_PORT = "$Port"
  $backend = Join-Path $Root 'backend'
  Write-SupLog "启动进程: $VPython -m nanoquant (port $Port, home $env:NANOQUANT_HOME)"
  return Start-Process -FilePath $VPython `
    -ArgumentList '-m', 'nanoquant' `
    -WorkingDirectory $backend `
    -RedirectStandardOutput $OutLog `
    -RedirectStandardError $ErrLog `
    -PassThru -WindowStyle Hidden
}

$VPython = Resolve-Python
if (-not $VPython) {
  Write-SupLog "未找到 Python 解释器，退出"
  exit 1
}

$restartTimes = New-Object System.Collections.ArrayList

if ($Once) {
  $alive = Test-Health
  Write-SupLog ("单次检查: health={0}, heartbeat={1}s" -f $alive, (Get-HeartbeatAgeSec))
  if (-not $alive) {
    $p = Start-Engine $VPython
    Start-Sleep -Seconds 8
    $alive2 = Test-Health
    Write-SupLog ("已启动 PID {0}, health={1}" -f $p.Id, $alive2)
    exit ([int](-not $alive2))
  }
  exit 0
}

Write-SupLog "守护启动（间隔 ${IntervalSec}s, 端口 $Port）"
while ($true) {
  $alive = Test-Health
  $age = Get-HeartbeatAgeSec

  if (-not $alive) {
    $now = Get-Date
    $recent = @($restartTimes | Where-Object { ($now - $_).TotalSeconds -lt 60 })
    if ($recent.Count -ge 3) {
      Write-SupLog "1 分钟内已重启 3 次，暂停 60s（疑似启动即崩，请看 app.err.log）"
      Start-Sleep -Seconds 60
      continue
    }
    [void]$restartTimes.Add($now)
    try {
      [void](Start-Engine $VPython)
    } catch {
      Write-SupLog "启动失败: $_"
    }
    Start-Sleep -Seconds 10
    continue
  }

  if ($age -gt 300) {
    Write-SupLog "警告: 服务在跑但心跳已 ${age}s 未更新（引擎可能停摆，本次不重启）"
  }

  Start-Sleep -Seconds $IntervalSec
}
