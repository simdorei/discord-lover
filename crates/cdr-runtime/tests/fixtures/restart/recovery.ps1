param([switch]$Prepare,[string]$CaseName)
$ErrorActionPreference='Stop'
$root=$env:ENTRY_ROOT
$source=Get-Content (Join-Path $root 'codex-discord-rust-watchdog.ps1') -Raw
$boundary=$source.IndexOf('# CONTROL_ENTRY:')
. ([scriptblock]::Create($source.Substring(0,$boundary))) -RepoRoot $root -BinaryPath (Join-Path $root 'probe.exe') -PrepareRestart:$Prepare
$entry=[scriptblock]::Create($source.Substring($boundary))
. (Join-Path $PSScriptRoot 'artifact.ps1')
$script:newAlive=$false;$script:starts=0
function Get-VerifiedRuntimeProcess {if($script:newAlive){[pscustomobject]@{Id=77;Path=$BinaryPath}}}
function Get-RustProcessIdentity {param($Process) if($Process){"$($Process.Id)|99"}else{''}}
function Get-Process {param($Id,$ErrorAction) if($Id -eq 77 -and $script:newAlive){[pscustomobject]@{Id=77;Path=$BinaryPath}}}
function Get-HeartbeatHealth {[pscustomobject]@{Healthy=$true;Bootstrap=$false}}
function Stop-VerifiedRuntime {throw 'kill forbidden'}
function Start-Process {throw 'real start forbidden'}
function Start-RustRuntime {
 param([switch]$ResumeRemoteMcp)
 if(-not $ResumeRemoteMcp){throw 'handoff flag lost'}
 Set-CdrLaunchStarting
 $script:starts++;$script:newAlive=$true
 Set-CdrLaunchChild ([pscustomobject]@{Id=77;Path=$BinaryPath})
}
. ([scriptblock]::Create([IO.File]::ReadAllText((Join-Path $env:RESTART_FIXTURES ($CaseName+'.ps1')),[Text.Encoding]::UTF8)))
exit 0
