[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$RepoRoot,
    [ValidateSet('external-empty','external-stale','relative-empty','default','installed')]
    [string[]]$Cases=@('external-empty','external-stale','relative-empty','default','installed')
)
$ErrorActionPreference='Stop'
Import-Module (Join-Path $RepoRoot 'scripts/CdrBuildProvenance.psm1') -Force
Import-Module (Join-Path $RepoRoot 'scripts/CdrNativeProcess.psm1') -Force
$sandbox=Join-Path ([IO.Path]::GetTempPath()) ('cdr-installer-proof-'+[guid]::NewGuid().ToString('N'))
function Put([string]$Path,[string]$Text) {
    [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($Path))
    [IO.File]::WriteAllText($Path,$Text,[Text.UTF8Encoding]::new($false))
}
function Check($Value,[string]$Message) { if(-not $Value){throw $Message} }
function Git([string]$Root,[string[]]$Words) { Invoke-CdrNative git (@('-C',$Root)+$Words) | Out-Null }
try {
    foreach($case in $Cases) {
        $root=Join-Path $sandbox $case
        Put (Join-Path $root 'Cargo.toml') @'
[package]
name="cdr-runtime"
version="0.1.0"
edition="2024"
[build-dependencies]
serde_json="1"
'@
        # Only management output is simulated. Cargo, production build.rs, the
        # installer, provenance validation, and artifact staging are real.
        Put (Join-Path $root 'src/main.rs') @'
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--build-info") {
        println!("{}", include_str!(concat!(env!("OUT_DIR"), "/build-info.json")));
    } else {
        assert_eq!(std::env::args().nth(1).as_deref(), Some("--admin"));
        println!("fixture-management-only");
    }
}
'@
        Put (Join-Path $root '.gitignore') "target/`nbuild-output/`n.codex_discord_runtime`n.env`n"
        Put (Join-Path $root '.env') "TEST_SENTINEL=preserve`n"
        $null=[IO.Directory]::CreateDirectory((Join-Path $root 'scripts'))
        Copy-Item -LiteralPath (Join-Path $RepoRoot 'install.ps1') -Destination $root
        Copy-Item -LiteralPath (Join-Path $RepoRoot 'crates/cdr-runtime/build.rs') -Destination (Join-Path $root 'build.rs')
        foreach($file in Get-ChildItem -LiteralPath (Join-Path $RepoRoot 'scripts') -Filter '*.psm1' -File) {
            Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $root 'scripts')
        }
        Git $root @('init','-q')
        Git $root @('config','core.autocrlf','false')
        Git $root @('config','user.name','Fixture')
        Git $root @('config','user.email','fixture@example.invalid')
        Invoke-CdrNative cargo @('generate-lockfile','--offline') -WorkingDirectory $root | Out-Null
        Git $root @('add','.')
        Git $root @('commit','-qm','installer fixture')
        $targetInput=switch($case) {
            'default' { '' }
            'relative-empty' { 'build-output' }
            default { Join-Path $sandbox ($case+'-artifacts') }
        }
        $target=if(-not $targetInput){Join-Path $root 'target'}elseif([IO.Path]::IsPathRooted($targetInput)){$targetInput}else{Join-Path $root $targetInput}
        $built=Join-Path $target 'release/cdr-runtime.exe'
        $installed=Join-Path $root 'target/release/cdr-runtime.exe'
        if($case -eq 'external-stale') {
            $old=Invoke-CdrRuntimeBuild $root $built @('build','--release','--offline','--locked','--target-dir',$target)
            Git $root @('commit','--allow-empty','-qm','new source revision')
            Check ((Read-CdrBuildInfo $built).source_commit -eq $old.Commit) 'Stale fixture was not created'
        }
        if($case -eq 'installed') {
            Put $installed 'existing runtime sentinel'
            Put (Join-Path $root '.codex_discord_runtime') 'original selection'
        }
        $expected=Get-CdrBuildSourceState $root
        $arguments=@('-NoProfile','-ExecutionPolicy','Bypass','-File',(Join-Path $root 'install.ps1'),
            '-SkipEnvFile','-SkipCodexPlugin','-CodexHome',(Join-Path $sandbox 'fixture-codex-home'),'-CodexExe',$built)
        $failure=$null
        try {
            Invoke-CdrNative powershell.exe $arguments -WorkingDirectory $sandbox -TimeoutSeconds 120 `
                -EnvironmentOverrides @{CARGO_TARGET_DIR=$targetInput;CARGO_NET_OFFLINE='true';CDR_BUILD_SOURCE_STATE=''} | Out-Null
        } catch { $failure=$_ }
        if($case -eq 'installed') {
            Check ($null -ne $failure -and $failure.Exception.Message -like '*installed runtime exists*') 'Existing install was not refused before compilation'
            Check ([IO.File]::ReadAllText($installed) -ceq 'existing runtime sentinel') 'Existing executable was changed'
            Check ([IO.File]::ReadAllText((Join-Path $root '.codex_discord_runtime')) -ceq 'original selection') 'Existing runtime selection was changed'
            Check (-not [IO.File]::Exists($built)) 'Installer compiled despite refusing an existing installation'
        } else {
            if($null -ne $failure){throw $failure}
            Check ([IO.File]::Exists($built)) "Selected Cargo output is absent: $case"
            $null=Assert-CdrBuildSource $built $expected
            $null=Assert-CdrBuildSource $installed $expected
            Check ((Get-CdrBuildFileHash $built) -ceq (Get-CdrBuildFileHash $installed)) 'Installer did not stage the verified build'
            Check ([IO.File]::ReadAllText((Join-Path $root '.codex_discord_runtime')).Trim() -ceq 'rust') 'Installation did not finish'
        }
        Check ([IO.File]::ReadAllText((Join-Path $root '.env')) -ceq "TEST_SENTINEL=preserve`n") 'Existing env changed'
        Check (-not [Environment]::GetEnvironmentVariable('CDR_BUILD_SOURCE_STATE','Process')) 'Build provenance leaked to parent'
        Write-Output "installer_build_provenance_passed case=$case"
    }
} finally {
    $absolute=[IO.Path]::GetFullPath($sandbox)
    if([IO.Path]::GetDirectoryName($absolute) -ine [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') -or
        [IO.Path]::GetFileName($absolute) -notlike 'cdr-installer-proof-*'){throw 'Unsafe installer fixture cleanup path'}
    if([IO.Directory]::Exists($absolute)){Remove-Item -LiteralPath $absolute -Recurse -Force}
}
