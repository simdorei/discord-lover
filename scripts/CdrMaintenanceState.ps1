# Version 2 state and its global ownership marker are the SAME atomic file.
function Get-CdrMaintenancePath([string]$Root) {
    Join-Path $Root '.codex_discord_rust.maintenance.v2'
}

function Read-CdrMaintenanceState([string]$Path) {
    if ((Get-Item -LiteralPath $Path).Length -gt 32768) { throw 'maintenance_state_too_large' }
    $s = [IO.File]::ReadAllText($Path) | ConvertFrom-Json
    Assert-CdrMaintenanceShutdownPolicy $s.ShutdownPolicy
    if($s.PSObject.Properties['CompletionPolicy'] -and $s.CompletionPolicy -cne 'runtime-proof-v1'){
        throw 'maintenance_completion_policy_unsupported'
    }
    if ($s.Version -ne 2 -or $s.Operation -notmatch '^[a-f0-9]{32}$' -or
        $Path -cne (Get-CdrMaintenancePath $RepoRoot) -or $s.RepoRoot -cne $RepoRoot -or
        $s.BinaryPath -cne $BinaryPath -or $s.Fence.ProcessIdentity -notmatch '^\d+\|\d+$' -or
        $s.Fence.Nonce -cne $s.Operation -or $s.Attempts -lt 0 -or $s.Attempts -gt 3 -or
        $s.Phase -notin @('planned','prepared','drained','stop_requested','stopped',
            'installing','candidate_installed','mutation_started','cleanup_confirmed',
            'readiness_verified','launch_ready','launched','healthy','notifying','verified')) {
        throw 'maintenance_state_invalid; preserved'
    }
    foreach ($name in @('BaselineHash','CandidateHash','OperatorHash','EnvHash')) {
        if ($s.$name -notmatch '^[A-F0-9]{64}$') { throw 'maintenance_artifact_pin_invalid' }
    }
    $bundle = Join-Path $RepoRoot ('.codex-discord-backups/maintenance-v2-'+$s.Operation)
    if ($s.Bundle -cne $bundle -or $s.CandidatePath -cne (Join-Path $bundle 'candidate.exe') -or
        $s.OperatorPath -cne (Join-Path $bundle 'operator.exe') -or
        $s.TaskName -cne ('Codex Maintenance V2 '+$s.Operation) -or
        $s.Fence.RuntimeId -notmatch '^[A-Za-z0-9_-]{1,128}$' -or
        $s.NotifyChannel -cne '1543277263418826775') { throw 'maintenance_ticket_paths_invalid' }
    $created = [DateTimeOffset]::Parse($s.CreatedAt)
    $deadline = [DateTimeOffset]::Parse($s.Deadline)
    if ($deadline -le $created -or ($deadline-$created).TotalMinutes -gt 30) {
        throw 'maintenance_deadline_invalid'
    }
    if ($s.PSObject.Properties['DeploymentPolicy']) {
        Assert-CdrStabilizationState $s
    }
    return $s
}

function Get-CdrMaintenanceProgramPaths([string]$DeploymentPolicy = '') {
    @('codex-discord-rust-watchdog.ps1','codex-discord-rust-control.ps1','codex-discord-rust-drain.ps1',
        'scripts/CdrLaunchJournal.ps1','scripts/CdrDeploymentRecovery.ps1','scripts/CdrRestartTransaction.ps1',
        'scripts/CdrForceRestart.ps1','scripts/CdrMaintenanceCompatibility.ps1',
        'scripts/CdrRestartPreparation.ps1','scripts/CdrRuntimeIdentity.psm1',
        'scripts/CdrBuildProvenance.psm1','scripts/CdrNativeProcess.psm1','scripts/CdrAsyncRecoveryCompatibility.ps1',
        'scripts/Invoke-CdrMaintenance.ps1','scripts/CdrMaintenanceState.ps1','scripts/CdrMaintenanceEngine.ps1',
        'scripts/CdrMaintenanceCommand.ps1','scripts/CdrMaintenanceDiagnostics.ps1','scripts/CdrMaintenanceActions.ps1','scripts/CdrMaintenanceLaunch.ps1','scripts/CdrMaintenanceNotification.ps1',
        'scripts/CdrMaintenanceSchedule.ps1','scripts/CdrMaintenanceBackup.ps1','scripts/CdrMaintenanceFailure.ps1',
        'scripts/CdrMaintenanceCompletion.ps1','scripts/CdrMaintenanceNotificationResult.ps1','scripts/CdrMaintenanceNoticeJournal.ps1',
        'scripts/CdrMaintenanceCompletionAudit.ps1',
        'codex-discord-tray.ps1','codex-discord-tray-runtime.ps1','codex-discord-tray-restart-runtime.ps1')
    if ($DeploymentPolicy -ceq 'stabilization-held-v1') {
        @('scripts/CdrRuntimeLaunchCompatibility.ps1',
            'scripts/CdrInstallLaunchCompatibility.ps1','scripts/Register-CdrStabilization.ps1')
    } elseif ($DeploymentPolicy) { throw 'maintenance_deployment_policy_unsupported' }
}

