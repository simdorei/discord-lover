[CmdletBinding()]
param(
    [string]$RepoRoot = $PSScriptRoot,
    [string]$BinaryPath,
    [string]$EnvPath,
    [switch]$LibraryOnly,
    [switch]$DryRun,
    [switch]$LogHealthy,
    [switch]$CheckRestartReady,
    [switch]$PrepareRestart,
    [switch]$ForceRestart,
    [string]$CompleteRestartFenceJson,
    [string]$RecoverDeploymentStatePath,
    [string]$MaintenanceStatePath,
    [string]$ExpectedMaintenanceOperation,
    [string]$ExpectedRuntimeIdentity,
    [int]$RestartQuietSeconds = 90,
    [int]$RestartWaitTimeoutSeconds = 900,
    [int]$HealthCpuPercent = 95,
    [int]$HealthFreeMemoryMb = 768,
    [int]$HealthHeartbeatMaxAgeSeconds = 45,
    [int]$HealthHeartbeatStartupGraceSeconds = 120,
    [int]$HealthBadSampleLimit = 2
)

$ErrorActionPreference = 'Stop'
$RepoRoot = [IO.Path]::GetFullPath($RepoRoot)
if ([string]::IsNullOrWhiteSpace($BinaryPath)) {
    $BinaryPath = Join-Path $RepoRoot 'target\release\cdr-runtime.exe'
}
if ([string]::IsNullOrWhiteSpace($EnvPath)) {
    $EnvPath = Join-Path $RepoRoot '.env'
}
$BinaryPath = [IO.Path]::GetFullPath($BinaryPath)
$LockPath = Join-Path $RepoRoot '.codex_discord_rust.runtime.lock'
$HeartbeatPath = Join-Path $RepoRoot '.codex_discord_rust.heartbeat'
$RestartPath = Join-Path $RepoRoot '.codex_discord_rust.restart'
$DrainIdentityPath = Join-Path $RepoRoot '.codex_discord_rust.drain.identity'
$DrainPreparePath = Join-Path $RepoRoot '.codex_discord_rust.drain.prepare'
$DrainAckPath = Join-Path $RepoRoot '.codex_discord_rust.drain.ack'
$StopPath = Join-Path $RepoRoot '.codex_discord_rust.stop'
$DisablePath = Join-Path $RepoRoot '.codex_discord_bot.disabled'
$LauncherLog = Join-Path $RepoRoot 'discord_launcher.log'
$StdoutLog = Join-Path $RepoRoot 'codex_discord_rust.log'
$StderrLog = Join-Path $RepoRoot 'codex_discord_rust.error.log'
$watchdogProcess = [Diagnostics.Process]::GetCurrentProcess()
try {
    $watchdogTicks = $watchdogProcess.StartTime.ToUniversalTime().Ticks
    $watchdogTicks -= ($watchdogTicks % 10)
} finally {
    $watchdogProcess.Dispose()
}
$RestartClaimPath = Join-Path $RepoRoot ".codex_discord_rust.restart.claimed.$PID.$watchdogTicks"
$SupportRoot = if ($LibraryOnly) { $PSScriptRoot } else { $RepoRoot }
$DrainSupportPath = Join-Path $SupportRoot 'codex-discord-rust-drain.ps1'
if (-not (Test-Path -LiteralPath $DrainSupportPath -PathType Leaf)) {
    throw "Rust restart drain support was not found: $DrainSupportPath"
}
. $DrainSupportPath
. (Join-Path $SupportRoot 'codex-discord-rust-control.ps1')
. (Join-Path $SupportRoot 'scripts/CdrDeploymentRecovery.ps1')
. (Join-Path $SupportRoot 'scripts/CdrLaunchJournal.ps1')
. (Join-Path $SupportRoot 'scripts/CdrRestartTransaction.ps1')
. (Join-Path $SupportRoot 'scripts/CdrForceRestart.ps1')
. (Join-Path $SupportRoot 'scripts/CdrMaintenanceCompatibility.ps1')

