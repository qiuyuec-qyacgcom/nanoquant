<#
.SYNOPSIS
  NanoQuant Windows 一键安装脚本
.DESCRIPTION
  检查 Python 3.12+ / Node 18+，创建虚拟环境，安装后端依赖（按哈希跳过重复安装）并构建前端。
  端口会写进 data\.port，start.ps1 会沿用，不必每次手动传 -Port。
.PARAMETER Python
  指定 python.exe 路径（PATH 里没有 python 时用）
.PARAMETER Port
  监听端口，默认 8899
.PARAMETER SkipFrontend
  跳过前端构建（已有 frontend\dist 或只需要后端 API）
.PARAMETER Mirror
  pip 镜像：cn / tsinghua / aliyun，或完整 URL
.PARAMETER Password
  指定初始管理员密码（至少 8 位）。不传则首次启动随机生成，并打印在启动日志里
.PARAMETER Force
  忽略 requirements 哈希，强制重装依赖
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\install.ps1
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -Port 9000 -SkipFrontend
.EXAMPLE
  powershell -ExecutionPolicy Bypass -File scripts\install.ps1 -Python C:\Python312\python.exe -Mirror cn
#>
param(
  [ValidateRange(1, 65535)][int]$Port = 8899,
  [switch]$SkipFrontend,
  [string]$Python = '',
  [string]$Mirror = $env:PIP_INDEX_URL,
  [string]$Password = $env:NANOQUANT_ADMIN_PASSWORD,
  [switch]$Force
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)

function Write-Step($msg) { Write-Host "[信息] $msg" -ForegroundColor Cyan }
function Write-Ok($msg) { Write-Host "[完成] $msg" -ForegroundColor Green }
function Write-Warn2($msg) { Write-Host "[注意] $msg" -ForegroundColor Yellow }
function Fail($msg) { Write-Host "[失败] $msg" -ForegroundColor Red; exit 1 }

switch ($Mirror) {
  'cn' { $Mirror = 'https://pypi.tuna.tsinghua.edu.cn/simple' }
  'tsinghua' { $Mirror = 'https://pypi.tuna.tsinghua.edu.cn/simple' }
  'aliyun' { $Mirror = 'https://mirrors.aliyun.com/pypi/simple' }
}

Write-Host @'
 _   _             ___                      _
