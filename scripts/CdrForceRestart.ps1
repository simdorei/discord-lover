# Emergency restart runs outside the bot/app-server and never asks either to drain.
# Process handles pin creation identities. Never kill by image name or delete a lock
# while its owner is alive. The normal watchdog resumes an interrupted launch.
function Test-CdrCallerInsideRuntime {
    param([string]$Identity)
    if ($Identity -notmatch '^(\d+)\|(\d+)$') { return $false }
    $runtimePid=[int]$Matches[1]; $runtimeTicks=[long]$Matches[2]
    $inventory=@(Get-CimInstance Win32_Process -Property ProcessId,ParentProcessId,CreationDate)
    $cursor=$inventory | Where-Object ProcessId -eq $PID | Select-Object -First 1
    for ($i=0; $i -lt 64 -and $null -ne $cursor; $i++) {
        if ($cursor.ProcessId -eq $runtimePid) {
            return [math]::Abs(($cursor.CreationDate.ToUniversalTime().Ticks - $runtimeTicks)) -le 10
        }
        $parent=$inventory | Where-Object ProcessId -eq $cursor.ParentProcessId | Select-Object -First 1
        if ($null -eq $parent -or $parent.CreationDate -gt $cursor.CreationDate) { return $false }
        $cursor=$parent
    }
    return $false
}

function Start-CdrDetachedForceRestart {
    param([string]$Root, [string]$Identity, [string]$ExpectedBinaryHash, [string]$ExpectedSourceCommit)
    if ($Identity -notmatch '^\d+\|\d+$') { throw 'Detached force restart requires an exact runtime identity.' }
    if ($ExpectedBinaryHash -cnotmatch '^[A-F0-9]{64}$' -or ($ExpectedSourceCommit -and $ExpectedSourceCommit -cnotmatch '^[a-f0-9]{40}$')) { throw 'Detached force restart requires a pinned artifact.' }
    $entry=Join-Path $Root 'codex-discord-rust-restart.ps1'
    if (-not [IO.File]::Exists($entry)) { throw 'Force restart entry was not found.' }
    # WMI starts a local worker under the OS provider, outside the bot's process
    # ancestry/job. Start-Process alone would inherit the tree being terminated.
    $environment=[string[]]@([Environment]::GetEnvironmentVariables('Process').GetEnumerator() |
        ForEach-Object { [string]$_.Key + '=' + [string]$_.Value })
    $startup=New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{
        ShowWindow=[uint16]0; EnvironmentVariables=$environment
    }
    $command='"' + (Join-Path $PSHOME 'powershell.exe') + '" -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "' +
        $entry + '" -RepoRoot "' + $Root + '" -Force -ForceWorker -ExpectedBotIdentity "' + $Identity + '" -ExpectedBinaryHash "' + $ExpectedBinaryHash + '"'
    if($ExpectedSourceCommit){$command+=' -ExpectedSourceCommit "'+$ExpectedSourceCommit+'"'}
    $result=Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
        CommandLine=$command; CurrentDirectory=$Root; ProcessStartupInformation=$startup
    }
    if ($result.ReturnValue -ne 0 -or $result.ProcessId -le 0) {
        throw "Independent force worker could not start: code=$($result.ReturnValue)"
    }
    return [int]$result.ProcessId
}

function Enter-CdrForceGate {
    param([string]$Root)
    Assert-CdrNoMaintenanceV2 -Root $Root
    try { return [IO.File]::Open((Join-Path $Root '.codex_discord_rust.force.lock'), 'OpenOrCreate', 'ReadWrite', 'None') }
    catch [IO.IOException] { throw 'A force restart is already running.' }
}