function Assert-CdrMaintenanceShutdownPolicy([string]$Policy) {
    if ($Policy -cne 'live-handshake-v1') { throw 'maintenance_shutdown_policy_missing_or_unsupported' }
}

function Assert-CdrMaintenanceProgramPins($State) {
    $policy = if ($State.PSObject.Properties['DeploymentPolicy']) { $State.DeploymentPolicy } else { '' }
    $expected = @(Get-CdrMaintenanceProgramPaths $policy)
    if (@($State.ProgramPins).Count -ne $expected.Count) { throw 'maintenance_program_pins_missing' }
    foreach ($path in $expected) {
        $pins = @($State.ProgramPins | Where-Object { $_.Path -ceq $path })
        if ($pins.Count -ne 1 -or (Get-CdrArtifactHash (Join-Path $RepoRoot $path)) -cne $pins[0].Hash) {
            throw 'maintenance_program_changed_after_arming; no further action'
        }
    }
}

function Save-CdrMaintenanceState($State, [string]$Path) {
    # Caller holds both operation and common locks. Fail if ownership changed.
    $current = [IO.File]::ReadAllText($Path) | ConvertFrom-Json
    if ($current.Version -ne 2 -or $current.Operation -cne $State.Operation) {
        throw 'maintenance_owner_changed; preserved'
    }
    Write-AtomicRestartMarker $Path ($State | ConvertTo-Json -Depth 10 -Compress)
}

function Set-CdrMaintenancePhase($State, [string]$Path, [string]$Phase) {
    $State.Phase = $Phase
    Save-CdrMaintenanceState $State $Path
}

function Assert-CdrMaintenanceBudget($State) {
    if ($State.Halted -or $State.Attempts -ge 3 -or
        [DateTimeOffset]::UtcNow -ge [DateTimeOffset]::Parse($State.Deadline)) {
        throw 'maintenance_recovery_budget_exhausted_or_halted; no automatic retry'
    }
}

function Get-CdrMaintenanceRemainingSeconds($State, [int]$Limit = 0) {
    $seconds = [math]::Floor(([DateTimeOffset]::Parse($State.Deadline)-[DateTimeOffset]::UtcNow).TotalSeconds)
    if ($seconds -lt 1) { throw 'maintenance_deadline_exceeded; no new side effect' }
    if ($Limit -gt 0) { $seconds=[math]::Min($seconds,$Limit) }
    return [int]$seconds
}

function Assert-CdrMaintenanceDeadline($State) { $null=Get-CdrMaintenanceRemainingSeconds $State }

function Assert-CdrCertifiedBaseline([string]$Hash) {
    # This catalog is code-reviewed release evidence, NOT a caller-supplied true.
    $catalog = Get-Content -LiteralPath (Join-Path $RepoRoot 'docs/rust-migration/evidence/drain-certified-artifacts.json') -Raw | ConvertFrom-Json
    $entries = @($catalog.artifacts | Where-Object { $_.artifact_sha256 -ceq $Hash })
    if ($catalog.version -ne 1 -or $entries.Count -ne 1) {
        throw 'T1_installed_drain_contract_unproven; prepare is prohibited'
    }
    $entry = $entries[0]
    foreach ($name in @('provenance','failure_contract')) {
        $proof = $entry.$name
        $path = [IO.Path]::GetFullPath((Join-Path $RepoRoot $proof.path))
        $prefix = [IO.Path]::GetFullPath((Join-Path $RepoRoot 'docs/rust-migration/evidence')) + '\'
        if (-not $path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) -or
            $proof.sha256 -notmatch '^[A-F0-9]{64}$' -or
            (Get-CdrArtifactHash $path) -cne $proof.sha256) { throw 'T1_evidence_pin_mismatch' }
    }
}

# Held stabilization is a separate ticket, never an authority to dispose/release.
function Assert-CdrStabilizationState($State) {
    if ($State.DeploymentPolicy -cne 'stabilization-held-v1' -or
        -not $State.PSObject.Properties['Compatibility']) {
        throw 'maintenance_deployment_policy_unsupported'
    }
    $c = $State.Compatibility
    if ($c.DatabasePath -cne (Join-Path $RepoRoot 'discord_mirror.sqlite') -or
        $c.CapabilityPath -cne (Join-Path $State.Bundle 'capabilities.json') -or
        $c.LaunchManifestPath -cne (Join-Path $State.Bundle 'launch-artifacts.json') -or
        $c.CapabilityHash -cnotmatch '^[A-F0-9]{64}$' -or
        $c.LaunchManifestHash -cnotmatch '^[A-F0-9]{64}$' -or
        $c.IncidentThread -cne '01a06156-56cd-70b0-af02-2de7445ba4c7' -or
        $c.OriginalJob -cne 'b3d5a1a3-5c3e-4764-967b-0cef767efde9' -or
        $c.OriginalDisposition -cne 'held-no-replay-no-disposal-no-release' -or
        $State.OperatorHash -cne $State.CandidateHash) {
        throw 'maintenance_stabilization_scope_or_pins_invalid'
    }
}
