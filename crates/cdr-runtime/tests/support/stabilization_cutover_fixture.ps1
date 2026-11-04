param([string]$Repository,[string]$FixtureRoot,[string]$Candidate,[string]$Scenario)
$ErrorActionPreference='Stop'
$RepoRoot=[IO.Path]::GetFullPath($FixtureRoot)
$BinaryPath=[IO.Path]::GetFullPath($Candidate)
$EnvPath=Join-Path $RepoRoot '.env'
$operation='1234567890abcdef1234567890abcdef'
$bundle=Join-Path $RepoRoot ('.codex-discord-backups/maintenance-v2-'+$operation)
[void][IO.Directory]::CreateDirectory($bundle)
. (Join-Path $Repository 'codex-discord-rust-control.ps1')
. (Join-Path $Repository 'codex-discord-rust-drain.ps1')
$moduleRoot=if ($env:CDR_D4_BASELINE) { $env:CDR_D4_BASELINE } else { $Repository }
. (Join-Path $moduleRoot 'scripts/CdrMaintenanceState.ps1')
. (Join-Path $moduleRoot 'scripts/CdrMaintenanceActions.ps1')
if (-not $env:CDR_D4_BASELINE) { . (Join-Path $Repository 'scripts/CdrMaintenanceCompatibility.ps1') }
$State=[pscustomobject]@{
    Version=2;ShutdownPolicy='live-handshake-v1';Operation=$operation;RepoRoot=$RepoRoot;BinaryPath=$BinaryPath
    Phase='candidate_installed';Attempts=0;Halted=$false;LastError='';Bundle=$bundle
    CreatedAt=[DateTimeOffset]::UtcNow.ToString('o');Deadline=[DateTimeOffset]::UtcNow.AddMinutes(5).ToString('o')
    Fence=[pscustomobject]@{RuntimeId='fixture';ProcessIdentity='42|99';Nonce=$operation}
    BaselineHash=('A'*64);CandidateHash=(Get-CdrArtifactHash $BinaryPath);OperatorHash=(Get-CdrArtifactHash $BinaryPath)
    EnvHash='';CandidatePath=(Join-Path $bundle 'candidate.exe');OperatorPath=(Join-Path $bundle 'operator.exe')
    TaskName=('Codex Maintenance V2 '+$operation);NotifyChannel='1543277263418826775';Heartbeats=@()
    ActiveCommand=$null;PreStopBackup=$null;PostStopBackup=$null;FailureObservation=$null
    DeploymentPolicy='stabilization-held-v1'
    Compatibility=[pscustomobject]@{
        DatabasePath=(Join-Path $RepoRoot 'discord_mirror.sqlite')
        CapabilityPath=(Join-Path $bundle 'capabilities.json');CapabilityHash=('B'*64)
        LaunchManifestPath=(Join-Path $bundle 'launch-artifacts.json');LaunchManifestHash=('C'*64)
        IncidentThread='01a06156-56cd-70b0-af02-2de7445ba4c7';OriginalJob='b3d5a1a3-5c3e-4764-967b-0cef767efde9'
        OriginalDisposition='held-no-replay-no-disposal-no-release'
    }
}
if ($Scenario -like 'engine-*') {
    $State.EnvHash='D'*64
    . (Join-Path $Repository 'crates/cdr-runtime/tests/support/stabilization_cutover_engine.ps1')
    return
}
$files=@('codex-discord-rust-watchdog.ps1','codex-discord-rust-control.ps1','codex-discord-rust-drain.ps1',
    'scripts/CdrAsyncRecoveryCompatibility.ps1','scripts/CdrRuntimeLaunchCompatibility.ps1',
    'scripts/CdrDeploymentRecovery.ps1','scripts/CdrLaunchJournal.ps1','scripts/CdrRestartTransaction.ps1',
    'scripts/CdrForceRestart.ps1','scripts/CdrInstallLaunchCompatibility.ps1',
    'scripts/CdrRestartPreparation.ps1','scripts/CdrRuntimeIdentity.psm1','scripts/CdrBuildProvenance.psm1','scripts/CdrNativeProcess.psm1')
