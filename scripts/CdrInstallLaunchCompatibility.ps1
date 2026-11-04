# Candidate-only first arming. The reviewed cutover owner must separately stop
# and drain old writers, install the reviewed files, and retain its real control
# lease. This helper does not copy code, migrate/restore a DB, unseal, or launch.
Set-StrictMode -Version Latest

function Get-CdrInstallTextHash {
    param([string]$Text)
    $sha=[Security.Cryptography.SHA256]::Create()
    try {
        return [BitConverter]::ToString($sha.ComputeHash(
            [Text.UTF8Encoding]::new($false).GetBytes($Text))).Replace('-','').ToLowerInvariant()
    } finally { $sha.Dispose() }
}

function Assert-CdrInstallDeadline {
    param([DateTimeOffset]$DeadlineUtc)
    if ([DateTimeOffset]::UtcNow -ge $DeadlineUtc) {
        throw 'installation_deadline_exceeded; persistent intent is preserved'
    }
}

function Get-CdrInstallPhysicalPath {
    param([string]$Path)
    if (-not [IO.Path]::IsPathRooted($Path)) { throw 'Installation paths must be absolute' }
    $full=ConvertTo-CdrComparablePath $Path
    if ($full -notmatch '^[A-Za-z]:\\') { throw 'Installation requires a local drive path' }
    $item=Get-Item -LiteralPath $full -ErrorAction Stop
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw 'Installation refuses reparse paths'
    }
    $directory=if ($item.PSIsContainer) { $item } else { $item.Directory }
    while ($null -ne $directory) {
        if ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw 'Installation refuses reparse ancestors'
        }
        $directory=$directory.Parent
    }
    return $full
}

function Assert-CdrInstallControlOwner {
    param([string]$Root,$ControlGuard)
    Assert-CdrLaunchControl -Root $Root -ControlGuard $ControlGuard
    $other=$null
    $exclusive=$false
    try {
        $other=[IO.File]::Open($ControlGuard.Name,'Open','ReadWrite','ReadWrite')
    } catch [IO.IOException] {
        if (($_.Exception.HResult -band 0xffff) -notin @(32,33)) { throw }
        $exclusive=$true
    } finally { if ($null -ne $other) { $other.Dispose() } }
    if (-not $exclusive) { throw 'Installation requires an exclusive write lease' }
    $owner=(Read-CdrPinnedText $ControlGuard 2048) | ConvertFrom-Json -ErrorAction Stop
    $process=[Diagnostics.Process]::GetCurrentProcess()
    try {
        $ticks=$process.StartTime.ToUniversalTime().Ticks
        $ticks-=($ticks % 10)
        if ($owner.Version -ne 1 -or $owner.ProcessId -ne $PID -or
            $owner.StartTicks -cne [string]$ticks -or $owner.Purpose -cne 'maintenance' -or
            -not [StringComparer]::OrdinalIgnoreCase.Equals(
                (ConvertTo-CdrComparablePath $owner.Root),$Root) -or
            -not [StringComparer]::OrdinalIgnoreCase.Equals(
                (ConvertTo-CdrComparablePath $owner.Executable),
                (ConvertTo-CdrComparablePath $process.MainModule.FileName))) {
            throw 'Installation control owner identity differs'
        }
    } finally { $process.Dispose() }
}

function Add-CdrReviewedLaunchPins {
    param([string]$Root,[IO.FileStream]$Manifest,$Pins)
    $decl=(Read-CdrPinnedText $Manifest 16384) | ConvertFrom-Json -ErrorAction Stop
    $required=@('codex-discord-rust-watchdog.ps1','codex-discord-rust-control.ps1',
        'codex-discord-rust-drain.ps1','scripts/CdrAsyncRecoveryCompatibility.ps1',
        'scripts/CdrRuntimeLaunchCompatibility.ps1','scripts/CdrDeploymentRecovery.ps1',
        'scripts/CdrLaunchJournal.ps1','scripts/CdrRestartTransaction.ps1',
        'scripts/CdrForceRestart.ps1','scripts/CdrRestartPreparation.ps1',
        'scripts/CdrRuntimeIdentity.psm1','scripts/CdrBuildProvenance.psm1','scripts/CdrNativeProcess.psm1')
    if ($decl.protocol -cne 'cdr-runtime-launch-artifacts-v1' -or
        @($decl.files).Count -ne $required.Count) {
        throw 'Reviewed launch artifact inventory is incomplete'
    }
    $seen=New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
    foreach ($entry in $decl.files) {
        if ($required -cnotcontains [string]$entry.path -or -not $seen.Add([string]$entry.path)) {
            throw 'Reviewed launch artifact inventory has an unexpected or duplicate path'
        }
        $path=Get-CdrInstallPhysicalPath (Join-Path $Root $entry.path)
        $Pins.Add((Get-CdrPinnedReadHandle $path $entry.sha256))
    }
}