function Enter-CdrForceControl {
    param([string]$Root)
    try { return Enter-CdrControl -Root $Root -Emergency -Purpose 'force_restart' }
    catch { if ($_.Exception.Message -notlike 'cdr_control_busy:*') { throw } }

    # An ordinary watchdog can hold control for 900 seconds waiting for a turn.
    # Preempt that exact controller; deployment/binary replacement is not a turn.
    $path = Join-Path $Root '.codex_discord_rust.control.lock'
    $stream = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    $reader = [IO.StreamReader]::new($stream)
    try { $owner = $reader.ReadToEnd() | ConvertFrom-Json }
    finally { $reader.Dispose() }
    if ($owner.Version -ne 1 -or $owner.Root -ine $Root -or $owner.Purpose -cne 'watchdog' -or
        $owner.ProcessId -eq $PID -or $owner.StartTicks -notmatch '^\d+$') {
        throw 'Control belongs to an unverified or deployment owner; no process was stopped.'
    }
    $process = Get-Process -Id ([int]$owner.ProcessId) -ErrorAction SilentlyContinue
    if ($null -ne $process) {
        try {
            [void]$process.Handle
            $identity = Get-RustProcessIdentity $process
            if ($identity -cne "$($owner.ProcessId)|$($owner.StartTicks)" -or
                $process.Path -ine $owner.Executable -or
                [IO.Path]::GetFileName($process.Path) -notin @('powershell.exe', 'pwsh.exe')) {
                throw 'Control owner identity changed; force cancellation refused.'
            }
            # If the waiter released control while we inspected it, take the lock
            # instead of terminating a process that no longer owns the operation.
            try { return Enter-CdrControl -Root $Root -Emergency -Purpose 'force_restart' }
            catch { if ($_.Exception.Message -notlike 'cdr_control_busy:*') { throw } }
            Write-RustWatchdogLog "force_cancel_restart_wait controller=$identity"
            $process.Kill()
            if (-not $process.WaitForExit(5000)) { throw 'Restart controller did not exit after termination.' }
        } finally { $process.Dispose() }
    }
    return Enter-CdrControl -Root $Root -Emergency -Purpose 'force_restart'
}

function Get-CdrOwnedProcessHandles {
    param($RootProcess)
    # Snapshot ancestry while the verified root is alive; pin each discovered PID
    # and reject entries whose creation time no longer matches the snapshot.
    [void]$RootProcess.Handle
    $inventory = @(Get-CimInstance Win32_Process -Property ProcessId,ParentProcessId,CreationDate)
    $owned = [Collections.Generic.List[object]]::new()
    $owned.Add($RootProcess)
    for ($i = 0; $i -lt $owned.Count; $i++) {
        $parent = $owned[$i]
        foreach ($entry in $inventory | Where-Object { $_.ParentProcessId -eq $parent.Id }) {
            if ($entry.ProcessId -eq $PID) { throw 'Force restart helper is inside the target process tree.' }
            $child = Get-Process -Id ([int]$entry.ProcessId) -ErrorAction SilentlyContinue
            if ($null -eq $child) { continue }
            try {
                [void]$child.Handle
                $started = $child.StartTime.ToUniversalTime()
                if ($started -lt $parent.StartTime.ToUniversalTime() -or
                    [math]::Abs(($started - $entry.CreationDate.ToUniversalTime()).Ticks) -gt 10) {
                    $child.Dispose(); continue
                }
                $owned.Add($child)
            } catch { $child.Dispose() }
        }
    }
    return ,$owned
}

function Stop-CdrOwnedTreeNow {
    param($Process, [string]$ExpectedIdentity)
    $owned = Get-CdrOwnedProcessHandles -RootProcess $Process
    try {
        if ((Get-RustProcessIdentity $Process) -cne $ExpectedIdentity -or
            (Get-VerifiedRuntimeIdentity) -cne $ExpectedIdentity) {
            throw 'Runtime identity changed before force stop; no process was stopped.'
        }
        Write-RustWatchdogLog "force_stop identity=$ExpectedIdentity owned_processes=$($owned.Count) active_work_wait=false"
        # Closing this runtime's Windows job handles also terminates its app-server
        # children. Pinned descendants cover older builds without that job setup.
        foreach ($item in $owned) {
            if (-not $item.HasExited) {
                try { $item.Kill() } catch { if (-not $item.HasExited) { throw } }
            }
        }
        foreach ($item in $owned) {
            if (-not $item.WaitForExit(5000)) { throw "Owned process did not exit: pid=$($item.Id)" }
        }
    } finally {
        foreach ($item in $owned) { $item.Dispose() }
    }
}