$entries=@()
foreach ($relative in $files) {
    $destination=Join-Path $RepoRoot $relative
    [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($destination))
    [IO.File]::Copy((Join-Path $Repository $relative),$destination,$false)
    if ($relative -cne 'scripts/CdrInstallLaunchCompatibility.ps1') {
        $entries+=@{path=$relative;sha256=(Get-CdrArtifactHash $destination).ToLowerInvariant()}
    }
}
[IO.File]::WriteAllText($EnvPath,('CODEX_DISCORD_MIRROR_DB='+$State.Compatibility.DatabasePath),[Text.UTF8Encoding]::new($false))
$State.EnvHash=Get-CdrArtifactHash $EnvPath
[IO.File]::WriteAllText($State.Compatibility.CapabilityPath,([ordered]@{
    protocol='cdr-artifact-capabilities-v1';artifact_sha256=$State.CandidateHash.ToLowerInvariant()
    async_resolution_max_format=1;async_recovery_policy_max_format=1
}|ConvertTo-Json -Compress),[Text.UTF8Encoding]::new($false))
[IO.File]::WriteAllText($State.Compatibility.LaunchManifestPath,([ordered]@{
    protocol='cdr-runtime-launch-artifacts-v1';files=$entries
}|ConvertTo-Json -Depth 5 -Compress),[Text.UTF8Encoding]::new($false))
$State.Compatibility.CapabilityHash=Get-CdrArtifactHash $State.Compatibility.CapabilityPath
$State.Compatibility.LaunchManifestHash=Get-CdrArtifactHash $State.Compatibility.LaunchManifestPath
$seal=Join-Path $RepoRoot '.codex_discord_bot.disabled'
[IO.File]::WriteAllText($seal,$operation,[Text.UTF8Encoding]::new($false))
$required=Join-Path $RepoRoot '.codex_discord_rust.compatibility.required'
$contract=Join-Path $RepoRoot '.codex_discord_rust.compatibility.json'
function Assert-CdrMaintenanceNoRuntime {}
function Assert-CdrMaintenanceArtifacts { Assert-CdrStabilizationArtifacts $State }
function Assert-CdrMaintenanceMarkers { Assert-CdrMarkerOwner $seal $operation }
$script:writers=0;$script:starts=0;$script:modes=@()
function Invoke-CdrMaintenanceCommand {
    param($s,$File,$Arguments,$LimitSeconds,[switch]$PassThru)
    $script:modes+=@($Arguments[0]);$script:writers++
}
if ($Scenario -like 'preflight-*') {
    . (Join-Path $PSScriptRoot 'stabilization_preflight_fixture.ps1')
    return
}
if ($Scenario -eq 'legacy') {
    $State.PSObject.Properties.Remove('DeploymentPolicy')
    $State.OperatorPath=$BinaryPath
    Invoke-CdrMaintenanceProbe $State 'preflight'
    Invoke-CdrMaintenanceProbe $State 'cleanup'
    if (($script:modes -join ',') -cne 'preflight,cleanup') { throw 'legacy operator dispatch changed' }
    return
}
if ($Scenario -eq 'wiring') {
    $paths=@(Get-CdrMaintenanceProgramPaths 'stabilization-held-v1')
    foreach ($relative in @('scripts/CdrMaintenanceCompatibility.ps1','scripts/CdrInstallLaunchCompatibility.ps1','scripts/Register-CdrStabilization.ps1')) {
        if ($paths -cnotcontains $relative) { throw 'D4 executable dependency not pinned' }
    }
    $source=[IO.File]::ReadAllText((Join-Path $Repository 'codex-discord-rust-watchdog.ps1'))
    if (-not $source.Contains('Invoke-CdrProductionCheckedLaunch -DeadlineUtc')) { throw 'watchdog does not use checked production launch' }
    $State.DeploymentPolicy='unsupported'
    $rejected=$false;try {Assert-CdrStabilizationState $State} catch {$rejected=$true}
    if (-not $rejected) { throw 'unknown policy accepted' }
    return
}
$controlGuard=$null;$writer=$null;$errorText=''
try {
    $controlGuard=Enter-CdrControl -Root $RepoRoot -Purpose 'maintenance'
    if ($Scenario -eq 'foreign') { [IO.File]::WriteAllText($required,('0'*64)) }
    if ($Scenario -eq 'foreign-seal') { [IO.File]::WriteAllText($seal,'foreign-owner') }
    if ($Scenario -eq 'writer') { $writer=[IO.File]::Open($State.Compatibility.DatabasePath,'Open','ReadWrite','ReadWrite') }
    try {
        if ($Scenario -ne 'missing') { Enable-CdrMaintenanceCompatibility $State }
        if ($Scenario -eq 'repeat') {
            $beforeRequired=Get-CdrArtifactHash $required;$beforeContract=Get-CdrArtifactHash $contract
            $requiredTime=[IO.File]::GetLastWriteTimeUtc($required);$contractTime=[IO.File]::GetLastWriteTimeUtc($contract)
            Enable-CdrMaintenanceCompatibility $State
            if ((Get-CdrArtifactHash $required) -cne $beforeRequired -or (Get-CdrArtifactHash $contract) -cne $beforeContract -or
                [IO.File]::GetLastWriteTimeUtc($required) -ne $requiredTime -or [IO.File]::GetLastWriteTimeUtc($contract) -ne $contractTime) {
                throw 'repeated arming rewrote intent'
            }
        }
        if ($Scenario -eq 'changed-env') { [IO.File]::AppendAllText($EnvPath,([Environment]::NewLine+'CHANGED=1')) }
        Invoke-CdrMaintenanceFullReadiness $State
        Invoke-CdrMaintenanceProbe $State 'cleanup'
        Invoke-CdrProductionCheckedLaunch -DeadlineUtc ([DateTimeOffset]::Parse($State.Deadline)) -Launch {
            $script:starts++
            $output=& $BinaryPath --help 2>&1
            if ($LASTEXITCODE -ne 0 -or -not ($output -join [Environment]::NewLine).Contains('--env')) { throw 'native help child failed' }
        }
    } catch { $errorText=$_.Exception.Message }
    if ($Scenario -in @('valid','repeat')) {
        if ($errorText) { throw $errorText }
        if ($script:writers -ne 1 -or $script:starts -ne 1) { throw 'positive path did not reach exactly one readiness and start' }
    } else {
        if (-not $errorText -or $script:writers -ne 0 -or $script:starts -ne 0) {
            throw "unsafe rejected path: writers=$script:writers starts=$script:starts error=$errorText"
        }
        if ($Scenario -eq 'foreign' -and [IO.File]::ReadAllText($required) -cne ('0'*64)) { throw 'foreign intent changed' }
        if ($Scenario -in @('writer','foreign-seal','missing') -and
            ([IO.File]::Exists($required) -or [IO.File]::Exists($contract))) { throw 'rejected arming published intent' }
        $expected=@{'missing'='compatibility_not_armed';'foreign'='Existing required intent differs';
            'foreign-seal'='Foreign maintenance marker';'changed-env'='Pinned artifact hash mismatch'}
        if ($Scenario -ne 'writer' -and -not $errorText.Contains($expected[$Scenario])) { throw "wrong rejection: $errorText" }
    }
    Write-Output "scenario=$Scenario writers=$script:writers starts=$script:starts error=$errorText"
} finally {
    if ($null -ne $writer) { $writer.Dispose() }
    if ($null -ne $controlGuard) { $controlGuard.Dispose() }
}
