[CmdletBinding()]
param([string]$TargetDirectory)
$ErrorActionPreference='Stop'
$repository=Split-Path -Parent $PSScriptRoot
Import-Module (Join-Path $PSScriptRoot 'CdrBuildProvenance.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'CdrNativeProcess.psm1') -Force
$root=Join-Path ([IO.Path]::GetTempPath()) ('cdr-build-proof-'+[guid]::NewGuid().ToString('N'))
if(-not $TargetDirectory){$TargetDirectory=Join-Path $root 'artifacts'}
function Put([string]$Path,[string]$Text){[void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($Path));[IO.File]::WriteAllText($Path,$Text,[Text.UTF8Encoding]::new($false))}
function Check($Value,[string]$Message){if(-not $Value){throw $Message}}
function Git([string[]]$Words){Invoke-CdrNative git (@('-C',$root)+$Words) | Out-Null}
try {
    Put (Join-Path $root 'Cargo.toml') @'
[package]
name="cdr-build-proof"
version="0.1.0"
edition="2024"
[dependencies]
proof-dep={path="dependency"}
[build-dependencies]
serde_json="1"
'@
    Put (Join-Path $root 'dependency/Cargo.toml') @'
[package]
name="proof-dep"
version="0.1.0"
edition="2024"
'@
    Put (Join-Path $root 'dependency/src/lib.rs') 'pub const VALUE:u8=1;'
    Put (Join-Path $root 'src/main.rs') 'fn main(){assert!(proof_dep::VALUE>0);println!("{}",include_str!(concat!(env!("OUT_DIR"),"/build-info.json")));}'
    Put (Join-Path $root '.gitignore') "artifacts/`n"
    Copy-Item -LiteralPath (Join-Path $repository 'crates/cdr-runtime/build.rs') -Destination (Join-Path $root 'build.rs')
    Git @('init','-q');Git @('config','core.autocrlf','false');Git @('config','user.name','Fixture');Git @('config','user.email','fixture@example.invalid')
    Push-Location $root
    try{Invoke-CdrNative cargo @('generate-lockfile','--offline') -WorkingDirectory $root | Out-Null}finally{Pop-Location}
    Git @('add','.');Git @('commit','-qm','initial fixture')
    $binary=Join-Path $TargetDirectory 'debug/cdr-build-proof.exe'
    $arguments=@('build','--offline','--locked','--target-dir',$TargetDirectory)
    $first=Invoke-CdrRuntimeBuild $root $binary $arguments
    Check ($first.State -eq 'clean') 'Initial build not clean'
    Git @('commit','--allow-empty','-qm','commit-only change')
    $second=Invoke-CdrRuntimeBuild $root $binary $arguments
    Check ($second.Commit -ne $first.Commit -and $second.Tree -eq $first.Tree) 'Commit-only test did not change HEAD'
    Check ((Read-CdrBuildInfo $binary).source_commit -eq $second.Commit) 'Same-cache commit metadata stayed stale'
    Put (Join-Path $root 'dependency/src/lib.rs') 'pub const VALUE:u8=2;'
    $dirty=Invoke-CdrRuntimeBuild $root $binary $arguments
    Check ($dirty.State -eq 'dirty' -and (Read-CdrBuildInfo $binary).source_state -eq 'dirty') 'Dependency edit not marked dirty'
    Git @('checkout','--','dependency/src/lib.rs')
    $clean=Invoke-CdrRuntimeBuild $root $binary $arguments
    Check ($clean.State -eq 'clean') 'Clean restoration not recognized'
    Check (-not [Environment]::GetEnvironmentVariable('CDR_BUILD_SOURCE_STATE','Process')) 'Build input leaked to parent'
    Push-Location $root
    try{Invoke-CdrNative cargo $arguments -WorkingDirectory $root | Out-Null}finally{Pop-Location}
    $unknown=Read-CdrBuildInfo $binary
    Check ($unknown.source_state -eq 'unknown' -and -not $unknown.source_commit) 'Direct Cargo reused a previous known source'
    try {$null=Assert-CdrBuildSource $binary $clean;throw 'Expected mismatch refusal'}catch{Check ($_.Exception.Message -like '*refusing to relabel*') 'Wrong artifact accepted'}
    # Discord Lover ships a source updater, not the Rust repository's ZIP publisher.
    # Its installer is covered by Test-CdrInstallerBuildProvenance.ps1.
    Write-Output 'build_provenance_tests_passed commit-only dependency-dirty clean known-to-unknown artifact-mismatch'
} catch { Write-Output $_.ScriptStackTrace; throw } finally {
    $absolute=[IO.Path]::GetFullPath($root)
    if([IO.Path]::GetDirectoryName($absolute) -ine [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') -or [IO.Path]::GetFileName($absolute) -notlike 'cdr-build-proof-*'){throw 'Unsafe fixture cleanup path'}
    Remove-Item -LiteralPath $absolute -Recurse -Force
}
