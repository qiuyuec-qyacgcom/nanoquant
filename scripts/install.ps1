<#
.SYNOPSIS
  NanoQuant Windows 一键安装脚本
.DESCRIPTION
  检查 Python / Node, 创建虚拟环境, 安装后端依赖并构建前端.
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\install.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -Port 9000 -SkipFrontend
#>
param(
  [int]$Port = 8899,
  [switch]$SkipFrontend
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)

function Write-Step($msg) { Write-Host "[信息] $msg" -ForegroundColor Cyan }
function Write-Ok($msg) { Write-Host "[完成] $msg" -ForegroundColor Green }
function Write-Warn2($msg) { Write-Host "[注意] $msg" -ForegroundColor Yellow }

Write-Host @"
 _   _             ___                      _
| \ | | __ _ _ __ / _ \ _   _  __ _ _ __   | |_
|  \| |/ _` + "`" + ` | '_ \ | | | | | |/ _` + "`" + ` | '_ \  | __|
| |\  | (_| | | | | |_| | |_| | (_| | | | | |_
|_| \_|\__,_|_| |_|\__\_\__,_|\__,_|_| |_|\__|

  小资金 · 币安 U 本位永续 · 超短线量化终端
"@ -ForegroundColor Cyan

Write-Step "安装目录: $Root"
Write-Step "监听端口: $Port"

# ---------------------------------------------------------------- Python
$python = $null
foreach ($candidate in @('python', 'python3', 'py')) {
  $cmd = Get-Command $candidate -ErrorAction SilentlyContinue
  if ($cmd) {
    $ver = & $candidate -c "import sys;print('%d.%d' % sys.version_info[:2])" 2>$null
    if ($LASTEXITCODE -eq 0 -and [version]$ver -ge [version]'3.10') { $python = $candidate; break }
  }
}
if (-not $python) {
  throw "未找到 Python 3.10+, 请先安装: https://www.python.org/downloads/ (安装时勾选 Add to PATH)"
}
Write-Ok "Python: $(& $python --version 2>&1)"

$Venv = Join-Path $Root '.venv'
if (-not (Test-Path (Join-Path $Venv 'Scripts\python.exe'))) {
  Write-Step "创建虚拟环境 $Venv"
  & $python -m venv $Venv
}
$VPython = Join-Path $Venv 'Scripts\python.exe'

Write-Step "安装后端依赖 (首次可能需要几分钟)"
& $VPython -m pip install --upgrade pip -q
& $VPython -m pip install -q -r (Join-Path $Root 'backend\requirements.txt')
if ($LASTEXITCODE -ne 0) { throw "后端依赖安装失败, 可尝试使用国内镜像: -i https://pypi.tuna.tsinghua.edu.cn/simple" }
Write-Ok "后端依赖就绪"

# ---------------------------------------------------------------- 前端
$Dist = Join-Path $Root 'frontend\dist\index.html'
if (-not $SkipFrontend) {
  if (Test-Path $Dist) {
    Write-Step "已存在前端产物, 跳过构建"
  } else {
    $npm = Get-Command npm -ErrorAction SilentlyContinue
    if ($npm) {
      Write-Step "构建前端 (npm install + build)"
      Push-Location (Join-Path $Root 'frontend')
      try {
        & npm install --no-audit --no-fund
        if ($LASTEXITCODE -ne 0) { throw "npm install 失败" }
        & npm run build
        if ($LASTEXITCODE -ne 0) { throw "npm run build 失败" }
      } finally { Pop-Location }
      Write-Ok "前端构建完成"
    } else {
      Write-Warn2 "未检测到 npm, 跳过前端构建"
      Write-Warn2 "安装 Node 18+ 后执行: cd frontend; npm install; npm run build"
    }
  }
}

# ---------------------------------------------------------------- 目录
foreach ($d in @('data', 'logs', 'data\backups')) {
  New-Item -ItemType Directory -Force -Path (Join-Path $Root $d) | Out-Null
}

Write-Host ""
Write-Host "========================================" -ForegroundColor Green
Write-Host " 安装完成" -ForegroundColor Green
Write-Host "========================================" -ForegroundColor Green
Write-Host ""
Write-Host "  启动: powershell -ExecutionPolicy Bypass -File scripts\start.ps1"
Write-Host "  访问: http://127.0.0.1:$Port"
Write-Host "  账号: admin / admin123  (登录后请立即修改)"
Write-Host ""
Write-Host "  数据目录: $Root\data"
Write-Host "  日志目录: $Root\logs"
Write-Host ""