if (
    $HealthCpuPercent -ne 95 -or
    $HealthFreeMemoryMb -ne 768 -or
    $HealthBadSampleLimit -ne 2
) {
    throw (
        'Rust watchdog supports heartbeat health only; ' +
        'CPU, free-memory, and bad-sample thresholds are not supported.'
    )
}
if ($HealthHeartbeatMaxAgeSeconds -lt 1) {
    throw 'HealthHeartbeatMaxAgeSeconds must be at least 1.'
}
if ($HealthHeartbeatStartupGraceSeconds -lt 0) {
    throw 'HealthHeartbeatStartupGraceSeconds cannot be negative.'
}
if ($RestartQuietSeconds -lt 0 -or $RestartWaitTimeoutSeconds -lt 0) {
    throw 'Restart quiet and wait timeout values cannot be negative.'
}

function Write-RustWatchdogLog {
    param([string]$Message)
    Add-Content -LiteralPath $LauncherLog -Encoding UTF8 -Value "[$((Get-Date).ToString('s'))] rust_watchdog $Message"
}

function Get-RuntimePid {
    if (-not (Test-Path -LiteralPath $LockPath -PathType Leaf)) {
        return 0
    }
    $text = Get-Content -LiteralPath $LockPath -Raw -ErrorAction SilentlyContinue
    if ($text -match '(?m)^pid=(\d+)$') {
        return [int]$Matches[1]
    }
    return 0
}

function Get-RustProcessIdentity {
    param($Process)
    if ($null -eq $Process) { return '' }
    try {
        $ticks = $Process.StartTime.ToUniversalTime().Ticks
        $ticks -= ($ticks % 10)
        return "$([int]$Process.Id)|$ticks"
    } catch {
        return ''
    }
}

function Get-VerifiedRustProcessById {
    param([int]$ProcessId)
    if ($ProcessId -le 0) { return $null }
    $process = Get-Process -Id $ProcessId -ErrorAction SilentlyContinue
    if ($null -eq $process) { return $null }
    try {
        $actualPath = [string]$process.Path
        if ([string]::IsNullOrWhiteSpace($actualPath)) { return $null }
        if (-not [IO.Path]::GetFullPath($actualPath).Equals(
            $BinaryPath,
            [StringComparison]::OrdinalIgnoreCase
        )) { return $null }
        return $process
    } catch {
        return $null
    }
}

function Get-VerifiedRuntimeProcess {
    return Get-VerifiedRustProcessById -ProcessId (Get-RuntimePid)
}

function Get-VerifiedRuntimeIdentity {
    $process = Get-VerifiedRuntimeProcess
    return Get-RustProcessIdentity -Process $process
}

function Get-BoundRestartIdentity {
    param([string]$MarkerPath)
    if (-not (Test-Path -LiteralPath $MarkerPath -PathType Leaf)) { return '' }
    $text = Get-Content -LiteralPath $MarkerPath -Raw -ErrorAction SilentlyContinue
    if ($text -match '(?m)^identity=(\d+\|\d+)$') {
        return [string]$Matches[1]
    }
    return ''
}

function Get-HeartbeatHealth {
    param($Process)
    $processAge = [math]::Max(0, ((Get-Date) - $Process.StartTime).TotalSeconds)
    $withinBootstrap = $processAge -le $HealthHeartbeatStartupGraceSeconds
    $state = 'heartbeat_missing'
    $age = $null
    if (Test-Path -LiteralPath $HeartbeatPath -PathType Leaf) {
        $text = Get-Content -LiteralPath $HeartbeatPath -Raw -ErrorAction SilentlyContinue
        if ($text -notmatch '(?m)^pid=(\d+)$') {
            $state = 'heartbeat_pid_invalid'
        } elseif ([int]$Matches[1] -ne [int]$Process.Id) {
            $state = 'heartbeat_pid_mismatch'
        } elseif ($text -notmatch '(?m)^updated_at=(\d+)$') {
            $state = 'heartbeat_timestamp_invalid'
        } else {
            try {
                $updated = [DateTimeOffset]::FromUnixTimeSeconds([long]$Matches[1])
                $age = ([DateTimeOffset]::UtcNow - $updated).TotalSeconds
                $startedSecond = ([DateTimeOffset]$Process.StartTime.ToUniversalTime()).ToUnixTimeSeconds()
                if ($age -ge 0 -and $updated.ToUnixTimeSeconds() -ge $startedSecond -and
                    $age -le $HealthHeartbeatMaxAgeSeconds) {
                    return [pscustomobject]@{
                        Healthy = $true
                        Bootstrap = $false
                        State = 'healthy'
                        AgeSeconds = $age
                    }
                }
                $state = 'heartbeat_stale'
            } catch {
                $state = 'heartbeat_timestamp_invalid'
            }
        }
    }
    return [pscustomobject]@{
        Healthy = $withinBootstrap
        Bootstrap = $withinBootstrap
        State = $state
        AgeSeconds = $age
    }
}

