# Upgrade journals use the existing root control lease. No task/Discord mutations.
function Get-CdrUpgradeStatePath { Join-Path $RepoRoot '.codex_discord_rust.upgrade.json' }
function Get-CdrUpgradeBundle($State) { Join-Path $RepoRoot ('.codex-discord-backups/upgrade-'+$State.Operation) }
function Get-CdrUpgradeFile($State, $Entry, [string]$Area) {
    Join-Path (Join-Path (Get-CdrUpgradeBundle $State) $Area) $Entry.Path
}
function Save-CdrUpgradeState($State) {
    $path=Get-CdrUpgradeStatePath
    if ([IO.File]::Exists($path)) {
        $previous=[IO.File]::ReadAllText($path)|ConvertFrom-Json
        if ($previous.Operation -cne $State.Operation) { throw 'upgrade_owner_changed' }
    }
    Write-AtomicRestartMarker $path ($State|ConvertTo-Json -Depth 12 -Compress)
}
function Set-CdrUpgradePhase($State, [string]$Phase) {
    $State.Phase=$Phase; Save-CdrUpgradeState $State
    Write-Host ('upgrade_phase='+$Phase)
}
function Read-CdrUpgradeState {
    $path=Get-CdrUpgradeStatePath
    Assert-CdrUpgradePath $path
    if ((Get-Item -LiteralPath $path).Length -gt 65536) { throw 'upgrade_state_too_large' }
    $s=[IO.File]::ReadAllText($path)|ConvertFrom-Json
    if ($s.Version -ne 1 -or $s.Operation -cnotmatch '^[a-f0-9]{32}$' -or
        $s.Root -cne $RepoRoot -or $s.Source -cne $UpgradeSourceRoot -or
        $s.Phase -notin @('prepared','draining','stopping','stopped','installing','installed',
            'rolling_back','launch_ready','launching','healthy','completed') -or
        $s.LaunchKind -notin @('candidate','previous')) { throw 'upgrade_state_invalid' }
    $expected=@('target/release/cdr-runtime.exe','codex-discord-tray.ps1','codex-discord-tray-agy.ps1')
    if ($s.Guarded) { $expected+=@('.codex_discord_rust.compatibility.json','.codex_discord_rust.compatibility.required') }
    if (@($s.Files).Count -ne $expected.Count) { throw 'upgrade_manifest_invalid' }
    foreach ($name in $expected) {
        $entry=@($s.Files|Where-Object {$_.Path -ceq $name})
        if ($entry.Count -ne 1 -or $entry[0].NewHash -cnotmatch '^[A-F0-9]{64}$' -or
            ($entry[0].OldHash -and $entry[0].OldHash -cnotmatch '^[A-F0-9]{64}$')) { throw 'upgrade_manifest_invalid' }
    }
    if ($s.Fence.Nonce -cne $s.Operation -or $s.Fence.ProcessIdentity -cnotmatch '^\d+\|\d+$' -or
        $s.Fence.RuntimeId -cnotmatch '^[A-Za-z0-9_-]{1,128}$') { throw 'upgrade_fence_invalid' }
    Assert-CdrUpgradeCode $s
    return $s
}
function Get-CdrUpgradeCodePaths {
    @('update.ps1','codex-discord-rust-watchdog.ps1','codex-discord-rust-control.ps1','codex-discord-rust-drain.ps1',
        'scripts/CdrDeploymentRecovery.ps1','scripts/CdrLaunchJournal.ps1','scripts/CdrRestartTransaction.ps1',
        'scripts/CdrForceRestart.ps1','scripts/CdrMaintenanceCompatibility.ps1','scripts/CdrMaintenanceDiagnostics.ps1',
        'scripts/CdrUpgradeState.ps1','scripts/CdrUpgradeFiles.ps1','scripts/CdrUpgradeCommand.ps1',
        'scripts/CdrUpgradeCompatibility.ps1','scripts/CdrUpgradeEngine.ps1')
}
function Assert-CdrUpgradeCode($State) {
    $expected=@(Get-CdrUpgradeCodePaths)
    if (@($State.CodePins).Count -ne $expected.Count) { throw 'upgrade_code_pins_invalid' }
    foreach ($name in $expected) {
        $pin=@($State.CodePins|Where-Object {$_.Path -ceq $name})
        $path=Join-Path $UpgradeSourceRoot $name
        Assert-CdrUpgradePath $path
        if ($pin.Count -ne 1 -or (Get-CdrArtifactHash $path) -cne $pin[0].Hash) { throw 'upgrade_tool_changed; use the original updater to resume' }
    }
}
function Assert-CdrUpgradeMarkers($State) {
    Assert-CdrNoMaintenanceV2 $RepoRoot
    Assert-CdrNoOrphanRestartClaim $RepoRoot
    foreach ($name in @('.codex_discord_rust.restart','.codex_discord_rust.restart.launch','.codex_discord_rust.force.launch')) {
        if (Test-Path -LiteralPath (Join-Path $RepoRoot $name)) { throw 'upgrade_foreign_operation_preserved' }
    }
    foreach ($path in @($DisablePath,$StopPath)) { Assert-CdrMarkerOwner $path $State.Operation }
    if ($State.Phase -notin @('prepared','draining','completed') -and -not [IO.File]::Exists($DisablePath)) {
        throw 'upgrade_disable_seal_missing'
    }
    Assert-CdrUpgradeFence $State -AllowAbsent
}
function Assert-CdrUpgradeStable($State) {
    Assert-CdrUpgradeCode $State
    if ((Get-CdrArtifactHash $EnvPath) -cne $State.EnvironmentHash) { throw 'upgrade_environment_changed' }
    $settingsPath=([string]$State.Binding.agy_settings -replace '^\\\\\?\\','')
    Assert-CdrUpgradePath $settingsPath
    $settingsHash=if([IO.File]::Exists($settingsPath)){(Get-CdrArtifactHash $settingsPath).ToLowerInvariant()}else{$null}
    if ($settingsHash -cne $State.Binding.agy_settings_sha256) { throw 'upgrade_agy_settings_changed' }
    $mode=Join-Path $RepoRoot '.codex_discord_runtime'
    $modeHash=if ([IO.File]::Exists($mode)) {Get-CdrArtifactHash $mode}else{''}
    if ($modeHash -cne $State.ModeHash) { throw 'upgrade_runtime_selection_changed' }
    foreach ($pin in $State.TargetPins) {
        $path=Join-Path $RepoRoot $pin.Path
        Assert-CdrUpgradePath $path
        if ((Get-CdrArtifactHash $path) -cne $pin.Hash) { throw 'upgrade_launcher_changed' }
    }
    foreach ($entry in $State.Files) {
        foreach ($area in @('candidate','previous')) {
            $hash=if($area -eq 'candidate'){$entry.NewHash}else{$entry.OldHash}
            if ($hash) {
                $path=Get-CdrUpgradeFile $State $entry $area; Assert-CdrUpgradePath $path
                if ((Get-CdrArtifactHash $path) -cne $hash) { throw 'upgrade_backup_or_candidate_changed' }
            }
        }
    }
    $envBackup=Join-Path (Get-CdrUpgradeBundle $State) 'previous/.env'
    if ((Get-CdrArtifactHash $envBackup) -cne $State.EnvironmentHash) { throw 'upgrade_environment_backup_changed' }
    foreach ($name in @('BeforeStopBackup','AfterStopBackup')) {
        if ($State.$name) {
            Assert-CdrUpgradePath $State.$name.Path
            if ((Get-CdrArtifactHash $State.$name.Path) -cne $State.$name.Hash) { throw 'upgrade_database_backup_changed' }
        }
    }
}
