[CmdletBinding()]
param(
    [string]$RepoRoot,
    [switch]$DryRun,
    [switch]$Immediate,
    [switch]$Force,
    [switch]$ForceWorker,
    [switch]$Deferred,
    [string]$ExpectedBotIdentity,
    [string]$ExpectedBinaryHash,
    [string]$ExpectedSourceCommit,
    [int]$DelaySeconds = 10,
    [int]$QuietSeconds = 90,
    [int]$WaitTimeoutSeconds = 900
)

$ErrorActionPreference = 'Stop'
$MinimumDelaySeconds = 15
$MinimumQuietSeconds = 15
if ([string]::IsNullOrWhiteSpace($RepoRoot)) { $RepoRoot = $PSScriptRoot }
$RepoRoot = [IO.Path]::GetFullPath($RepoRoot)
$Watchdog = Join-Path $RepoRoot 'codex-discord-rust-watchdog.ps1'
$RuntimeLock = Join-Path $RepoRoot '.codex_discord_rust.runtime.lock'
$RestartLog = Join-Path $RepoRoot 'codex-discord-rust-restart.log'
$BinaryPath = [IO.Path]::GetFullPath(
    (Join-Path $RepoRoot 'target\release\cdr-runtime.exe')
)
Import-Module (Join-Path $RepoRoot 'scripts/CdrRuntimeIdentity.psm1') -Force
# Capture once, before any deferred/independent handoff or readiness wait.
$artifact=Get-CdrRuntimeArtifact $BinaryPath $ExpectedBinaryHash $ExpectedSourceCommit
$ExpectedBinaryHash=$artifact.ArtifactHash
$EffectiveDelaySeconds = [math]::Max($MinimumDelaySeconds, $DelaySeconds)
$EffectiveQuietSeconds = [math]::Max($MinimumQuietSeconds, $QuietSeconds)
if ($WaitTimeoutSeconds -lt 0) {
    throw 'WaitTimeoutSeconds cannot be negative.'
}
if (-not (Test-Path -LiteralPath $Watchdog -PathType Leaf)) {
    throw "Rust watchdog was not found: $Watchdog"
}

function Write-RustRestartLog {
    param([string]$Message)
    $safeMessage = $Message -replace '[\r\n]+', ' '
    $line = "$([DateTime]::UtcNow.ToString('o')) $safeMessage`r`n"
    [IO.File]::AppendAllText($RestartLog, $line, [Text.UTF8Encoding]::new($false))
}

function Get-VerifiedRustIdentity {
    if (-not (Test-Path -LiteralPath $RuntimeLock -PathType Leaf)) { return '' }
    $lockText = Get-Content -LiteralPath $RuntimeLock -Raw -ErrorAction SilentlyContinue
    if ($lockText -notmatch '(?m)^pid=(\d+)$') { return '' }
    $process = Get-Process -Id ([int]$Matches[1]) -ErrorAction SilentlyContinue
    if ($null -eq $process) { return '' }
    try {
        $actualPath = [string]$process.Path
        if ([string]::IsNullOrWhiteSpace($actualPath)) { return '' }
        if (-not [IO.Path]::GetFullPath($actualPath).Equals(
            $BinaryPath,
            [StringComparison]::OrdinalIgnoreCase
        )) { return '' }
        $ticks = $process.StartTime.ToUniversalTime().Ticks
        $ticks -= ($ticks % 10)
        return "$([int]$process.Id)|$ticks"
    } catch {
        return ''
    }
}