function Test-HeartbeatHealthy {
    param($Process)
    return [bool](Get-HeartbeatHealth -Process $Process).Healthy
}

function Wait-RustRuntimeExit {
    param(
        $Process,
        [string]$ExpectedIdentity,
        [string]$Reason,
        [int]$TimeoutSeconds = 30
    )
    if ($null -eq $Process) { return }
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        if ($Process.HasExited) {
            Write-RustWatchdogLog "graceful_exit reason=$Reason identity=$ExpectedIdentity"
            return
        }
        $current = Get-RustProcessIdentity -Process (
            Get-VerifiedRustProcessById -ProcessId ([int]$Process.Id)
        )
        if ($current -ne $ExpectedIdentity) {
            Write-RustWatchdogLog "graceful_exit reason=$Reason identity=$ExpectedIdentity replacement=$current"
            return
        }
        Start-Sleep -Milliseconds 250
        $Process.Refresh()
    }
    throw "graceful_exit_timeout reason=$Reason identity=$ExpectedIdentity timeout_seconds=$TimeoutSeconds"
}

function Stop-VerifiedRuntime {
    param(
        $Process,
        [string]$ExpectedIdentity
    )
    if ($null -eq $Process) { return }
    $current = Get-RustProcessIdentity -Process (
        Get-VerifiedRustProcessById -ProcessId ([int]$Process.Id)
    )
    if ($current -ne $ExpectedIdentity) {
        Write-RustWatchdogLog "forced_stop_refused expected=$ExpectedIdentity current=$current"
        return
    }
    $Process.Kill()
    if (-not $Process.WaitForExit(15000)) {
        throw "Rust runtime did not stop within 15 seconds: identity=$ExpectedIdentity"
    }
}

function Clear-DeadRuntimeArtifacts {
    param([switch]$PreserveRestartDrain)
    $lockedPid = Get-RuntimePid
    if ($lockedPid -le 0) {
        if (-not $PreserveRestartDrain) { Clear-OrphanedRestartDrainArtifacts }
        return
    }
    if ($null -ne (Get-Process -Id $lockedPid -ErrorAction SilentlyContinue)) {
        throw "Runtime lock belongs to a live process that could not be verified: pid=$lockedPid"
    }
    $currentLockPid = Get-RuntimePid
    if ($currentLockPid -ne $lockedPid) { return }
    Remove-Item -LiteralPath $LockPath -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $HeartbeatPath -PathType Leaf) {
        $heartbeat = Get-Content -LiteralPath $HeartbeatPath -Raw -ErrorAction SilentlyContinue
        if ($heartbeat -notmatch '(?m)^pid=(\d+)$' -or [int]$Matches[1] -eq $lockedPid) {
            Remove-Item -LiteralPath $HeartbeatPath -Force -ErrorAction SilentlyContinue
        }
    }
    Write-RustWatchdogLog "stale_runtime_artifacts_removed pid=$lockedPid"
    if (-not $PreserveRestartDrain) { Clear-OrphanedRestartDrainArtifacts }
}