| \ | | __ _ _ __ / _ \ _   _  __ _ _ __   | |_
|  \| |/ _` | '_ \ | | | | | |/ _` | '_ \  | __|
| |\  | (_| | | | | |_| | |_| | (_| | | | | |_
|_| \_|\__,_|_| |_|\__\_\__,_|\__,_|_| |_|\__|

  小资金 · 币安 U 本位永续 · 超短线量化终端
'@ -ForegroundColor Cyan

Write-Step "安装目录: $Root"
Write-Step "监听端口: $Port"
if ($Password) {
  if ($Password.Length -lt 8) { Fail "-Password 至少 8 位（这是唯一能进后台的凭据）" }
  Write-Step "初始管理员密码: 使用你指定的值（不回显）"
} else {
  Write-Step "初始管理员密码: 首次启动随机生成，打印在启动日志里"
}

# ---------------------------------------------------------------- 磁盘空间
# SQLite / 备份 / 日志都写在同一块盘，空间不足会变成"盘中写入失败"，比启动失败更难查
try {
  $drive = (Get-Item $Root).PSDrive.Name
  $freeGB = [math]::Round((Get-PSDrive $drive).Free / 1GB, 1)
  if ($freeGB -lt 2) {
    Write-Warn2 "可用空间仅 $freeGB GB（建议 ≥ 2 GB）：备份/日志/SQLite 都写在这里"
  }
} catch { }

# ---------------------------------------------------------------- Python 3.12+
# 探测顺序：-Python → 已有 .venv → py/py -3.x → PATH 上的 python → 常见安装目录
# 之所以要看 .venv：机器上可能根本没把 python 加进 PATH，但虚拟环境里那个是真 Python，
# 重复执行安装脚本时不该因为"找不到 python 命令"就失败。
$probe = 'import sys; raise SystemExit(0 if sys.version_info[:2] >= (3, 12) else 1)'
$Venv = Join-Path $Root '.venv'
$VenvPy = Join-Path $Venv 'Scripts\python.exe'

function Test-Py([string]$exe, [string[]]$pre = @()) {
  if (-not $exe) { return $false }
  if (-not (Get-Command $exe -ErrorAction SilentlyContinue) -and -not (Test-Path $exe)) { return $false }
  try {
    & $exe @pre -c $probe 2>$null | Out-Null
    return ($LASTEXITCODE -eq 0)
  } catch {
    return $false
  }
}

$pyExe = ''
$pyPre = @()
$pyFromVenv = $false

if ($Python -and (Test-Py $Python)) {
  $pyExe = $Python
} elseif ((Test-Path $VenvPy) -and (Test-Py $VenvPy)) {
  $pyExe = $VenvPy
  $pyFromVenv = $true
} else {
  foreach ($cand in @(@('py', @('-3.13')), @('py', @('-3.12')), @('python', @()), @('python3', @()))) {
    if (Test-Py $cand[0] $cand[1]) { $pyExe = $cand[0]; $pyPre = $cand[1]; break }
  }
}

if (-not $pyExe) {
  # PATH 上找不到时扫一遍常见安装位置（官方安装器默认装在 LOCALAPPDATA\Programs\Python）
  $bases = @(
    (Join-Path $env:LOCALAPPDATA 'Programs\Python'),
    $env:PROGRAMFILES,
    ${env:PROGRAMFILES(x86)},
    'C:\'
  ) | Where-Object { $_ -and (Test-Path $_) }
  foreach ($base in $bases) {
    $found = Get-ChildItem $base -Directory -Filter 'Python3*' -ErrorAction SilentlyContinue |
      Sort-Object Name -Descending
    foreach ($dir in $found) {
      $candidate = Join-Path $dir.FullName 'python.exe'
      if ((Test-Path $candidate) -and (Test-Py $candidate)) { $pyExe = $candidate; break }
    }
    if ($pyExe) { break }
  }
}

if (-not $pyExe) {
  Fail "未找到 Python 3.12+。请安装 https://www.python.org/downloads/（勾选 Add to PATH），或 winget install Python.Python.3.12，装好后可用 -Python 指定路径"
}

$pyVersion = (& $pyExe @pyPre --version 2>&1)
Write-Ok "Python: $pyVersion  ($pyExe)"
if ($pyFromVenv) { Write-Step "复用 .venv 里的解释器（系统 PATH 里没有 python 属正常）" }

# ---------------------------------------------------------------- 虚拟环境
if (-not (Test-Path $VenvPy)) {
  Write-Step "创建虚拟环境 $Venv"
  & $pyExe @pyPre -m venv $Venv
  if ($LASTEXITCODE -ne 0) { Fail "venv 创建失败" }
} else {
  Write-Step "复用已有虚拟环境 $Venv"
}

# ---------------------------------------------------------------- 后端依赖
$Req = Join-Path $Root 'backend\requirements.txt'
$HashFile = Join-Path $Venv '.requirements.sha256'
$NewHash = (Get-FileHash $Req -Algorithm SHA256).Hash
$OldHash = ''
if (Test-Path $HashFile) { $OldHash = (Get-Content $HashFile -Raw).Trim() }

$pipArgs = @('-q')
if ($Mirror) { $pipArgs += @('-i', $Mirror) }

if (-not $Force -and $OldHash -eq $NewHash) {
  Write-Step "依赖未变（requirements.txt 哈希一致），跳过安装；要强制重装加 -Force"
} else {
  Write-Step "安装后端依赖（首次可能要几分钟）"
  if ($Mirror) { Write-Step "pip 镜像: $Mirror" }
  & $VenvPy -m pip install --upgrade pip @pipArgs 2>$null | Out-Null
  & $VenvPy -m pip install @pipArgs -r $Req
  if ($LASTEXITCODE -ne 0) { Fail "后端依赖安装失败：可加 -Mirror cn 换源重试，或检查网络/代理" }
  Set-Content -Path $HashFile -Value $NewHash -Encoding ascii
  Write-Ok "后端依赖就绪"
}

# 装完自检：能 import 才算真的齐（镜像包没下全的情况确实存在）
Push-Location (Join-Path $Root 'backend')
try {
  & $VenvPy -c "import nanoquant.main"
  if ($LASTEXITCODE -ne 0) { Fail "依赖装完但 import 失败，请把上面的报错贴出来" }
} finally {
  Pop-Location
}
Write-Ok "后端可导入（nanoquant.main）"

# ---------------------------------------------------------------- 前端构建
$Dist = Join-Path $Root 'frontend\dist\index.html'
if (-not $SkipFrontend) {
  if (Test-Path $Dist) {
    Write-Step "已存在前端产物，跳过构建（要重建请先删 frontend\dist）"
  } elseif (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
    Write-Warn2 "未安装 Node.js/npm，跳过前端构建"
    Write-Warn2 "面板打不开但 API 正常；装好 Node 18+ 后执行: cd frontend; npm ci; npm run build"
  } else {
    $nodeMajor = 0
    try { $nodeMajor = [int](& node -p "process.versions.node.split('.')[0]") } catch { $nodeMajor = 0 }
    if ($nodeMajor -lt 18) {
      Write-Warn2 "Node 版本过低（当前 $(& node -v)），前端需要 18+，已跳过构建"
    } else {
      Write-Step "构建前端（npm ci + build）"
      Push-Location (Join-Path $Root 'frontend')
      try {
        if (Test-Path (Join-Path $Root 'frontend\package-lock.json')) {
          & npm ci --no-audit --no-fund
        } else {
          & npm install --no-audit --no-fund
        }
        if ($LASTEXITCODE -ne 0) { Fail "npm 安装依赖失败" }
        & npm run build
        if ($LASTEXITCODE -ne 0) { Fail "npm run build 失败" }
      } finally {
        Pop-Location
      }
      Write-Ok "前端构建完成（frontend\dist）"
    }
  }
}

# ---------------------------------------------------------------- 运行目录
foreach ($d in @('data', 'logs', 'data\backups')) {
  New-Item -ItemType Directory -Force -Path (Join-Path $Root $d) | Out-Null
}
# 端口持久化：-Port 要真的被 start.ps1 沿用，否则只是个"提示里好看"的参数
Set-Content -Path (Join-Path $Root 'data\.port') -Value $Port -Encoding ascii
# 初始密码：安装阶段还没有数据库，只能留成文件让首次启动读取并哈希入库，
# 启动成功后该文件会被删掉（明文不留在盘上）
if ($Password) {
  $secretFile = Join-Path $Root 'data\.admin_password'
  [System.IO.File]::WriteAllText($secretFile, $Password, (New-Object System.Text.UTF8Encoding($false)))
  Write-Ok "已暂存初始密码（首次启动写入数据库后自动删除）"
}
Write-Ok "运行目录就绪（data、logs、data\backups）"

Write-Host ""
Write-Host "========================================" -ForegroundColor Green
Write-Host " 安装完成" -ForegroundColor Green
Write-Host "========================================" -ForegroundColor Green
Write-Host ""
Write-Host "  访问地址 : http://127.0.0.1:$Port"
Write-Host "  默认账号 : admin"
if ($Password) {
  Write-Host "  初始密码 : 你安装时指定的值"
  Write-Host ""
  Write-Host "  登录后建议改成新的强口令（首次登录不强制）。" -ForegroundColor Yellow
} else {
  Write-Host "  初始密码 : 随机生成，见启动日志"
  Write-Host ""
  Write-Host "  启动时会在控制台与 logs\nanoquant.log 打印密码，登录后必须改密。" -ForegroundColor Yellow
}
Write-Host ""
Write-Host "  改密前后台除登录/改密/健康检查外的接口一律 403。" -ForegroundColor Yellow
Write-Host ""
Write-Host "  启动     : powershell -ExecutionPolicy Bypass -File scripts\start.ps1"
Write-Host "  停止     : powershell -ExecutionPolicy Bypass -File scripts\stop.ps1"
Write-Host "  健康检查 : curl http://127.0.0.1:$Port/api/v1/health"
Write-Host ""
Write-Host "  数据目录 : $Root\data     (SQLite / 备份 / 缓存)"
Write-Host "  日志目录 : $Root\logs"
Write-Host ""
Write-Host "  首次使用建议顺序:"
Write-Host "   1. 用初始密码登录，立即改成自己的强口令（随机密码首次登录会被强制）"
Write-Host "   2. 「参数配置 → 数据源」填币安 API Key（只开合约交易，关闭提币）"
Write-Host "   3. 「参数配置 → 系统 → 告警」配 Webhook / Telegram，否则出事只能在页面里看"
Write-Host "   4. 「AI 模型」接入 DeepSeek / Qwen / GPT 等 OpenAI 兼容模型并点「诊断」"
Write-Host "   5. 「标的池」添加合约 → 「总览」以 DEMO 模式启动 → 观察后再切 LIVE"
Write-Host ""
