# P3 observation sampler (tasks.md 12.1).
#
# Collects the machine-readable daily snapshot defined by
# .codeartsdoer/specs/rust_full_migration/evidence/12-observation-template.json:
# availability, p99 of /engine/status and /engine/positions, p99 of reads during
# a cycle, and the daily contract diff. Human review fields (resend, data_sample,
# alerts) are filled in the same JSON after manual verification.
#
# Requirements:
#   - the Rust kernel is already running at -BaseUrl
#   - NANOQUANT_OBS_PASSWORD (admin password) for authenticated sampling
#   - pwsh 7 or Windows PowerShell 5.1
#
# Exit codes: 0 snapshot written, 1 sampling error, 2 missing credentials/tooling.

param(
    [string]$BaseUrl = "http://127.0.0.1:8899",
    [string]$ApiPrefix = "/api/v1",
    [string]$User = "admin",
    [string]$Password = $env:NANOQUANT_OBS_PASSWORD,
    [int]$P99Samples = 100,
    [int]$AvailabilityChecks = 1,
    [switch]$TriggerCycle,
    [switch]$AvailabilityOnly,
    [string]$OutFile = "",
    [string]$ContractBaseline = ".codeartsdoer/specs/rust_full_migration/baseline/contract_baseline.json",
    [string]$Binary = ""
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path $PSScriptRoot -Parent
$evidenceDir = Join-Path $repoRoot ".codeartsdoer/specs/rust_full_migration/evidence"
$templatePath = Join-Path $evidenceDir "12-observation-template.json"
$date = (Get-Date).ToString("yyyy-MM-dd")
if ([string]::IsNullOrWhiteSpace($OutFile)) {
    $OutFile = Join-Path $evidenceDir "12-observation-daily-$date.json"
}

function New-Snapshot {
    if (-not (Test-Path $templatePath)) {
        throw "template not found: $templatePath"
    }
    return Get-Content -LiteralPath $templatePath -Raw -Encoding UTF8 | ConvertFrom-Json
}

function Read-Snapshot {
    if (Test-Path -LiteralPath $OutFile) {
        return Get-Content -LiteralPath $OutFile -Raw -Encoding UTF8 | ConvertFrom-Json
    }
    return New-Snapshot
}

function Test-Health {
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $response = Invoke-WebRequest -Uri "$BaseUrl$ApiPrefix/health" -UseBasicParsing -TimeoutSec 10
        $watch.Stop()
        return @{ ok = ($response.StatusCode -ge 200 -and $response.StatusCode -lt 300); ms = $watch.Elapsed.TotalMilliseconds }
    }
    catch {
        $watch.Stop()
        return @{ ok = $false; ms = $watch.Elapsed.TotalMilliseconds }
    }
}

function Get-Token {
    if ([string]::IsNullOrWhiteSpace($Password)) {
        Write-Error "NANOQUANT_OBS_PASSWORD is required for authenticated sampling"
        exit 2
    }
    $body = @{ username = $User; password = $Password } | ConvertTo-Json
    $response = Invoke-RestMethod -Method Post -Uri "$BaseUrl$ApiPrefix/auth/login" `
        -Body $body -ContentType "application/json" -TimeoutSec 15
    if ([string]::IsNullOrWhiteSpace($response.token)) {
        throw "login did not return a token"
    }
    return $response.token
}

function Invoke-TimedGet {
    param([string]$Path, [string]$Token, [int]$Count)
    $values = New-Object System.Collections.Generic.List[double]
    for ($i = 0; $i -lt $Count; $i++) {
        $watch = [System.Diagnostics.Stopwatch]::StartNew()
        Invoke-WebRequest -Uri "$BaseUrl$ApiPrefix$Path" -UseBasicParsing -TimeoutSec 10 `
            -Headers @{ Authorization = "Bearer $Token" } | Out-Null
        $watch.Stop()
        $values.Add($watch.Elapsed.TotalMilliseconds)
    }
    return $values.ToArray()
}

function Get-P99 {
    param([double[]]$Values)
    if ($null -eq $Values -or $Values.Count -eq 0) { return 0.0 }
    $sorted = $Values | Sort-Object
    $index = [Math]::Ceiling(0.99 * $sorted.Count) - 1
    if ($index -lt 0) { $index = 0 }
    return [double]$sorted[$index]
}

function Resolve-Binary {
    if (-not [string]::IsNullOrWhiteSpace($Binary)) { return $Binary }
    $candidates = @(
        (Join-Path $repoRoot "rust/nanoquant/target/release/nanoquant.exe"),
        (Join-Path $repoRoot "rust/nanoquant/target/release/nanoquant")
    )
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) { return $candidate }
    }
    return ""
}

function Update-Verdict {
    param($Snapshot)
    $blockers = New-Object System.Collections.Generic.List[string]
    if (-not $Snapshot.availability.pass) { $blockers.Add("availability") }
    foreach ($name in @("engine_status", "engine_positions", "cycle_read")) {
        if (-not $Snapshot.p99.$name.pass) { $blockers.Add("p99.$name") }
    }
    foreach ($name in @("resend", "contract_diff", "data_sample", "alerts")) {
        if (-not $Snapshot.$name.pass) { $blockers.Add($name) }
    }
    $Snapshot.verdict.pass = ($blockers.Count -eq 0)
    $Snapshot.verdict.blockers = @($blockers)
}