function Invoke-RestartReadinessCheck {
    param([string]$Identity)
    $null=Get-CdrRuntimeArtifact $BinaryPath $ExpectedBinaryHash $ExpectedSourceCommit
    # Preflight is read-only. A bad/stale thread must not seal live intake.
    Push-Location -LiteralPath $RepoRoot
    $previousPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        & $BinaryPath --restart-readiness `
            --restart-quiet-seconds $EffectiveQuietSeconds `
            --restart-wait-timeout-seconds $WaitTimeoutSeconds `
            --env (Join-Path $RepoRoot '.env') | Out-Host
        $preflightExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previousPreference
        Pop-Location
    }
    if ($preflightExitCode -ne 0) {
        throw "Rust restart preflight failed with exit code $preflightExitCode; intake left open"
    }
    if ((Get-VerifiedRustIdentity) -ne $Identity) {
        throw 'Rust process changed during restart preflight; no drain requested'
    }
    $prepareOutput = & $Watchdog `
        -RepoRoot $RepoRoot `
        -BinaryPath $BinaryPath `
        -PrepareRestart `
        -ExpectedRuntimeIdentity $Identity `
        -ExpectedBinaryHash $ExpectedBinaryHash -ExpectedSourceCommit $ExpectedSourceCommit `
        -RestartQuietSeconds $EffectiveQuietSeconds `
        -RestartWaitTimeoutSeconds $WaitTimeoutSeconds
    if ($LASTEXITCODE -ne 0) {
        throw "Rust restart readiness check failed with exit code $LASTEXITCODE"
    }
    $receipts = @($prepareOutput | Where-Object { "$_" -like 'restart_prepare_fence=*' })
    if ($receipts.Count -ne 1) { throw 'Restart preparation returned no unique fence receipt' }
    $fence = $receipts[0].Substring('restart_prepare_fence='.Length) | ConvertFrom-Json
    if ($fence.ProcessIdentity -cne $Identity -or
        $fence.RuntimeId -notmatch '^[A-Za-z0-9_-]{1,128}$' -or
        $fence.Nonce -notmatch '^[A-Za-z0-9_-]{1,128}$') {
        throw 'Restart preparation receipt does not match the requested identity'
    }
    return $fence
}

function Invoke-BoundRustRestart {
    param([string]$Identity)
    if ($Identity -notmatch '^\d+\|\d+$') {
        throw 'Rust restart is missing the expected bot identity.'
    }
    if ($EffectiveDelaySeconds -gt 0) {
        Start-Sleep -Seconds $EffectiveDelaySeconds
    }
    if ((Get-VerifiedRustIdentity) -ne $Identity) {
        throw 'restart no longer matches the requested Rust process'
    }
    $fence = Invoke-RestartReadinessCheck -Identity $Identity
    # A successful Prepare authorizes the old process to disappear immediately.
    # Complete only that exact fence; never invoke generic health recovery here.
    & $Watchdog `
        -RepoRoot $RepoRoot `
        -BinaryPath $BinaryPath `
        -CompleteRestartFenceJson ($fence | ConvertTo-Json -Compress) `
        -ExpectedBinaryHash $ExpectedBinaryHash -ExpectedSourceCommit $ExpectedSourceCommit `
        -RestartQuietSeconds $EffectiveQuietSeconds `
        -RestartWaitTimeoutSeconds $WaitTimeoutSeconds
    if ($LASTEXITCODE -ne 0) {
        throw "Rust watchdog restart failed with exit code $LASTEXITCODE"
    }
}

