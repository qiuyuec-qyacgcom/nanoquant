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
  [string]$Binary = ''
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

function Resolve-Binary {
  $candidates = @()
  if ($Binary) { $candidates += $Binary }
  $candidates += (Join-Path $Root 'rust\nanoquant\target\release\nanoquant.exe')
  $candidates += (Join-Path $Root 'rust\nanoquant\target\release\nanoquant')
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

function Start-Engine($Bin) {
  Rotate-Log $OutLog
  Rotate-Log $ErrLog
  if (-not $env:NANOQUANT_HOME) { $env:NANOQUANT_HOME = $Root }
  $env:NANOQUANT_PORT = "$Port"
  # 执行内核 exec-core（默认必选）：先起内核，主服务靠 EXEC_TCP 完成握手。
  # 内核起不来时主服务仍启动（HTTP/前端可用），但交易引擎不会启动。
  if (-not $env:EXEC_TCP) {
    $coreScript = Join-Path $Root 'scripts\start-core.ps1'
    if (Test-Path $coreScript) {
      & $coreScript -Addr '127.0.0.1:9101' -Detach | Out-Null
      if ($LASTEXITCODE -eq 0) {
        $env:EXEC_TCP = '127.0.0.1:9101'
        Write-SupLog "执行内核已就绪 (127.0.0.1:9101)"
      } else {
        Write-SupLog "执行内核未启动（缺 Secret 或二进制），主服务将启动但交易引擎不会启动"
      }
    }
  }
  Write-SupLog "启动进程: $Bin (port $Port, home $env:NANOQUANT_HOME)"
  return Start-Process -FilePath $Bin `
    -WorkingDirectory $Root `
    -RedirectStandardOutput $OutLog `
    -RedirectStandardError $ErrLog `
    -PassThru -WindowStyle Hidden
}

$Bin = Resolve-Binary
if (-not $Bin) {
  Write-SupLog "未找到 Rust 二进制，退出。请先执行: cargo build --release --manifest-path rust/nanoquant/Cargo.toml"
  exit 1
}

$restartTimes = New-Object System.Collections.ArrayList

if ($Once) {
  $alive = Test-Health
  Write-SupLog ("单次检查: health={0}, heartbeat={1}s" -f $alive, (Get-HeartbeatAgeSec))
  if (-not $alive) {
    $p = Start-Engine $Bin
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
      [void](Start-Engine $Bin)
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
