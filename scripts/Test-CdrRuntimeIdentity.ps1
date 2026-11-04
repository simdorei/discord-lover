[CmdletBinding()]
param([Parameter(Mandatory)][string]$RuntimeBinary)
$ErrorActionPreference='Stop'
Import-Module (Join-Path $PSScriptRoot 'CdrRuntimeIdentity.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'CdrBuildProvenance.psm1') -Force
$RepoRoot=Join-Path ([IO.Path]::GetTempPath()) ('cdr-identity-proof-'+[guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($RepoRoot)
. (Join-Path $PSScriptRoot '../codex-discord-rust-drain.ps1')
. (Join-Path $PSScriptRoot '../codex-discord-rust-control.ps1')
function Check($Value,[string]$Message){if(-not $Value){throw $Message}}
function Refuses([scriptblock]$Action,[string]$Pattern){try{&$Action;throw 'unexpected acceptance'}catch{Check ($_.Exception.Message -like $Pattern) $_.Exception.Message}}
try {
    $BinaryPath=Join-Path $RepoRoot 'runtime.exe'
    Copy-Item -LiteralPath $RuntimeBinary -Destination $BinaryPath
    $artifact=Get-CdrRuntimeArtifact $BinaryPath
    Check $artifact.RequireStartupRecord 'Modern binary not recognized'
    Check (-not [IO.File]::Exists((Join-Path $RepoRoot '.env'))) 'Build information needs env'
    Refuses {Get-CdrRuntimeArtifact $BinaryPath ('F'*64)} '*differs from the requested artifact*'
    Refuses {Get-CdrRuntimeArtifact $BinaryPath '' ('a'*40)} '*does not match*'
    $lock=Join-Path $RepoRoot '.codex_discord_rust.runtime.lock'
    [IO.File]::WriteAllText($lock,"pid=77`nstarted_at=1`n")
    Check ($null -eq (Read-CdrRuntimeStartup $RepoRoot $BinaryPath '77|99')) 'Legacy marker invented provenance'
    Refuses {Assert-CdrStartedArtifact $RepoRoot $BinaryPath '77|99' $artifact} '*no matching startup*'
    $startup=@{schema_version=1;process_identity='77|99';executable_path=$BinaryPath;artifact_sha256=$artifact.ArtifactHash.ToLowerInvariant();build_info=$artifact.BuildInfo}
    [IO.File]::WriteAllText($lock,"pid=77`nstartup=$($startup|ConvertTo-Json -Compress -Depth 5)`n")
    Assert-CdrStartedArtifact $RepoRoot $BinaryPath '77|99' $artifact
    Check ($null -eq (Read-CdrRuntimeStartup $RepoRoot $BinaryPath '77|100')) 'PID reuse accepted stale startup provenance'
    $startup.artifact_sha256='e'*64
    [IO.File]::WriteAllText($lock,"pid=77`nstartup=$($startup|ConvertTo-Json -Compress -Depth 5)`n")
    Refuses {Assert-CdrStartedArtifact $RepoRoot $BinaryPath '77|99' $artifact} '*differs from the pinned*'
    $startup.artifact_sha256=$artifact.ArtifactHash.ToLowerInvariant()
    [IO.File]::WriteAllText($lock,"pid=77`nstartup=$($startup|ConvertTo-Json -Compress -Depth 5)`n")
    $pin=Get-CdrRuntimeArtifactPin $BinaryPath $artifact.ArtifactHash
    try {Refuses {$writer=[IO.File]::Open($BinaryPath,'Open','Write','ReadWrite');$writer.Dispose()} '*being used*'}finally{$pin.Dispose()}
    # Completion requires a fresh heartbeat and matching startup + disk artifact.
    function Get-VerifiedRuntimeProcess {[pscustomobject]@{Id=77}}
    function Get-RustProcessIdentity {param($Process);'77|99'}
    function Get-VerifiedRuntimeIdentity {'77|99'}
    function Get-HeartbeatHealth {[pscustomobject]@{Healthy=$true;Bootstrap=$false}}
    $fence=[pscustomobject]@{RuntimeId='runtime';ProcessIdentity='42|1';Nonce='nonce'}
    Write-CdrRestartCompletion $fence '77|99' $artifact
    Check (Test-CdrRestartCompleted $fence $artifact.ArtifactHash) 'Matching completion refused'
    [IO.File]::WriteAllText($BinaryPath,'changed after completed restart')
    Refuses {Test-CdrRestartCompleted $fence $artifact.ArtifactHash} '*hash mismatch*'
    Write-Output 'runtime_identity_tests_passed unknown-source stale-PID changed-artifact pinned-handle completion-recheck'
} finally {
    $absolute=[IO.Path]::GetFullPath($RepoRoot)
    if([IO.Path]::GetDirectoryName($absolute) -ine [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') -or [IO.Path]::GetFileName($absolute) -notlike 'cdr-identity-proof-*'){throw 'Unsafe fixture cleanup path'}
    Remove-Item -LiteralPath $absolute -Recurse -Force
}
