<#
.SYNOPSIS
  启动执行内核 exec-core（Windows，Rust，默认组件，签名校验 + 指标聚合）

.DESCRIPTION
  Windows 没有 unix socket，用回环 TCP（默认 127.0.0.1:9101）。
  主服务通过 EXEC_TCP 找到它并完成握手；握手失败时交易引擎不启动。

  密钥来源（按顺序）：
    1. -Secret 显式传入；
    2. 参数页配置好当前交易环境的 API 凭证后，从数据库读取（ops show-exec-secret）。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\start-core.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\start-core.ps1 -Detach
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\start-core.ps1 -Stop
#>
param(
  [string]$Addr = '127.0.0.1:9101',
  [string]$Secret = '',
  [switch]$Detach,
  [switch]$Stop
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$DataDir = Join-Path $Root 'data'
$PidFile = Join-Path $DataDir 'exec-core.pid'
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

function Resolve-CoreBinary {
  $candidates = @(
    (Join-Path $Root 'rust\exec-core\target\release\exec-core.exe'),
    (Join-Path $Root 'rust\exec-core\target\debug\exec-core.exe')
  )
  return $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}

if ($Stop) {
  if (Test-Path $PidFile) {
    $old = Get-Content $PidFile -Raw
    if ($old) {
      $pidValue = [int]$old.Trim()
      Stop-Process -Id $pidValue -Force -ErrorAction SilentlyContinue
      Write-Host "执行内核已停止 (PID $pidValue)" -ForegroundColor Cyan
    }
    Remove-Item $PidFile -Force -ErrorAction SilentlyContinue
  } else {
    Write-Host '没有运行中的执行内核记录' -ForegroundColor Yellow
  }
  exit 0
}

$bin = Resolve-CoreBinary
if (-not $bin) {
  Write-Host "未找到 exec-core 二进制：$Root\rust\exec-core\target\release\exec-core.exe" -ForegroundColor Yellow
  Write-Host '请先构建: cargo build --release --manifest-path rust/exec-core/Cargo.toml' -ForegroundColor Yellow
  exit 1
}

if (-not $Secret) {
  $app = Join-Path $Root 'rust\nanoquant\target\release\nanoquant.exe'
  if (Test-Path $app) {
    $env:NANOQUANT_HOME = $Root
    try {
      $Secret = (& $app ops show-exec-secret 2>$null | Select-Object -First 1)
    } catch {
      $Secret = ''
    }
  }
}

if (-not $Secret) {
  Write-Host '未找到交易所 Secret：先在参数页填好 API 凭证，或用 -Secret <secret> 传入' -ForegroundColor Yellow
  exit 2
}

$env:EXEC_TCP = $Addr
$env:EXEC_SECRET = $Secret

if (-not $Detach) {
  Write-Host "执行内核启动中... 监听 $Addr" -ForegroundColor Cyan
  & $bin
  exit $LASTEXITCODE
}

$outLog = Join-Path $DataDir 'exec-core.out.log'
$errLog = Join-Path $DataDir 'exec-core.err.log'
$proc = Start-Process -FilePath $bin -WorkingDirectory $Root -PassThru -WindowStyle Hidden -RedirectStandardOutput $outLog -RedirectStandardError $errLog
Set-Content -Path $PidFile -Value $proc.Id -Encoding ASCII

$ready = $false
$port = [int]($Addr.Split(':')[-1])
for ($i = 0; $i -lt 30; $i++) {
  Start-Sleep -Milliseconds 200
  try {
    $client = New-Object System.Net.Sockets.TcpClient
    $client.Connect('127.0.0.1', $port)
    $client.Close()
    $ready = $true
    break
  } catch {
    # 还没监听上，继续等
  }
}

if ($ready) {
  Write-Host "执行内核已启动 (PID $($proc.Id))，监听 $Addr" -ForegroundColor Green
  Write-Host "主服务需要 EXEC_TCP=$Addr 才能完成握手" -ForegroundColor DarkGray
  exit 0
}

Write-Host "执行内核启动失败，详见 $errLog" -ForegroundColor Red
exit 1