function Wait-RustThreadsQuietForRestart {
    $arguments = @(
        '--restart-readiness',
        '--restart-quiet-seconds',
        [string]$RestartQuietSeconds,
        '--restart-wait-timeout-seconds',
        [string]$RestartWaitTimeoutSeconds,
        '--env',
        $EnvPath
    )
    Push-Location -LiteralPath $RepoRoot
    try {
        & $BinaryPath @arguments
        $probeExitCode = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    if ($probeExitCode -ne 0) {
        throw "Rust restart readiness probe failed with exit code $probeExitCode"
    }
}

function Claim-RestartRequest {
    if (-not (Test-Path -LiteralPath $RestartPath -PathType Leaf)) { return '' }
    try {
        Move-Item -LiteralPath $RestartPath -Destination $RestartClaimPath -ErrorAction Stop
        return $RestartClaimPath
    } catch {
        if (-not (Test-Path -LiteralPath $RestartPath)) { return '' }
        throw
    }
}

function Restore-RestartRequest {
    param([string]$ClaimPath)
    if (-not (Test-Path -LiteralPath $ClaimPath -PathType Leaf)) { return }
    if (Test-Path -LiteralPath $RestartPath) { return }
    Move-Item -LiteralPath $ClaimPath -Destination $RestartPath -ErrorAction Stop
}

function Start-RustRuntime {
    param([switch]$ResumeRemoteMcp, [DateTimeOffset]$DeadlineUtc = [DateTimeOffset]::MaxValue)
    $script:RustRestartStartedProcess = $null
    $script:CdrTrayStartedIdentity = ''
    if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
        throw "Rust runtime binary was not found: $BinaryPath"
    }
    if (-not (Test-Path -LiteralPath $EnvPath -PathType Leaf)) {
        throw "Rust runtime environment file was not found: $EnvPath"
    }
    $quotedEnv = '"' + $EnvPath.Replace('"', '\"') + '"'
    $resumeName = 'CODEX_REMOTE_MCP_RESTART_RESUME'
    $previousResume = [Environment]::GetEnvironmentVariable($resumeName, 'Process')
    try {
        $resumeValue = if ($ResumeRemoteMcp) { '1' } else { $null }
        [Environment]::SetEnvironmentVariable($resumeName, $resumeValue, 'Process')
        if ([DateTimeOffset]::UtcNow -ge $DeadlineUtc) { throw 'maintenance_deadline_exceeded_before_launch' }
        Invoke-CdrProductionCheckedLaunch -DeadlineUtc $DeadlineUtc -Launch {
        Set-CdrLaunchStarting
        if ([DateTimeOffset]::UtcNow -ge $DeadlineUtc) { throw 'maintenance_deadline_exceeded_before_process_start' }
        $process = Start-Process -FilePath $BinaryPath `
            -ArgumentList @('--env', $quotedEnv) `
            -WorkingDirectory $RepoRoot `
            -RedirectStandardOutput $StdoutLog `
            -RedirectStandardError $StderrLog `
            -WindowStyle Hidden `
            -PassThru
        $script:RustRestartStartedProcess = $process
        Set-CdrLaunchChild -Process $process
        $deadline = [DateTimeOffset]::UtcNow.AddSeconds(15)
        if ($DeadlineUtc -lt $deadline) { $deadline=$DeadlineUtc }
        while ([DateTimeOffset]::UtcNow -lt $deadline) {
            if ($process.HasExited) {
                $details = if (Test-Path -LiteralPath $StderrLog) {
                    (Get-Content -LiteralPath $StderrLog -Tail 20) -join [Environment]::NewLine
                } else { '' }
                throw "Rust runtime exited during startup with code $($process.ExitCode). $details"
            }
            $verified = Get-VerifiedRuntimeProcess
            if ($null -ne $verified -and [int]$verified.Id -eq $process.Id) {
                Write-RustWatchdogLog "started identity=$(Get-RustProcessIdentity -Process $verified)"
                # Memory only: UI work must not spend the maintenance launch budget.
                $script:CdrTrayStartedIdentity = Get-RustProcessIdentity -Process $verified
                return
            }
            Start-Sleep -Milliseconds 250
        }
        throw "Rust runtime did not publish its verified lock marker within 15 seconds: pid=$($process.Id)"
        }
    } finally {
        [Environment]::SetEnvironmentVariable($resumeName, $previousResume, 'Process')
    }
}

