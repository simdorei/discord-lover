function Get-CdrUpgradeTargetPaths {
    @('codex-discord-rust-watchdog.ps1','codex-discord-rust-control.ps1','codex-discord-rust-drain.ps1',
        'scripts/CdrDeploymentRecovery.ps1','scripts/CdrLaunchJournal.ps1','scripts/CdrRestartTransaction.ps1',
        'scripts/CdrForceRestart.ps1','scripts/CdrMaintenanceCompatibility.ps1',
        'scripts/CdrAsyncRecoveryCompatibility.ps1','scripts/CdrRuntimeLaunchCompatibility.ps1',
        'codex-discord-tray-runtime.ps1','codex-discord-tray-restart-runtime.ps1')
}
function Read-CdrUpgradeCompatibility([string]$BaselineHash,[string]$EnvironmentHash,$Binding) {
    $required=Join-Path $RepoRoot '.codex_discord_rust.compatibility.required'
    $contract=Join-Path $RepoRoot '.codex_discord_rust.compatibility.json'
    $requiredExists=Test-Path -LiteralPath $required; $contractExists=Test-Path -LiteralPath $contract
    if (-not $requiredExists -and -not $contractExists) { return }
    if (-not $requiredExists -or -not $contractExists) { throw 'upgrade_incomplete_compatibility_guard' }
    Assert-CdrUpgradePath $required; Assert-CdrUpgradePath $contract
    if ((Get-Item -LiteralPath $contract).Length -gt 16384 -or
        [IO.File]::ReadAllText($required).Trim() -cne (Get-CdrArtifactHash $contract).ToLowerInvariant()) {
        throw 'upgrade_compatibility_guard_invalid'
    }
    $decl=[IO.File]::ReadAllText($contract)|ConvertFrom-Json
    if ($decl.protocol -cne 'cdr-runtime-launch-v1' -or
        $decl.candidate_sha256 -cne $BaselineHash.ToLowerInvariant() -or
        $decl.environment_sha256 -cne $EnvironmentHash.ToLowerInvariant()) { throw 'upgrade_guard_binding_mismatch' }
    foreach ($pair in @(@($decl.root,$RepoRoot),@($decl.candidate_path,$BinaryPath),
        @($decl.environment_path,$EnvPath),@($decl.database_path,$Binding.database))) {
        if (-not [StringComparer]::OrdinalIgnoreCase.Equals(
            [IO.Path]::GetFullPath(($pair[0] -replace '^\\\\\?\\','')),
            [IO.Path]::GetFullPath(($pair[1] -replace '^\\\\\?\\','')))) {
            throw 'upgrade_guard_path_mismatch'
        }
    }
    Assert-CdrUpgradePath $decl.capability_path
    if ((Get-CdrArtifactHash $decl.capability_path).ToLowerInvariant() -cne $decl.capability_sha256) { throw 'upgrade_capability_changed' }
    $cap=[IO.File]::ReadAllText($decl.capability_path)|ConvertFrom-Json
    if ($cap.protocol -cne 'cdr-artifact-capabilities-v1' -or $cap.artifact_sha256 -cne $decl.candidate_sha256 -or
        $cap.async_resolution_max_format -ne 1 -or $cap.async_recovery_policy_max_format -ne 1) { throw 'upgrade_capability_unsupported' }
    return [pscustomobject]@{Declaration=$decl;Capability=$cap}
}
function Add-CdrUpgradeCompatibility($State) {
    $runtime=@($State.Files|Where-Object {$_.Path -eq 'target/release/cdr-runtime.exe'})[0]
    $guard=Read-CdrUpgradeCompatibility $runtime.OldHash $State.EnvironmentHash $State.Binding
    if (-not $guard) { return }
    $decl=$guard.Declaration; $cap=$guard.Capability
    # Rebind only the artifact identity, retaining every existing capability and
    # recovery policy. The candidate must prove these after the old writer exits.
    $cap.artifact_sha256=$runtime.NewHash.ToLowerInvariant()
    $capPath=Join-Path (Get-CdrUpgradeBundle $State) 'capabilities.json'
    Write-NewCdrMarker $capPath ($cap|ConvertTo-Json -Depth 8 -Compress)
    $decl.candidate_sha256=$cap.artifact_sha256; $decl.capability_path=$capPath
    $decl.capability_sha256=(Get-CdrArtifactHash $capPath).ToLowerInvariant()
    $newContract=Join-Path (Get-CdrUpgradeBundle $State) 'launch-contract.json'
    Write-NewCdrMarker $newContract ($decl|ConvertTo-Json -Depth 8 -Compress)
    $newRequired=Join-Path (Get-CdrUpgradeBundle $State) 'launch-required.txt'
    Write-NewCdrMarker $newRequired ((Get-CdrArtifactHash $newContract).ToLowerInvariant()+"`n")
    Add-CdrUpgradePayload $State '.codex_discord_rust.compatibility.json' $newContract
    Add-CdrUpgradePayload $State '.codex_discord_rust.compatibility.required' $newRequired
    $State.Guarded=$true
}
function Test-CdrUpgradeCompatibility($State) {
    if (-not $State.Guarded) { return }
    & {
        . (Join-Path $RepoRoot 'scripts/CdrAsyncRecoveryCompatibility.ps1')
        $entry=@($State.Files|Where-Object {$_.Path -eq 'target/release/cdr-runtime.exe'})[0]
        $contractEntry=@($State.Files|Where-Object {$_.Path -eq '.codex_discord_rust.compatibility.json'})[0]
        $decl=[IO.File]::ReadAllText((Get-CdrUpgradeFile $State $contractEntry 'candidate'))|ConvertFrom-Json
        $null=Assert-CdrAsyncRecoveryCompatibility -CandidatePath (Get-CdrUpgradeFile $State $entry 'candidate') `
            -DatabasePath $decl.database_path -CapabilityManifestPath $decl.capability_path `
            -ExpectedCandidateSha256 $decl.candidate_sha256 -ExpectedCapabilitySha256 $decl.capability_sha256 `
            -EnvironmentPath $EnvPath -ExpectedEnvironmentSha256 $decl.environment_sha256 `
            -WorkingDirectory $RepoRoot -RequireRecoveryPolicy
    }
}