function Move-CdrForceArtifacts {
    param([string]$BackupRoot)
    $names = @('runtime.lock','heartbeat','drain.identity','drain.prepare','drain.ack',
        'restart','restart.launch','restart.completed','stop')
    $paths = @($names | ForEach-Object { Join-Path $RepoRoot ('.codex_discord_rust.' + $_) })
    $paths += @(Get-ChildItem -LiteralPath $RepoRoot -Filter '.codex_discord_rust.restart.claimed.*' -File |
        Where-Object { $_.Name -match '^\.codex_discord_rust\.restart\.claimed\.\d+\.\d+$' } |
        ForEach-Object { $_.FullName })
    foreach ($path in $paths) {
        if (-not [IO.File]::Exists($path)) { continue }
        if ([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($path)) -ine $RepoRoot) {
            throw 'Force restart artifact escaped the repository root.'
        }
        Move-Item -LiteralPath $path -Destination (Join-Path $BackupRoot ([IO.Path]::GetFileName($path)))
    }
}

function Stop-CdrRestartReadinessProbes {
    param([switch]$Preview)
    # Graceful preflight is a second cdr-runtime.exe with its own read-only
    # app-server. It must not be mistaken for a second bot or left waiting 900s.
    $others = @(Get-Process -Name 'cdr-runtime' -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -ieq $BinaryPath -and (Get-RustProcessIdentity $_) -cne (Get-VerifiedRuntimeIdentity) })
    foreach ($probe in $others) {
        try {
            [void]$probe.Handle
            $info = Get-CimInstance Win32_Process -Filter "ProcessId = $($probe.Id)" `
                -Property ProcessId,CreationDate,CommandLine
            if ($null -eq $info -or
                [math]::Abs(($probe.StartTime.ToUniversalTime() - $info.CreationDate.ToUniversalTime()).Ticks) -gt 10 -or
                $info.CommandLine -notmatch '(?:^|\s)--restart-readiness(?:\s|$)' -or
                $info.CommandLine -notmatch ('(?i)(?:^|\s)--env\s+"?' + [regex]::Escape($EnvPath) + '"?(?:\s|$)')) {
                throw 'Another runtime is not a bound restart-readiness probe; preserved.'
            }
            if ($Preview) { continue }
            Write-RustWatchdogLog "force_cancel_readiness_probe identity=$(Get-RustProcessIdentity $probe)"
            if (-not $probe.HasExited) {
                try { $probe.Kill() } catch { if (-not $probe.HasExited) { throw } }
            }
            if (-not $probe.WaitForExit(5000)) { throw 'Restart readiness probe did not exit after termination.' }
        } finally { $probe.Dispose() }
    }
}

function Invoke-CdrForceRestart {
    param([string]$ExpectedIdentity, [switch]$Preview)
    Assert-CdrNoMaintenanceV2 -Root $RepoRoot
    if ([IO.File]::Exists($DisablePath)) { throw 'Deployment/disabled marker is active; force restart did not alter it.' }
    if (-not [IO.File]::Exists($EnvPath)) { throw 'Runtime environment file is missing.' }
    $process = Get-VerifiedRuntimeProcess
    $identity = Get-RustProcessIdentity $process
    if ($ExpectedIdentity -and $identity -cne $ExpectedIdentity) { throw 'Force restart target identity changed.' }
    if (-not $Preview -and (Test-CdrCallerInsideRuntime $identity)) {
        throw 'Force worker must run outside the target runtime tree; use codex-discord-rust-restart.ps1 -Force.'
    }
    # Any extra same-binary process must be a verified read-only readiness probe.
    # Missing/replaced lock therefore cannot silently permit a second bot.
    $artifact=Get-CdrRuntimeArtifact $BinaryPath $ExpectedBinaryHash $ExpectedSourceCommit
    if ($Preview) {
        Stop-CdrRestartReadinessProbes -Preview
        Write-Output "would_force_restart identity=$identity wait_for_active_work=false"
        return
    }
    $journalPath = Join-Path $RepoRoot '.codex_discord_rust.force.launch'
    if ([IO.File]::Exists($journalPath)) {
        $record = [IO.File]::ReadAllText($journalPath) | ConvertFrom-Json
        if ($record.Version -ne 1 -or $record.BinaryPath -ine $BinaryPath -or
            $record.Operation -notmatch '^force:[a-f0-9]{32}$' -or
            $record.Phase -notin @('stopping','prepared','launching','child')) { throw 'Invalid force launch journal preserved.' }
        $backupParent = [IO.Path]::GetFullPath((Join-Path $RepoRoot 'maintenance_backups\force-restart'))
        if ([IO.Path]::GetDirectoryName([IO.Path]::GetFullPath($record.BackupRoot)) -ine $backupParent) {
            throw 'Force restart backup is outside its namespace.'
        }
    } else {
        $nonce = [guid]::NewGuid().ToString('N')
        $backup = Join-Path $RepoRoot ('maintenance_backups\force-restart\' + $nonce)
        [void][IO.Directory]::CreateDirectory($backup)
        $record = [pscustomobject]@{
            Version=1; BinaryPath=$BinaryPath; Operation="force:$nonce"; Fence=$null
            Phase='stopping'; ChildIdentity=''; ClaimPath=''; ArtifactHash=$artifact.ArtifactHash
            BuildInfo=$artifact.BuildInfo; RequireStartupRecord=$artifact.RequireStartupRecord
            TargetIdentity=$identity; BackupRoot=$backup
        }
        Save-CdrLaunchJournal $journalPath $record
    }
    if ($ExpectedBinaryHash -and $record.ArtifactHash -cne $ExpectedBinaryHash.ToUpperInvariant()) { throw 'Force restart artifact changed across handoff.' }
    if (-not $record.ArtifactHash) { throw 'Legacy force journal has no pinned artifact; explicit recovery required.' }
    $artifactPin=Get-CdrRuntimeArtifactPin $BinaryPath $record.ArtifactHash
    try {
    Stop-CdrRestartReadinessProbes
    if ($record.Phase -eq 'stopping') {
        if ($null -ne $process) {
            if ($identity -cne $record.TargetIdentity) { throw 'Another runtime replaced the force target; preserved.' }
            Stop-CdrOwnedTreeNow -Process $process -ExpectedIdentity $identity
        }
        if ($null -ne (Get-VerifiedRuntimeProcess)) { throw 'Force target remained alive.' }
        Move-CdrForceArtifacts -BackupRoot $record.BackupRoot
        $record.Phase = 'prepared'
        Save-CdrLaunchJournal $journalPath $record
    }
    # Same durable child tracking as normal restart: no duplicate launch after a
    # controller crash, and no app-server RPC/readiness request before termination.
    $record = Read-CdrLaunchJournal $journalPath
    $child = Get-CdrRecordedChild $record
    if ($null -eq $child) {
        if ($null -ne (Get-VerifiedRuntimeProcess)) { throw 'Unrecorded replacement preserved.' }
        Clear-DeadRuntimeArtifacts
        $record.Phase = 'prepared'; $record.ChildIdentity = ''
        Save-CdrLaunchJournal $journalPath $record
        $script:CdrLaunchJournalPath = $journalPath
        try { Start-RustRuntime -ResumeRemoteMcp }
        finally { $script:CdrLaunchJournalPath = $null }
        $record = Read-CdrLaunchJournal $journalPath
    }
    Wait-CdrReplacementReady -ExpectedIdentity $record.ChildIdentity -Artifact $record
    $receipt = [ordered]@{ Operation=$record.Operation; InterruptedIdentity=$record.TargetIdentity
        ReplacementIdentity=$record.ChildIdentity; CompletedAt=[DateTimeOffset]::UtcNow.ToString('o')
        BackupRoot=$record.BackupRoot; WaitedForActiveWork=$false
        ArtifactHash=$record.ArtifactHash; BuildInfo=$record.BuildInfo; RequireStartupRecord=$record.RequireStartupRecord }
    Write-AtomicRestartMarker -Path (Join-Path $RepoRoot '.codex_discord_rust.force.completed') `
        -Text ($receipt | ConvertTo-Json -Compress)
    [IO.File]::Delete($journalPath)
    Write-RustWatchdogLog "force_restart_completed old=$($record.TargetIdentity) replacement=$($record.ChildIdentity)"
    Write-Output "force_restart_completed identity=$($record.ChildIdentity) active_work_wait=false"
    } finally { $artifactPin.Dispose() }
}