$snapshot = Read-Snapshot
$snapshot.schema_version = 1
$snapshot.date = $date
$now = [int64][DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
if ($snapshot.window.start_ts -eq 0) { $snapshot.window.start_ts = $now }
$snapshot.window.end_ts = $now

$health = Test-Health
$snapshot.availability.checks = [int]$snapshot.availability.checks + [Math]::Max(1, $AvailabilityChecks)
if (-not $health.ok) { $snapshot.availability.failures = [int]$snapshot.availability.failures + 1 }
$ratio = if ($snapshot.availability.checks -gt 0) {
    ($snapshot.availability.checks - $snapshot.availability.failures) / $snapshot.availability.checks
} else { 0.0 }
$snapshot.availability.ratio = [Math]::Round($ratio, 5)
$snapshot.availability.pass = ($snapshot.availability.ratio -ge $snapshot.availability.threshold)

if (-not $AvailabilityOnly) {
    $token = Get-Token

    $statusSamples = Invoke-TimedGet -Path "/engine/status" -Token $token -Count $P99Samples
    $snapshot.p99.engine_status.samples = $statusSamples.Count
    $snapshot.p99.engine_status.p99_ms = [Math]::Round((Get-P99 $statusSamples), 2)
    $snapshot.p99.engine_status.pass = ($snapshot.p99.engine_status.p99_ms -le $snapshot.p99.engine_status.threshold_ms)

    $positionSamples = Invoke-TimedGet -Path "/engine/positions" -Token $token -Count $P99Samples
    $snapshot.p99.engine_positions.samples = $positionSamples.Count
    $snapshot.p99.engine_positions.p99_ms = [Math]::Round((Get-P99 $positionSamples), 2)
    $snapshot.p99.engine_positions.pass = ($snapshot.p99.engine_positions.p99_ms -le $snapshot.p99.engine_positions.threshold_ms)

    if ($TriggerCycle) {
        Invoke-WebRequest -Method Post -Uri "$BaseUrl$ApiPrefix/engine/cycle-now" -UseBasicParsing -TimeoutSec 15 `
            -Headers @{ Authorization = "Bearer $token" } | Out-Null
        $cycleSamples = Invoke-TimedGet -Path "/engine/status" -Token $token -Count $P99Samples
        $snapshot.p99.cycle_read.samples = $cycleSamples.Count
        $snapshot.p99.cycle_read.p99_ms = [Math]::Round((Get-P99 $cycleSamples), 2)
        $snapshot.p99.cycle_read.pass = ($snapshot.p99.cycle_read.p99_ms -le $snapshot.p99.cycle_read.threshold_ms)
    }

    $binary = Resolve-Binary
    $baselinePath = Join-Path $repoRoot $ContractBaseline
    if ([string]::IsNullOrWhiteSpace($binary) -or -not (Test-Path -LiteralPath $baselinePath)) {
        Write-Warning "contract diff skipped: binary or baseline missing"
        $snapshot.contract_diff.pass = $false
        $snapshot.contract_diff.report = "skipped: binary or baseline missing"
    }
    else {
        $previousHome = $env:NANOQUANT_HOME
        $env:NANOQUANT_HOME = $repoRoot
        try {
            $raw = & $binary ops contract-diff --baseline $baselinePath --format json
            $diff = $raw | ConvertFrom-Json
            $snapshot.contract_diff.missing = [int]$diff.missing.Count
            $snapshot.contract_diff.extra = [int]$diff.extra.Count
            $snapshot.contract_diff.report = (Join-Path $evidenceDir "12-contract-diff-$date.json")
            $raw | Set-Content -LiteralPath $snapshot.contract_diff.report -Encoding UTF8
            $snapshot.contract_diff.pass = ($snapshot.contract_diff.missing -eq 0)
        }
        finally {
            if ($null -eq $previousHome) { Remove-Item Env:NANOQUANT_HOME -ErrorAction SilentlyContinue }
            else { $env:NANOQUANT_HOME = $previousHome }
        }
    }
}

$start = [DateTimeOffset]::FromUnixTimeSeconds([int64]$snapshot.window.start_ts).LocalDateTime
$snapshot.window.duration_hours = [Math]::Round(((Get-Date) - $start).TotalHours, 2)

Update-Verdict $snapshot

$json = $snapshot | ConvertTo-Json -Depth 10
$dir = Split-Path $OutFile -Parent
if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
$json | Set-Content -LiteralPath $OutFile -Encoding UTF8

Write-Output "snapshot=$OutFile"
Write-Output ("availability={0:P2} p99_status={1}ms p99_positions={2}ms p99_cycle={3}ms verdict_pass={4}" -f `
    $snapshot.availability.ratio, $snapshot.p99.engine_status.p99_ms, `
    $snapshot.p99.engine_positions.p99_ms, $snapshot.p99.cycle_read.p99_ms, $snapshot.verdict.pass)

if ($snapshot.verdict.pass) { exit 0 } else { exit 0 }