# Called under the common control guard, including by a later scheduled watchdog.
function Assert-CdrRestartTransactionMarkers {
    param($Record)
    foreach ($path in @($StopPath, $DisablePath)) {
        if (Test-Path -LiteralPath $path) { throw 'Another maintenance intent is active; restart transaction paused' }
    }
    foreach ($path in @($RestartPath, $DrainPreparePath, $DrainAckPath, $Record.ClaimPath)) {
        if (-not $path -or -not [IO.File]::Exists($path)) { continue }
        if (-not (Test-RestartDrainFenceMatch (Get-RestartDrainFence $path) $Record.Fence)) {
            throw 'Foreign restart fence preserved during transaction recovery'
        }
    }
}

function Invoke-CdrRestartTransaction {
    param($Fence)
    $journalPath = Join-Path $RepoRoot '.codex_discord_rust.restart.launch'
    $record = Read-CdrLaunchJournal $journalPath
    if ($null -eq $record) {
        $artifact=Get-CdrRuntimeArtifact $BinaryPath $ExpectedBinaryHash $ExpectedSourceCommit
        $record=New-CdrLaunchJournal $journalPath ('restart:' + $Fence.Nonce) $Fence $artifact.ArtifactHash -Artifact $artifact
    } elseif ($record.Operation -cne ('restart:'+$Fence.Nonce) -or -not (Test-RestartDrainFenceMatch $record.Fence $Fence)) { throw 'Foreign restart journal preserved.' }
    if ($record.Phase -eq 'preparing') { throw 'Restart preparation is incomplete; no launch authorized.' }
    if ($ExpectedBinaryHash -and $record.ArtifactHash -cne $ExpectedBinaryHash.ToUpperInvariant()) { throw 'Restart artifact changed across handoff.' }
    Assert-CdrRestartTransactionMarkers $record
    $child = Get-CdrRecordedChild $record
    if ($null -ne $child) {
        Wait-CdrReplacementReady -ExpectedIdentity $record.ChildIdentity -Artifact $record
    } else {
        if ($null -ne (Get-VerifiedRuntimeProcess)) {
            throw 'Unrecorded live runtime preserved; restart transaction paused'
        }
        # A recorded child is definitely dead, or no launch has been attempted.
        # Partial claim/drain cleanup can be resumed without recreating old ACKs.
        if (-not $record.ClaimPath) {
            $record.ClaimPath = $RestartClaimPath
            Save-CdrLaunchJournal $journalPath $record
        }
        if ([IO.File]::Exists($RestartPath)) {
            if ([IO.File]::Exists($record.ClaimPath)) { throw 'Both restart and claim markers exist; preserved' }
            [IO.File]::Move($RestartPath, $record.ClaimPath)
        }
        Assert-CdrRestartTransactionMarkers $record
        foreach ($path in @($DrainPreparePath, $DrainAckPath)) {
            if ([IO.File]::Exists($path)) { [IO.File]::Delete($path) }
        }
        Clear-DeadRuntimeArtifacts
        $record.Phase = 'prepared'; $record.ChildIdentity = ''
        Save-CdrLaunchJournal $journalPath $record
        $script:CdrLaunchJournalPath = $journalPath
        try { Start-RustRuntime -ResumeRemoteMcp }
        finally { $script:CdrLaunchJournalPath = $null }
        $record = Read-CdrLaunchJournal $journalPath
        $child = Get-CdrRecordedChild $record
        if ($null -eq $child) { throw 'Recorded replacement did not survive startup' }
        Wait-CdrReplacementReady -ExpectedIdentity $record.ChildIdentity -Artifact $record
    }
    Assert-CdrRestartTransactionMarkers $record
    Write-CdrRestartCompletion -Fence $Fence -ExpectedChildIdentity $record.ChildIdentity -Artifact $record
    foreach ($path in @($RestartPath, $DrainPreparePath, $DrainAckPath, $record.ClaimPath)) {
        if ($path -and [IO.File]::Exists($path)) { [IO.File]::Delete($path) }
    }
    [IO.File]::Delete($journalPath)
    Write-Output 'restart_completed_verified'
}