if ($Force) {
    # Emergency means no delay, quiet probe, drain acknowledgement or turn wait.
    try {
        . (Join-Path $RepoRoot 'scripts/CdrForceRestart.ps1')
        $forceIdentity = Get-VerifiedRustIdentity
        if ($ExpectedBotIdentity -and $forceIdentity -cne $ExpectedBotIdentity) {
            throw 'Force restart target identity changed.'
        }
        if (-not $DryRun -and (Test-CdrCallerInsideRuntime $forceIdentity)) {
            if ($ForceWorker) { throw 'Independent force worker is still inside the bot process tree.' }
            $workerPid = Start-CdrDetachedForceRestart -Root $RepoRoot -Identity $forceIdentity -ExpectedBinaryHash $ExpectedBinaryHash -ExpectedSourceCommit $ExpectedSourceCommit
            Write-RustRestartLog "force_restart_handed_off worker_pid=$workerPid identity=$forceIdentity"
            Write-Output "force_restart_handed_off worker_pid=$workerPid active_work_wait=false"
            exit 0
        }
        & $Watchdog -RepoRoot $RepoRoot -BinaryPath $BinaryPath -ForceRestart `
            -ExpectedRuntimeIdentity $ExpectedBotIdentity -ExpectedBinaryHash $ExpectedBinaryHash -ExpectedSourceCommit $ExpectedSourceCommit -DryRun:$DryRun
        exit $LASTEXITCODE
    } catch {
        Write-RustRestartLog "force_restart_failed error=$($_.Exception.Message)"
        Add-Content -LiteralPath (Join-Path $RepoRoot 'discord_launcher.log') -Encoding UTF8 `
            -Value "[$((Get-Date).ToString('s'))] force_restart_failed details=codex-discord-rust-restart.log"
        throw
    }
}

if ($DryRun) {
    if (
        -not [string]::IsNullOrWhiteSpace($ExpectedBotIdentity) -and
        (Get-VerifiedRustIdentity) -ne $ExpectedBotIdentity
    ) {
        throw 'Dry-run no longer matches the requested Rust process.'
    }
    & $Watchdog `
        -RepoRoot $RepoRoot `
        -BinaryPath $BinaryPath `
        -DryRun `
        -RestartQuietSeconds $EffectiveQuietSeconds
    exit $LASTEXITCODE
}

if ($Deferred) {
    try {
        Invoke-BoundRustRestart -Identity $ExpectedBotIdentity
        Write-RustRestartLog "restart_completed runtime=rust installed_sha256=$ExpectedBinaryHash"
        Write-Output "restart_completed: runtime=rust installed_sha256=$ExpectedBinaryHash"
        exit 0
    } catch {
        $message = $_.Exception.Message
        Write-RustRestartLog "restart_failed error=$message"
        throw
    }
}

$currentIdentity = Get-VerifiedRustIdentity
if ([string]::IsNullOrWhiteSpace($currentIdentity)) {
    throw 'Running Rust bot process identity could not be verified.'
}
if (
    -not [string]::IsNullOrWhiteSpace($ExpectedBotIdentity) -and
    $ExpectedBotIdentity -ne $currentIdentity
) {
    throw 'Provided expected identity does not match the running Rust process.'
}
$expectedIdentity = if (
    [string]::IsNullOrWhiteSpace($ExpectedBotIdentity)
) { $currentIdentity } else { $ExpectedBotIdentity }

if ($Immediate) {
    Invoke-BoundRustRestart -Identity $expectedIdentity
    Write-Output (
        "restart_completed: runtime=rust quiet_seconds=$EffectiveQuietSeconds " +
        "wait_timeout_seconds=$WaitTimeoutSeconds installed_sha256=$ExpectedBinaryHash"
    )
    exit 0
}

$escapedScript = $PSCommandPath.Replace("'", "''")
$escapedRoot = $RepoRoot.Replace("'", "''")
$command = (
    "& '$escapedScript' -RepoRoot '$escapedRoot' -Deferred " +
    "-ExpectedBotIdentity '$expectedIdentity' -DelaySeconds $EffectiveDelaySeconds " +
    "-ExpectedBinaryHash '$ExpectedBinaryHash' -ExpectedSourceCommit '$ExpectedSourceCommit' " +
    "-QuietSeconds $EffectiveQuietSeconds -WaitTimeoutSeconds $WaitTimeoutSeconds"
)
Start-Process -FilePath 'powershell.exe' `
    -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-Command', $command) `
    -WindowStyle Hidden
Write-Output (
    "restart_queued: runtime=rust delay_seconds=$EffectiveDelaySeconds " +
    "quiet_seconds=$EffectiveQuietSeconds wait_timeout_seconds=$WaitTimeoutSeconds"
)
exit 0