if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
    throw "Rust runtime binary was not found: $BinaryPath"
}

if ($LibraryOnly) { return }
# CONTROL_ENTRY: tests execute this boundary with fixture process providers.
$script:CdrTrayStartedIdentity = ''
$script:CdrTrayPollAllowed = $false
$script:CdrTrayObservedIdentity = ''
$forceGuard = $null
if ($ForceRestart -and -not $DryRun) {
    if ($MaintenanceStatePath -or $RecoverDeploymentStatePath -or $PrepareRestart -or
        $CheckRestartReady -or $CompleteRestartFenceJson) { throw 'force_restart_mode_conflict' }
    if ($ExpectedRuntimeIdentity -and (Get-VerifiedRuntimeIdentity) -cne $ExpectedRuntimeIdentity) {
        throw 'Force restart target identity changed.'
    }
    $forceGuard = Enter-CdrForceGate -Root $RepoRoot
}
$controlGuard = $null
try {
if ($null -ne $forceGuard) {
    $controlGuard = Enter-CdrForceControl -Root $RepoRoot
} else {
    $purpose = if ($MaintenanceStatePath -or $RecoverDeploymentStatePath) { 'maintenance' }
        elseif ([IO.File]::Exists((Join-Path $RepoRoot '.codex_discord_rust.force.launch'))) { 'force_restart' }
        else { 'watchdog' }
    $controlGuard = Enter-CdrControl -Root $RepoRoot -MaintenanceV2:([bool]$MaintenanceStatePath) -Purpose $purpose
}
if ($ForceRestart -or [IO.File]::Exists((Join-Path $RepoRoot '.codex_discord_rust.force.launch'))) {
    Invoke-CdrForceRestart -ExpectedIdentity $ExpectedRuntimeIdentity -Preview:$DryRun
    exit 0
}
if ($MaintenanceStatePath) {
    if ($RecoverDeploymentStatePath -or $PrepareRestart -or $CheckRestartReady -or $CompleteRestartFenceJson -or $DryRun) {
        throw 'maintenance_v2_mode_conflict'
    }
    foreach ($module in @('State','Command','Backup','Failure','Actions','Launch','Notification','Schedule','Engine')) {
        . (Join-Path $RepoRoot ('scripts/CdrMaintenance'+$module+'.ps1'))
    }
    Invoke-CdrMaintenanceEngine -StatePath $MaintenanceStatePath -ExpectedOperation $ExpectedMaintenanceOperation
    exit 0
}
Assert-CdrNoMaintenanceV2 -Root $RepoRoot
if ($RecoverDeploymentStatePath) {
    Invoke-CdrDeploymentRecovery -StatePath $RecoverDeploymentStatePath
    exit 0
}
$pendingRestart = Read-CdrLaunchJournal (Join-Path $RepoRoot '.codex_discord_rust.restart.launch')
if ($null -ne $pendingRestart) {
    if ($CompleteRestartFenceJson -and
        -not (Test-RestartDrainFenceMatch ($CompleteRestartFenceJson | ConvertFrom-Json) $pendingRestart.Fence)) {
        throw 'Completion request differs from the pending launch journal'
    }
    if ($DryRun) { Write-Output 'restart_transaction_pending'; exit 0 }
    Invoke-CdrRestartTransaction -Fence $pendingRestart.Fence
    exit 0
}
$completionFence = $null
Assert-CdrNoOrphanRestartClaim -Root $RepoRoot
if ($CompleteRestartFenceJson) {
    $completionFence = $CompleteRestartFenceJson | ConvertFrom-Json
    if (Test-CdrRestartCompleted -Fence $completionFence) {
        Write-Output 'restart_completed_verified'
        exit 0
    }
    $published = Get-RestartDrainFence -Path $RestartPath
    if (-not (Test-RestartDrainFenceMatch $published $completionFence)) {
        throw 'Restart handoff has neither an exact pending fence nor a verified completion receipt'
    }
}
$running = Get-VerifiedRuntimeProcess
$runningIdentity = Get-RustProcessIdentity -Process $running
if (-not $DryRun -and -not $CheckRestartReady -and -not $PrepareRestart -and
    -not (Test-Path -LiteralPath $DisablePath) -and -not (Test-Path -LiteralPath $StopPath) -and
    -not (Test-Path -LiteralPath $RestartPath -PathType Leaf) -and
    (Test-Path -LiteralPath $DrainPreparePath -PathType Leaf) -and $null -ne $running) {
    $PrepareRestart = $true
    $ExpectedRuntimeIdentity = $runningIdentity
    Write-RustWatchdogLog "restart_drain_resume identity=$runningIdentity"
}
if ($CheckRestartReady -or $PrepareRestart) {
    foreach ($path in @($DisablePath, $StopPath, $RestartPath)) {
        if (Test-Path -LiteralPath $path) {
            throw 'Existing maintenance marker preserved; explicit preparation refused before drain'
        }
    }
    if ($null -eq $running) { throw 'Rust runtime is not running.' }
    if (-not [string]::IsNullOrWhiteSpace($ExpectedRuntimeIdentity) -and
        $ExpectedRuntimeIdentity -ne $runningIdentity) {
        throw 'Rust restart preparation no longer matches the requested process.'
    }
    $health = Get-HeartbeatHealth -Process $running
    if (-not $health.Healthy -or $health.Bootstrap) {
        throw "Rust runtime heartbeat is not ready: state=$($health.State)"
    }
    $fence = Enter-RestartDrain -Process $running `
        -ExpectedProcessIdentity $runningIdentity `
        -TimeoutSeconds $RestartWaitTimeoutSeconds
    Assert-RestartDrainBound -Fence $fence -ExpectedProcessIdentity $runningIdentity
    Wait-RustThreadsQuietForRestart
    Assert-RestartDrainBound -Fence $fence -ExpectedProcessIdentity $runningIdentity
    if ((Get-VerifiedRuntimeIdentity) -ne $runningIdentity) {
        throw 'Rust process changed while validating restart readiness.'
    }
    $health = Get-HeartbeatHealth -Process (Get-VerifiedRuntimeProcess)
    if (-not $health.Healthy -or $health.Bootstrap) {
        throw "Rust runtime heartbeat changed while validating restart readiness: state=$($health.State)"
    }
    if ($PrepareRestart) {
        Write-BoundRestartMarker -Fence $fence
        Write-Output 'restart_prepare_ok'
        Write-Output ('restart_prepare_fence=' + ($fence | ConvertTo-Json -Compress))
    } else {
        Write-Output 'restart_check_ok runtime_remains_sealed=true'
    }
    exit 0
}

# Deployment owns its stop marker until its identity-bound recovery takes over.
if (Test-Path -LiteralPath $DisablePath) {
    if ($DryRun) { Write-Output 'disabled' }
    elseif (Test-Path -LiteralPath $StopPath) {
        if ($null -ne $running) {
            Wait-RustRuntimeExit -Process $running -ExpectedIdentity $runningIdentity `
                -Reason 'maintenance_stop_requested'
        }
        # Do not consume another maintenance owner's markers or start/kill anything.
    } elseif ($LogHealthy) { Write-RustWatchdogLog 'disabled' }
    exit 0
}
if (Test-Path -LiteralPath $StopPath) {
    if ($DryRun) { Write-Output 'stop_requested'; exit 0 }
    if ($null -ne $running) {
        Wait-RustRuntimeExit `
            -Process $running `
            -ExpectedIdentity $runningIdentity `
            -Reason 'stop_requested'
    } else {
        Clear-DeadRuntimeArtifacts
    }
    Remove-Item -LiteralPath $StopPath -Force
    Write-RustWatchdogLog 'stopped_by_request'
    exit 0
}

if (Test-Path -LiteralPath $DisablePath) {
    if ($DryRun) { Write-Output 'disabled' }
    elseif ($LogHealthy) { Write-RustWatchdogLog 'disabled' }
    exit 0
}

$restart = Test-Path -LiteralPath $RestartPath -PathType Leaf
$restartFence = if ($restart) { Get-RestartDrainFence -Path $RestartPath } else { $null }
$restartIdentity = if ($null -ne $restartFence) { $restartFence.ProcessIdentity } else { '' }
$health = if ($null -ne $running) { Get-HeartbeatHealth -Process $running } else { $null }
$unhealthy = $null -ne $running -and -not $health.Healthy
if ($DryRun) {
    if ($restart -and [string]::IsNullOrWhiteSpace($restartIdentity)) {
        Write-Output 'would_reject_unbound_restart'
    } elseif ($restart -and $null -ne $running -and $restartIdentity -ne $runningIdentity) {
        Write-Output 'would_ignore_stale_restart'
    } elseif ($restart) {
        Write-Output 'would_restart_requested'
    } elseif ($null -eq $running) {
        Write-Output 'would_start'
    } elseif ($unhealthy) {
        Write-Output 'would_restart_unhealthy'
    } elseif ($health.Bootstrap) {
        Write-Output "running_bootstrap state=$($health.State)"
    } else {
        Write-Output 'running'
    }
    exit 0
}

if ($restart) {
    if ([string]::IsNullOrWhiteSpace($restartIdentity)) {
        throw 'Rust restart marker is missing a verified process identity.'
    }
    $restartAck = Get-RestartDrainFence -Path $DrainAckPath -RequireSealed
    if (-not (Test-RestartDrainFenceMatch $restartAck $restartFence)) {
        throw 'Rust restart marker does not have an exact sealed drain acknowledgement.'
    }
    if ($null -ne $running -and $restartIdentity -ne $runningIdentity) {
        throw "Restart targets another instance; marker preserved: expected=$restartIdentity current=$runningIdentity"
    }
    if ($null -ne $running) {
        Write-RustWatchdogLog "restart reason=requested identity=$runningIdentity"
        Wait-RustRuntimeExit `
            -Process $running `
            -ExpectedIdentity $runningIdentity `
            -Reason 'restart_requested'
    } else {
        Clear-DeadRuntimeArtifacts -PreserveRestartDrain
    }
    Invoke-CdrRestartTransaction -Fence $restartFence
    exit 0
}

if ($unhealthy) {
    Write-RustWatchdogLog "restart reason=stale_heartbeat identity=$runningIdentity state=$($health.State)"
    Stop-VerifiedRuntime -Process $running -ExpectedIdentity $runningIdentity
    if ((Get-VerifiedRuntimeIdentity) -eq $runningIdentity) {
        throw "Unhealthy Rust runtime remained alive: identity=$runningIdentity"
    }
    Start-RustRuntime
    $script:CdrTrayPollAllowed = $true
    exit 0
}

if ($null -eq $running) {
    Clear-DeadRuntimeArtifacts
    Start-RustRuntime
} elseif ($LogHealthy) {
    Write-RustWatchdogLog "healthy identity=$runningIdentity bootstrap=$($health.Bootstrap)"
}
$script:CdrTrayObservedIdentity = $runningIdentity
$script:CdrTrayPollAllowed = $true
exit 0
} finally {
    if ($null -ne $controlGuard) { $controlGuard.Dispose() }
    if ($null -ne $forceGuard) { $forceGuard.Dispose() }
    # Only an ordinary successful watchdog reaches this UI branch. Dedicated
    # maintenance/recovery/check/prepare/completion and their reentry never do.
    if ($script:CdrTrayPollAllowed) {
        try {
            . (Join-Path $RepoRoot 'codex-discord-tray-runtime.ps1')
            $tray=Invoke-CdrTrayAfterWatchdog -Root $RepoRoot `
                -StartedIdentity $script:CdrTrayStartedIdentity -ObservedIdentity $script:CdrTrayObservedIdentity
            if ($tray.State -notin @('no_request','already_attempted')) {
                Write-RustWatchdogLog "tray_bootstrap state=$($tray.State) pid=$($tray.Pid)"
            }
        } catch {
            try { Write-RustWatchdogLog "tray_bootstrap_unknown type=$($_.Exception.GetType().FullName)" } catch { }
        }
    }
}
