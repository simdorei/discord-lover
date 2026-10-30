function Read-CdrUpgradeHeartbeat($Process) {
    $stream=$null; $reader=$null
    try {
        # The runtime atomically renames a still-open temporary file. Share write
        # and delete so a healthy heartbeat cannot fail just at that boundary.
        $stream=[IO.File]::Open($HeartbeatPath,'Open','Read',([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
        if ($stream.Length -gt 4096) { return $null }
        $reader=[IO.StreamReader]::new($stream,[Text.Encoding]::UTF8)
        $text=$reader.ReadToEnd()
        if ($text -notmatch '(?m)^pid=(\d+)\r?$' -or [int]$Matches[1] -ne $Process.Id) { return $null }
        if ($text -notmatch '(?m)^updated_at=(\d+)\r?$') { return $null }
        $stamp=[long]$Matches[1]; $updated=[DateTimeOffset]::FromUnixTimeSeconds($stamp)
        $age=([DateTimeOffset]::UtcNow-$updated).TotalSeconds
        if ($age -lt 0 -or $age -gt $HealthHeartbeatMaxAgeSeconds -or
            $stamp -lt ([DateTimeOffset]$Process.StartTime.ToUniversalTime()).ToUnixTimeSeconds()) { return $null }
        return $stamp
    } catch [IO.IOException] { return $null }
    finally { if($reader){$reader.Dispose()}elseif($stream){$stream.Dispose()} }
}
function Assert-CdrUpgradeFence($State,[switch]$AllowAbsent) {
    foreach ($path in @($DrainPreparePath,$DrainAckPath)) {
        if (-not [IO.File]::Exists($path)) { if($AllowAbsent){continue}else{throw 'upgrade_drain_fence_missing'} }
        if (-not (Test-RestartDrainFenceMatch (Get-RestartDrainFence $path) $State.Fence)) { throw 'upgrade_foreign_drain_fence' }
    }
}
function Wait-CdrUpgradeHealth($State) {
    $journal=Read-CdrLaunchJournal (Join-Path (Get-CdrUpgradeBundle $State) 'launch.json')
    if (-not $journal -or $journal.Operation -cne ('upgrade:'+$State.Operation+':'+$State.LaunchKind)) { throw 'upgrade_launch_journal_mismatch' }
    $last=$null; $deadline=[DateTimeOffset]::UtcNow.AddSeconds($StartupTimeoutSeconds)
    do {
        $child=Get-CdrRecordedChild $journal
        if (-not $child) { throw 'upgrade_recorded_child_exited; no automatic relaunch or database rollback' }
        if ((Get-VerifiedRuntimeIdentity) -cne $journal.ChildIdentity) { throw 'upgrade_child_lock_mismatch' }
        Assert-CdrUpgradeInstalled $State
        $stamp=Read-CdrUpgradeHeartbeat $child
        if ($null -ne $stamp) {
                if ($null -ne $last -and $stamp -gt $last) {
                    $State.Heartbeats=@($last,$stamp); $State.ChildIdentity=$journal.ChildIdentity
                    Save-CdrUpgradeState $State; return
                }; $last=$stamp
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    throw 'upgrade_startup_not_verified; state retained, no second launch'
}
function Clear-CdrUpgradeDrain($State) {
    Assert-CdrUpgradeNoRuntime
    Assert-CdrUpgradeFence $State -AllowAbsent
    foreach ($path in @($DrainPreparePath,$DrainAckPath)) { if([IO.File]::Exists($path)){[IO.File]::Delete($path)} }
    if ([IO.File]::Exists($DrainIdentityPath)) {
        $identity=Read-RestartDrainFields $DrainIdentityPath @('version','runtime_id','pid','state')
        if ($identity.runtime_id -cne $State.Fence.RuntimeId) { throw 'upgrade_foreign_drain_identity' }
        [IO.File]::Delete($DrainIdentityPath)
    }
    Assert-CdrMarkerOwner $StopPath $State.Operation
    if ([IO.File]::Exists($StopPath)) { [IO.File]::Delete($StopPath) }
    Clear-DeadRuntimeArtifacts -PreserveRestartDrain
}
function Complete-CdrUpgrade($State) {
    Assert-CdrUpgradeStable $State; Assert-CdrUpgradeInstalled $State
    if ((Get-VerifiedRuntimeIdentity) -cne $State.ChildIdentity) { throw 'upgrade_completion_process_changed' }
    $heartbeat=Read-CdrUpgradeHeartbeat (Get-VerifiedRuntimeProcess)
    if ($null -eq $heartbeat -or @($State.Heartbeats).Count -ne 2 -or
        $State.Heartbeats[1] -le $State.Heartbeats[0]) { throw 'upgrade_completion_health_missing' }
    Assert-CdrUpgradeMarkers $State
    Remove-CdrUpgradeStaging $State
    Set-CdrUpgradePhase $State 'completed'
    $receipt=Join-Path (Get-CdrUpgradeBundle $State) 'completed.json'
    Write-AtomicRestartMarker $receipt ($State|ConvertTo-Json -Depth 12 -Compress)
    Assert-CdrMarkerOwner $DisablePath $State.Operation
    if ([IO.File]::Exists($DisablePath)) { [IO.File]::Delete($DisablePath) }
    [IO.File]::Delete((Get-CdrUpgradeStatePath))
    Write-Output ('upgrade_'+$(if($State.LaunchKind -eq 'candidate'){'verified'}else{'rolled_back'})+' receipt='+$receipt)
}
function Invoke-CdrUpgradeEngine($State) {
    try {
        Assert-CdrUpgradeStable $State
        Assert-CdrUpgradeMarkers $State
        if ($null -ne $State.Command) { throw 'upgrade_helper_outcome_unknown; no helper replay' }
        while ($true) {
            Assert-CdrUpgradeStable $State; Assert-CdrUpgradeMarkers $State
            switch ($State.Phase) {
                'prepared' {
                    if ((Get-VerifiedRuntimeIdentity) -cne $State.Fence.ProcessIdentity) { throw 'upgrade_original_process_changed' }
                    New-CdrUpgradeSnapshot $State 'BeforeStopBackup'
                    Set-CdrUpgradePhase $State 'draining'
                }
                'draining' {
                    if (-not [IO.File]::Exists($DisablePath)) { Write-NewCdrMarker $DisablePath $State.Operation }
                    if ((Get-VerifiedRuntimeIdentity) -cne $State.Fence.ProcessIdentity) { throw 'upgrade_original_process_changed' }
                    if (-not [IO.File]::Exists($DrainPreparePath)) {
                        Write-NewCdrMarker $DrainPreparePath ("version=1`nruntime_id=$($State.Fence.RuntimeId)`nprocess_identity=$($State.Fence.ProcessIdentity)`nnonce=$($State.Operation)`n")
                    }
                    $fence=Enter-RestartDrain (Get-VerifiedRuntimeProcess) $State.Fence.ProcessIdentity $WaitTimeoutSeconds
                    if (-not (Test-RestartDrainFenceMatch $fence $State.Fence)) { throw 'upgrade_foreign_drain_fence' }
                    Assert-RestartDrainBound $State.Fence $State.Fence.ProcessIdentity
                    Set-CdrUpgradePhase $State 'stopping'
                }
                'stopping' {
                    $original=Get-VerifiedRuntimeProcess
                    if ($original) {
                        Assert-RestartDrainBound $State.Fence $State.Fence.ProcessIdentity
                        if (-not [IO.File]::Exists($StopPath)) { Write-NewCdrMarker $StopPath $State.Operation }
                        Wait-RustRuntimeExit $original $State.Fence.ProcessIdentity 'discord_lover_upgrade' 45
                    }
                    Assert-CdrUpgradeNoRuntime
                    Set-CdrUpgradePhase $State 'stopped'
                }
                'stopped' {
                    $exclusive=Enter-CdrUpgradeExclusive
                    try {
                        New-CdrUpgradeSnapshot $State 'AfterStopBackup'
                        Test-CdrUpgradeCompatibility $State
                        Set-CdrUpgradePhase $State 'installing'
                    } finally { $exclusive.Dispose() }
                }
                'installing' {
                    $exclusive=Enter-CdrUpgradeExclusive
                    try { Install-CdrUpgradeFiles $State; Set-CdrUpgradePhase $State 'installed' }
                    finally { $exclusive.Dispose() }
                }
                'installed' {
                    $null=Read-CdrUpgradeBinding $State $BinaryPath
                    $null=Invoke-CdrUpgradeCommand $State $BinaryPath @('--restart-readiness','--restart-quiet-seconds','0',
                        '--restart-wait-timeout-seconds','30','--env',$EnvPath) 100
                    Set-CdrUpgradePhase $State 'launch_ready'
                }
                'rolling_back' {
                    $exclusive=Enter-CdrUpgradeExclusive
                    try { Install-CdrUpgradeFiles $State -Previous; $State.LaunchKind='previous'; Set-CdrUpgradePhase $State 'launch_ready' }
                    finally { $exclusive.Dispose() }
                }
                'launch_ready' {
                    Assert-CdrUpgradeInstalled $State
                    $exclusive=Enter-CdrUpgradeExclusive
                    try { Clear-CdrUpgradeDrain $State }
                    finally { $exclusive.Dispose() }
                    $hash=Get-CdrArtifactHash $BinaryPath
                    $journalPath=Join-Path (Get-CdrUpgradeBundle $State) 'launch.json'
                    $null=New-CdrLaunchJournal $journalPath ('upgrade:'+$State.Operation+':'+$State.LaunchKind) $State.Fence $hash
                    # Durable BEFORE invoking the launcher; recovery adopts only its recorded child.
                    Set-CdrUpgradePhase $State 'launching'
                }
                'launching' {
                    $journalPath=Join-Path (Get-CdrUpgradeBundle $State) 'launch.json'
                    $journal=Read-CdrLaunchJournal $journalPath
                    if ($journal -and $journal.Phase -eq 'prepared') {
                        Assert-CdrUpgradeNoRuntime; Assert-CdrUpgradeInstalled $State
                        $script:CdrLaunchJournalPath=$journalPath
                        try { Start-RustRuntime }
                        finally { $script:CdrLaunchJournalPath=$null }
                    }
                    Wait-CdrUpgradeHealth $State; Set-CdrUpgradePhase $State 'healthy'
                }
                'healthy' { Complete-CdrUpgrade $State; return }
                'completed' { Complete-CdrUpgrade $State; return }
                default { throw 'upgrade_phase_invalid' }
            }
        }
    } catch {
        $State.LastError=Get-CdrMaintenanceDiagnostic $_.Exception.Message $EnvPath
        Save-CdrUpgradeState $State
        # Only before ANY launch attempt; never restore a DB or replay a helper.
        if ($State.Phase -in @('stopped','installing','installed') -and $null -eq $State.Command) {
            try {
                Assert-CdrUpgradeNoRuntime
                Set-CdrUpgradePhase $State 'rolling_back'
                Invoke-CdrUpgradeEngine $State
            } catch { throw ('upgrade_failed: '+$State.LastError+'; recovery_held: '+(Get-CdrMaintenanceDiagnostic $_.Exception.Message $EnvPath)) }
        }
        throw ('upgrade_failed: '+$State.LastError)
    }
}
