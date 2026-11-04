param([string]$Case)
$ErrorActionPreference='Stop'
$RepoRoot=$env:TRAY_CONTRACT_ROOT
$source=[IO.File]::ReadAllText((Join-Path $env:TRAY_CONTRACT_SOURCE 'codex-discord-rust-watchdog.ps1'))
$offset=$source.IndexOf('# CONTROL_ENTRY:')
if($offset -lt 0){throw 'missing real control boundary'}
$boundary=Join-Path $RepoRoot 'fixture-control-entry.ps1'
[IO.File]::WriteAllText($boundary,$source.Substring($offset))
$BinaryPath=Join-Path $RepoRoot 'runtime.exe'
$StopPath=Join-Path $RepoRoot '.codex_discord_rust.stop'
$DisablePath=Join-Path $RepoRoot '.codex_discord_bot.disabled'
$RestartPath=Join-Path $RepoRoot '.codex_discord_rust.restart'
$DrainPreparePath=Join-Path $RepoRoot '.codex_discord_rust.drain.prepare'
$DrainAckPath=Join-Path $RepoRoot '.codex_discord_rust.drain.ack'
$MaintenanceStatePath='';$RecoverDeploymentStatePath='';$CompleteRestartFenceJson=''
$PrepareRestart=$false;$CheckRestartReady=$false;$DryRun=$false;$LogHealthy=$false
$ExpectedRuntimeIdentity='';$ExpectedMaintenanceOperation='';$RestartWaitTimeoutSeconds=1
$null=[IO.Directory]::CreateDirectory((Join-Path $RepoRoot 'scripts'))
foreach($module in @('State','Command','Backup','Failure','Actions','Launch','Notification','Schedule','Engine')) {
    $text=if($module -eq 'Engine'){'function Invoke-CdrMaintenanceEngine {param($StatePath,$ExpectedOperation)}'}else{''}
    [IO.File]::WriteAllText((Join-Path $RepoRoot ('scripts\CdrMaintenance'+$module+'.ps1')),$text)
}
$tray=@'
function Invoke-CdrTrayAfterWatchdog {
    param($Root,$StartedIdentity,$ObservedIdentity)
    if(-not [IO.File]::Exists((Join-Path $Root 'guard.released'))){throw 'UI entered while guard owned'}
    [IO.File]::AppendAllText((Join-Path $Root 'entry-ui.log'),"attempt`n")
    if($env:R3_ENTRY_CASE -eq 'ordinary_throw'){throw 'fixture UI error'}
    [pscustomobject]@{State='spawn_requested';Pid=91}
}
'@
[IO.File]::WriteAllText((Join-Path $RepoRoot 'codex-discord-tray-runtime.ps1'),$tray)
function Enter-CdrControl {
    param($Root,[switch]$MaintenanceV2)
    $g=[pscustomobject]@{Root=$Root}
    $g|Add-Member ScriptMethod Dispose {[IO.File]::WriteAllText((Join-Path $this.Root 'guard.released'),'released')}
    return $g
}
function Assert-CdrNoMaintenanceV2 {param($Root)}
function Assert-CdrNoOrphanRestartClaim {param($Root)}
function Read-CdrLaunchJournal {param($Path);if($Case -eq 'pending_restart'){[pscustomobject]@{Fence=@{}}}else{$null}}
function Invoke-CdrRestartTransaction {param($Fence)}
function Invoke-CdrDeploymentRecovery {param($StatePath)}
function Test-CdrRestartCompleted {param($Fence);return $true}
function Get-VerifiedRuntimeProcess {
    if($Case -eq 'failure'){throw 'fixture observation failed'}
    if($Case -eq 'ordinary_start'){return $null}
    return [pscustomobject]@{Id=42}
}
function Get-RustProcessIdentity {param($Process);if($Process){'42|1000'}else{''}}
function Get-VerifiedRuntimeIdentity {'42|1000'}
function Get-HeartbeatHealth {param($Process);[pscustomobject]@{Healthy=$true;Bootstrap=$false;State='healthy'}}
function Enter-RestartDrain {param($Process,$ExpectedProcessIdentity,$TimeoutSeconds);@{}}
function Assert-RestartDrainBound {param($Fence,$ExpectedProcessIdentity)}
function Wait-RustThreadsQuietForRestart {}
function Write-BoundRestartMarker {param($Fence)}
function New-CdrRestartPreparation {param($Process,$Identity);[pscustomobject]@{ArtifactHash='fixture';Fence=@{}}}
function Complete-CdrRestartPreparation {param($Record)}
function Get-CdrRuntimeArtifactPin {param($Path,$ExpectedHash);$pin=[pscustomobject]@{};$pin|Add-Member ScriptMethod Dispose {};return $pin}
function Write-RustWatchdogLog {param($Message)}
function Wait-RustRuntimeExit {param($Process,$ExpectedIdentity,$Reason)}
function Clear-DeadRuntimeArtifacts {}
function Start-RustRuntime {}
function Stop-VerifiedRuntime {throw 'unexpected kill'}
function Start-Process {throw 'unexpected process start'}
switch($Case) {
    'maintenance' {$MaintenanceStatePath='fixture'}
    'recovery' {$RecoverDeploymentStatePath='fixture'}
    'complete' {$CompleteRestartFenceJson='{}'}
    'prepare' {$PrepareRestart=$true}
    'check' {$CheckRestartReady=$true}
    'dry_run' {$DryRun=$true}
    'disabled' {[IO.File]::WriteAllText($DisablePath,'fixture')}
    'stop' {[IO.File]::WriteAllText($StopPath,'fixture')}
}
$env:R3_ENTRY_CASE=$Case
$failed=$false
try {& $boundary} catch {$failed=$true;if($Case -ne 'failure'){throw}}
if(($Case -eq 'failure') -ne $failed){throw 'unexpected boundary error outcome'}
if(-not [IO.File]::Exists((Join-Path $RepoRoot 'guard.released'))){throw 'control guard not released'}
$uiPath=Join-Path $RepoRoot 'entry-ui.log'
$expectedUi=$Case -in @('ordinary','ordinary_start','ordinary_throw')
if([IO.File]::Exists($uiPath) -ne $expectedUi){throw "incorrect UI branch for $Case"}
Write-Output "PASS entry=$Case ui=$expectedUi"
