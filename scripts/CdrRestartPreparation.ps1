# Called with the shared control lease. Publish artifact + fence before sealing intake.
function New-CdrRestartPreparation {
    param($Process, [string]$Identity)
    $path=Join-Path $RepoRoot '.codex_discord_rust.restart.launch'
    $existing=Read-CdrLaunchJournal $path
    if ($existing) {
        if ($existing.Phase -cne 'preparing' -or $existing.Fence.ProcessIdentity -cne $Identity) { throw 'Foreign restart preparation preserved.' }
        $null=Get-CdrRuntimeArtifact $BinaryPath $existing.ArtifactHash $ExpectedSourceCommit
        return $existing
    }
    $artifact=Get-CdrRuntimeArtifact $BinaryPath $ExpectedBinaryHash $ExpectedSourceCommit
    $identityRecord=Get-RuntimeDrainIdentity $Process $Identity
    $fence=Get-RestartDrainFence $DrainPreparePath
    if ($null -eq $fence) {
        $fence=[pscustomobject]@{RuntimeId=$identityRecord.RuntimeId;ProcessIdentity=$Identity;Nonce=[guid]::NewGuid().ToString('N')}
    } elseif ($fence.RuntimeId -cne $identityRecord.RuntimeId -or $fence.ProcessIdentity -cne $Identity) { throw 'Foreign drain preparation preserved.' }
    return New-CdrLaunchJournal $path ('restart:'+$fence.Nonce) $fence $artifact.ArtifactHash -Artifact $artifact -Phase preparing
}

function Complete-CdrRestartPreparation {
    param($Record)
    Assert-CdrRestartTransactionMarkers $Record
    $ack=Get-RestartDrainFence $DrainAckPath -RequireSealed
    $marker=Get-RestartDrainFence $RestartPath
    if (-not (Test-RestartDrainFenceMatch $ack $Record.Fence) -or -not (Test-RestartDrainFenceMatch $marker $Record.Fence)) {
        throw 'Restart is not authorized by an exact sealed acknowledgement and restart marker.'
    }
    $null=Get-CdrRuntimeArtifact $BinaryPath $Record.ArtifactHash $ExpectedSourceCommit
    $Record.Phase='prepared'
    Save-CdrLaunchJournal (Join-Path $RepoRoot '.codex_discord_rust.restart.launch') $Record
}
