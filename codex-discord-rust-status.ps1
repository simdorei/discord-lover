[CmdletBinding()]
param(
    [string]$RepoRoot = $PSScriptRoot,
    [string]$BinaryPath
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'scripts/CdrRuntimeIdentity.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'scripts/CdrBuildProvenance.psm1') -Force
$RepoRoot = [IO.Path]::GetFullPath($RepoRoot)
if ([string]::IsNullOrWhiteSpace($BinaryPath)) {
    $BinaryPath = Join-Path $RepoRoot 'target\release\cdr-runtime.exe'
}
$BinaryPath = [IO.Path]::GetFullPath($BinaryPath)
$ModePath = Join-Path $RepoRoot '.codex_discord_runtime'
$DisablePath = Join-Path $RepoRoot '.codex_discord_bot.disabled'
$LockPath = Join-Path $RepoRoot '.codex_discord_rust.runtime.lock'
$HeartbeatPath = Join-Path $RepoRoot '.codex_discord_rust.heartbeat'
$RestartPath = Join-Path $RepoRoot '.codex_discord_rust.restart'
$StopPath = Join-Path $RepoRoot '.codex_discord_rust.stop'

$mode = if (Test-Path -LiteralPath $ModePath -PathType Leaf) {
    (Get-Content -LiteralPath $ModePath -Raw).Trim()
} else { 'unset' }
Write-Output "runtime_mode: $mode"
Write-Output "runtime_disabled: $(Test-Path -LiteralPath $DisablePath -PathType Leaf)"
Write-Output "repo: $RepoRoot"
Write-Output "runtime_binary: $BinaryPath"
Write-Output "runtime_binary_exists: $(Test-Path -LiteralPath $BinaryPath -PathType Leaf)"

$runtimePid = 0
if (Test-Path -LiteralPath $LockPath) {
    $text = Get-Content -LiteralPath $LockPath -Raw -ErrorAction SilentlyContinue
    if ($text -match '(?m)^pid=(\d+)$') {
        $runtimePid = [int]$Matches[1]
    }
}
Write-Output "runtime_lock_pid: $(if ($runtimePid) { $runtimePid } else { 'missing' })"
$process = if ($runtimePid) {
    Get-Process -Id $runtimePid -ErrorAction SilentlyContinue
} else { $null }
$verified = $false
$identityUnavailable = $false
if ($null -ne $process) {
    try {
        $actualPath = [string]$process.Path
        if ([string]::IsNullOrWhiteSpace($actualPath)) {
            $identityUnavailable = $true
        } else {
            $verified = [IO.Path]::GetFullPath($actualPath).Equals(
                $BinaryPath,
                [StringComparison]::OrdinalIgnoreCase
            )
        }
    } catch {
        $identityUnavailable = $true
    }
}
$processStatus = if ($verified) {
    "running verified pid=$runtimePid"
} elseif ($identityUnavailable) {
    "running pid=$runtimePid identity_path_unavailable"
} else {
    'not found or identity mismatch'
}
Write-Output "bot_process: $processStatus"
$startup=$null
if ($verified) {
    $ticks=$process.StartTime.ToUniversalTime().Ticks; $ticks-=($ticks % 10)
    try { $startup=Read-CdrRuntimeStartup $RepoRoot $BinaryPath "$runtimePid|$ticks" }
    catch { Write-Output "running_provenance_error: $($_.Exception.Message)" }
}
Write-Output ('running_build: ' + $(if($startup){$startup | ConvertTo-Json -Depth 5 -Compress}else{'unknown; no matching process-bound startup record'}))
try {
    $artifact=Get-CdrRuntimeArtifact $BinaryPath
    Write-Output ('on_disk_artifact: ' + ($artifact | ConvertTo-Json -Depth 5 -Compress))
} catch { Write-Output "on_disk_artifact_error: $($_.Exception.Message)" }
try {
    $checkout=Get-CdrBuildSourceState $RepoRoot
    Write-Output "checkout_source: $($checkout.Input)"
} catch { Write-Output 'checkout_source: unknown (not a readable Git checkout)' }
if (Test-Path -LiteralPath $HeartbeatPath) {
    $age = [math]::Round(((Get-Date) - (Get-Item -LiteralPath $HeartbeatPath).LastWriteTime).TotalSeconds, 1)
    Write-Output "heartbeat_age_seconds: $age"
} else {
    Write-Output 'heartbeat_age_seconds: missing'
}
Write-Output "restart_marker: $(if (Test-Path -LiteralPath $RestartPath) { 'present' } else { 'missing' })"
Write-Output "stop_marker: $(if (Test-Path -LiteralPath $StopPath) { 'present' } else { 'missing' })"
foreach ($log in @('codex_discord_rust.error.log', 'codex_discord_rust.log', 'discord_launcher.log')) {
    $path = Join-Path $RepoRoot $log
    if (Test-Path -LiteralPath $path) {
        Write-Output ""
        Write-Output "recent_$($log.Replace('.', '_')):"
        Get-Content -LiteralPath $path -Tail 20
    }
}
