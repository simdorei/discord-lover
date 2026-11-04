# Invoked only by the watchdog's dedicated maintenance entry, under its control lock.
# Deliberately never calls generic health recovery or Stop-VerifiedRuntime.
function New-CdrDeploymentRecoveryReceipt {
    param($State,[string]$Hash,[string]$Identity)
    [ordered]@{Marker=$State.Marker;Hash=$Hash;ReplacementIdentity=$Identity;
        ArtifactRole=$(if($Hash -ceq $State.BaselineHash){'restored_baseline'}else{'candidate_recovery'});
        SourceState='unknown'}
}

function Invoke-CdrDeploymentRecovery {
    param([string]$StatePath)
    Assert-CdrNoPendingRestart -Root $RepoRoot
    $state = [IO.File]::ReadAllText($StatePath) | ConvertFrom-Json
    if ($null -ne $state.Version -and $state.Version -ne 1) {
        throw 'maintenance_v2_or_unknown: legacy recovery cannot adopt this state version'
    }
    if ([IO.Path]::GetFullPath($state.RepoRoot) -cne $RepoRoot -or
        [IO.Path]::GetFullPath($state.BinaryPath) -cne $BinaryPath -or
        [int]$state.RuntimePid -le 0 -or [long]$state.RuntimeTicks -le 0 -or
        [string]::IsNullOrWhiteSpace($state.Marker)) {
        throw 'Recovery state lacks an exact approved runtime identity'
    }
    foreach ($path in @($StopPath, $DisablePath)) {
        Assert-CdrMarkerOwner -Path $path -Text $state.Marker
    }
    foreach ($path in @($DrainPreparePath, $RestartPath, $DrainAckPath)) {
        if ([IO.File]::Exists($path)) { throw 'Another restart fence is present; recovery refused' }
    }
    $receiptPath = $StatePath + '.completed'
    $receipt = if ([IO.File]::Exists($receiptPath)) {
        [IO.File]::ReadAllText($receiptPath) | ConvertFrom-Json
    } else { $null }
    if ($null -ne $receipt -and $receipt.Marker -cne $state.Marker) {
        throw 'Foreign recovery completion receipt preserved'
    }
    $hash = Get-CdrArtifactHash -Path $BinaryPath
    if ($hash -notin @($state.BaselineHash, $state.CandidateHash)) {
        throw 'Installed executable is not an approved deployment artifact'
    }
    $artifactRole=if($hash -ceq $state.BaselineHash){'restored_baseline'}else{'candidate_recovery'}
    $journalPath = $StatePath + '.launch'
    $launch = Read-CdrLaunchJournal $journalPath
    # A terminal receipt is evidence only, never authority for another launch.
    if ($null -ne $receipt -and $null -eq $launch) {
        if ([IO.File]::Exists($StopPath) -or [IO.File]::Exists($DisablePath)) {
            throw 'Another maintenance intent prevents verification of completed deployment'
        }
        $completed = Get-VerifiedRuntimeProcess
        if ($null -eq $completed -or $receipt.Hash -cne $hash -or
            (Get-RustProcessIdentity $completed) -cne $receipt.ReplacementIdentity) {
            throw 'Previously completed deployment has no exact live child; a new operation is required'
        }
        Wait-CdrReplacementReady -ExpectedIdentity $receipt.ReplacementIdentity
        Write-Output "recovery_completed_verified artifact_role=$artifactRole source=unknown"
        return
    }
    if ($null -ne $launch) {
        if ($launch.Operation -cne ('deployment:' + $state.Marker) -or $launch.ArtifactHash -cne $hash) {
            throw 'Foreign deployment launch ownership preserved'
        }
        $recordedChild = Get-CdrRecordedChild $launch
    }
    $original = Get-Process -Id ([int]$state.RuntimePid) -ErrorAction SilentlyContinue
    if ($null -ne $original) {
        if ($original.Path -cne $BinaryPath -or
            $original.StartTime.ToUniversalTime().Ticks.ToString() -cne [string]$state.RuntimeTicks) {
            throw 'Original PID now belongs to another instance; recovery refused'
        }
        if (-not [IO.File]::Exists($StopPath) -and -not [IO.File]::Exists($DisablePath)) {
            throw 'No owned stop intent; original process left running'
        }
        if (-not [IO.File]::Exists($StopPath)) {
            # The worker may have died between disabled and stop publication.
            # The approved state and original identity have both been verified.
            Write-NewCdrMarker -Path $StopPath -Text $state.Marker
        }
        # Observing a stop is irreversible. Never accept this process as recovered.
        Wait-RustRuntimeExit -Process $original `
            -ExpectedIdentity (Get-RustProcessIdentity $original) `
            -Reason 'deployment_recovery' -TimeoutSeconds 30
        if ($null -ne (Get-Process -Id ([int]$state.RuntimePid) -ErrorAction SilentlyContinue)) {
            throw 'Original instance has not definitively exited; recovery pending, no force kill'
        }
    }
    $current = Get-VerifiedRuntimeProcess
    if ($null -ne $current) {
        if ($null -ne $launch -and $null -ne $recordedChild -and
            (Get-RustProcessIdentity $current) -ceq $launch.ChildIdentity) {
            if ([IO.File]::Exists($StopPath)) {
                throw 'Another maintenance intent is active; recorded child will not be certified'
            }
            Wait-CdrReplacementReady -ExpectedIdentity $launch.ChildIdentity
            Assert-CdrMarkerOwner -Path $DisablePath -Text $state.Marker
            # Only a pending launch journal authorizes consuming the recovery seal.
            Write-AtomicRestartMarker -Path $receiptPath -Text (
                (New-CdrDeploymentRecoveryReceipt $state $hash $launch.ChildIdentity) |
                    ConvertTo-Json -Compress
            )
            if ([IO.File]::Exists($DisablePath)) { [IO.File]::Delete($DisablePath) }
            [IO.File]::Delete($journalPath)
            Write-Output "recovery_completed_verified artifact_role=$artifactRole source=unknown"
            return
        }
        if ($null -eq $receipt -or $receipt.Hash -cne $hash -or
            (Get-RustProcessIdentity $current) -cne $receipt.ReplacementIdentity) {
            throw 'Unrelated or unrecorded live instance preserved; recovery pending'
        }
        if ([IO.File]::Exists($StopPath) -or [IO.File]::Exists($DisablePath)) {
            throw 'Another maintenance intent is active; deployment completion refused'
        }
        Wait-CdrReplacementReady -ExpectedIdentity $receipt.ReplacementIdentity
        Write-Output "recovery_completed_verified artifact_role=$artifactRole source=unknown"
        return
    }
    if (-not [IO.File]::Exists($StopPath) -and -not [IO.File]::Exists($DisablePath) -and
        $null -eq $receipt -and $null -eq $launch) {
        throw 'No owned maintenance intent or completion receipt; recovery refused'
    }
    foreach ($path in @($DrainPreparePath, $RestartPath, $DrainAckPath)) {
        if ([IO.File]::Exists($path)) { throw 'Another restart fence is present; recovery refused' }
    }
    Clear-DeadRuntimeArtifacts
    foreach ($path in @($StopPath, $DisablePath)) {
        Assert-CdrMarkerOwner -Path $path -Text $state.Marker
    }
    try {
        $launch = New-CdrLaunchJournal $journalPath ('deployment:' + $state.Marker) -ArtifactHash $hash
        if ($null -ne (Get-CdrRecordedChild $launch)) {
            throw 'Recorded child is still starting; no duplicate launch'
        }
        $launch.Phase = 'prepared'; $launch.ChildIdentity = ''
        $launch | Add-Member -NotePropertyName ArtifactHash -NotePropertyValue $hash -Force
        Save-CdrLaunchJournal $journalPath $launch
        # disabled prevents a scheduled generic watchdog starting/killing a bot
        # if this worker dies. Rust itself only treats stop as a shutdown request.
        if (-not [IO.File]::Exists($DisablePath)) {
            Write-NewCdrMarker -Path $DisablePath -Text $state.Marker
        }
        if ([IO.File]::Exists($StopPath)) { [IO.File]::Delete($StopPath) }
        $script:CdrLaunchJournalPath = $journalPath
        try { Start-RustRuntime }
        finally { $script:CdrLaunchJournalPath = $null }
        $launch = Read-CdrLaunchJournal $journalPath
        $identity = Get-VerifiedRuntimeIdentity
        if ($launch.Phase -ne 'child' -or $launch.ChildIdentity -cne $identity) {
            throw 'Replacement is not the durably recorded launch child'
        }
        Wait-CdrReplacementReady -ExpectedIdentity $identity
        Write-AtomicRestartMarker -Path $receiptPath -Text (
            (New-CdrDeploymentRecoveryReceipt $state $hash $identity) |
                ConvertTo-Json -Compress
        )
        Assert-CdrMarkerOwner -Path $DisablePath -Text $state.Marker
        if ([IO.File]::Exists($DisablePath)) { [IO.File]::Delete($DisablePath) }
        [IO.File]::Delete($journalPath)
    } catch {
        $primary = $_
        try {
            if (-not [IO.File]::Exists($DisablePath)) {
                Write-NewCdrMarker -Path $DisablePath -Text $state.Marker
            }
        } catch {
            throw "Recovery failed: $($primary.Exception.Message); maintenance preservation failed: $($_.Exception.Message)"
        }
        throw $primary
    }
    Add-Content -LiteralPath $state.LogPath -Encoding UTF8 -Value (
        "$(Get-Date -Format o) recovery_completed_verified identity=$identity sha256=$hash"
    )
    Write-Output "recovery_completed_verified artifact_role=$artifactRole source=unknown"
}