function Install-CdrRuntimeLaunchCompatibility {
    param(
        [Parameter(Mandatory=$true)][string]$RepoRoot,
        [Parameter(Mandatory=$true)][string]$CandidatePath,
        [Parameter(Mandatory=$true)][string]$EnvironmentPath,
        [Parameter(Mandatory=$true)][string]$DatabasePath,
        [Parameter(Mandatory=$true)][string]$CapabilityManifestPath,
        [Parameter(Mandatory=$true)][string]$ExpectedCandidateSha256,
        [Parameter(Mandatory=$true)][string]$ExpectedEnvironmentSha256,
        [Parameter(Mandatory=$true)][string]$ExpectedCapabilitySha256,
        [Parameter(Mandatory=$true)][string]$ReviewedLaunchManifestPath,
        [Parameter(Mandatory=$true)][string]$ExpectedLaunchManifestSha256,
        [Parameter(Mandatory=$true)][string]$Operation,
        $ControlGuard,
        [Parameter(Mandatory=$true)][DateTimeOffset]$DeadlineUtc
    )
    Assert-CdrInstallDeadline $DeadlineUtc
    if ($Operation -cnotmatch '^[A-Za-z0-9:._-]{1,128}$') { throw 'Invalid installation owner' }
    $RepoRoot=Get-CdrInstallPhysicalPath $RepoRoot
    if (-not (Get-Item -LiteralPath $RepoRoot).PSIsContainer) { throw 'Installation root is not a directory' }
    Assert-CdrInstallControlOwner -Root $RepoRoot -ControlGuard $ControlGuard
    $CandidatePath=Get-CdrInstallPhysicalPath $CandidatePath
    $EnvironmentPath=Get-CdrInstallPhysicalPath $EnvironmentPath
    $DatabasePath=Get-CdrInstallPhysicalPath $DatabasePath
    $CapabilityManifestPath=Get-CdrInstallPhysicalPath $CapabilityManifestPath
    $ReviewedLaunchManifestPath=Get-CdrInstallPhysicalPath $ReviewedLaunchManifestPath
    $requiredPath=Join-Path $RepoRoot '.codex_discord_rust.compatibility.required'
    $contractPath=Join-Path $RepoRoot '.codex_discord_rust.compatibility.json'
    $pins=New-Object 'Collections.Generic.List[IO.FileStream]'
    try {
        $pins.Add((Get-CdrPinnedReadHandle (Join-Path $RepoRoot '.codex_discord_bot.disabled') (Get-CdrInstallTextHash $Operation)))
        $pins.Add((Get-CdrPinnedReadHandle $CandidatePath $ExpectedCandidateSha256))
        $pins.Add((Get-CdrPinnedReadHandle $EnvironmentPath $ExpectedEnvironmentSha256))
        $pins.Add((Get-CdrPinnedReadHandle $CapabilityManifestPath $ExpectedCapabilitySha256))
        $manifest=Get-CdrPinnedReadHandle $ReviewedLaunchManifestPath $ExpectedLaunchManifestSha256
        $pins.Add($manifest)
        Add-CdrReviewedLaunchPins -Root $RepoRoot -Manifest $manifest -Pins $pins
        $text=[ordered]@{
            protocol='cdr-runtime-launch-v1';root=$RepoRoot
            candidate_path=$CandidatePath;candidate_sha256=$ExpectedCandidateSha256
            environment_path=$EnvironmentPath;environment_sha256=$ExpectedEnvironmentSha256
            database_path=$DatabasePath;capability_path=$CapabilityManifestPath;capability_sha256=$ExpectedCapabilitySha256
            launcher_manifest_path=$ReviewedLaunchManifestPath;launcher_manifest_sha256=$ExpectedLaunchManifestSha256
            installed_by=$Operation
        } | ConvertTo-Json -Compress
        $hash=Get-CdrInstallTextHash $text
        $requiredExists=Test-Path -LiteralPath $requiredPath
        $contractExists=Test-Path -LiteralPath $contractPath
        if ($requiredExists) {
            $required=Get-CdrPinnedReadHandle $requiredPath ((Get-CdrArtifactHash $requiredPath).ToLowerInvariant())
            $pins.Add($required)
            if ((Read-CdrPinnedText $required 128).Trim() -cne $hash) {
                throw 'Existing required intent differs; no replacement or clearing is permitted'
            }
        }
        if ($contractExists) { $pins.Add((Get-CdrPinnedReadHandle $contractPath $hash)) }
        Assert-CdrInstallDeadline $DeadlineUtc
        $probe=@{
            CandidatePath=$CandidatePath;DatabasePath=$DatabasePath
            CapabilityManifestPath=$CapabilityManifestPath
            ExpectedCandidateSha256=$ExpectedCandidateSha256
            ExpectedCapabilitySha256=$ExpectedCapabilitySha256
            EnvironmentPath=$EnvironmentPath;ExpectedEnvironmentSha256=$ExpectedEnvironmentSha256
            WorkingDirectory=$RepoRoot;RequireRecoveryPolicy=$true;DeadlineUtc=$DeadlineUtc
        }
        $null=Assert-CdrAsyncRecoveryCompatibility @probe
        Assert-CdrInstallControlOwner -Root $RepoRoot -ControlGuard $ControlGuard
        Assert-CdrInstallDeadline $DeadlineUtc
        # Either one-file prefix is a durable launch fence. Never roll it back.
        if (-not $requiredExists) { Write-NewCdrMarker -Path $requiredPath -Text $hash }
        Assert-CdrInstallDeadline $DeadlineUtc
        if (-not $contractExists) { Write-NewCdrMarker -Path $contractPath -Text $text }
        $pins.Add((Get-CdrPinnedReadHandle $contractPath $hash))
        $retained=Get-CdrPinnedReadHandle $requiredPath ((Get-CdrArtifactHash $requiredPath).ToLowerInvariant())
        $pins.Add($retained)
        if ((Read-CdrPinnedText $retained 128).Trim() -cne $hash) {
            throw 'Installed required intent changed; launch remains held'
        }
        Assert-CdrInstallControlOwner -Root $RepoRoot -ControlGuard $ControlGuard
        return [pscustomobject]@{
            protocol='cdr-runtime-install-v1';operation=$Operation;contract_sha256=$hash
            admission_authorized=$false;recovery_authorized=$false;launch_performed=$false
        }
    } finally { foreach ($pin in $pins) { $pin.Dispose() } }
}
