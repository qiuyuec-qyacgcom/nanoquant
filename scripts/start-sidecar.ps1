<#
.SYNOPSIS
  启动执行内核 sidecar（Windows，Rust 版，只签名不下单）

.DESCRIPTION
  Windows 没有 unix socket，用回环 TCP（默认 127.0.0.1:9101）。
  主服务通过 EXEC_TCP 找到它，系统诊断里的「执行内核」才会显示已连接。

  密钥来源（按顺序）：
    1. -Secret 显式传入；
    2. 参数页配置好当前交易环境的 API 凭证后，从数据库读取（ops show-exec-secret）。

  说明：sidecar 只持有 Secret 并回传签名，不接触账户、持仓与订单；
  Rust 内核直连交易所下单，不部署 sidecar 也能正常交易。

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\start-sidecar.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\start-sidecar.ps1 -Detach
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\start-sidecar.ps1 -Stop
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
$PidFile = Join-Path $DataDir 'exec-sidecar.pid'
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

function Resolve-SidecarBinary {
  $candidates = @(
    (Join-Path $Root 'rust\exec-service\target\release\exec-service.exe'),
    (Join-Path $Root 'rust\exec-service\target\debug\exec-service.exe')
  )
  return $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
}

if ($Stop) {
  if (Test-Path $PidFile) {
    $old = Get-Content $PidFile -Raw
    if ($old) {
      $pidValue = [int]$old.Trim()
      Stop-Process -Id $pidValue -Force -ErrorAction SilentlyContinue
      Write-Host "执行内核 sidecar 已停止 (PID $pidValue)" -ForegroundColor Cyan
    }
    Remove-Item $PidFile -Force -ErrorAction SilentlyContinue
  } else {
    Write-Host '没有运行中的 sidecar 记录' -ForegroundColor Yellow
  }
  exit 0
}

$bin = Resolve-SidecarBinary
if (-not $bin) {
  Write-Host "未找到 exec-service 二进制：$Root\rust\exec-service\target\release\exec-service.exe" -ForegroundColor Yellow
  Write-Host '请先构建: cargo build --release --manifest-path rust/exec-service/Cargo.toml' -ForegroundColor Yellow
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
  Write-Host "执行内核 sidecar 启动中... 监听 $Addr" -ForegroundColor Cyan
  & $bin
  exit $LASTEXITCODE
}

$outLog = Join-Path $DataDir 'exec-sidecar.out.log'
$errLog = Join-Path $DataDir 'exec-sidecar.err.log'
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
  Write-Host "执行内核 sidecar 已启动 (PID $($proc.Id))，监听 $Addr" -ForegroundColor Green
  Write-Host "主服务需要 EXEC_TCP=$Addr 才能在系统诊断里看到它" -ForegroundColor DarkGray
  exit 0
}

Write-Host "执行内核 sidecar 启动失败，详见 $errLog" -ForegroundColor Red
exit 1
