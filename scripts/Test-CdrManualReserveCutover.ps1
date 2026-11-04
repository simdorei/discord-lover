[CmdletBinding()]
param([Parameter(Mandatory=$true)][string]$FixtureRoot,[Parameter(Mandatory=$true)][string]$NativeBinary)
$ErrorActionPreference='Stop'
$sourceRoot=Split-Path -Parent $PSScriptRoot
. (Join-Path $sourceRoot 'codex-discord-rust-control.ps1')
. (Join-Path $sourceRoot 'codex-discord-rust-drain.ps1')
. (Join-Path $sourceRoot 'scripts/CdrLaunchJournal.ps1')
. (Join-Path $sourceRoot 'scripts/CdrForceRestart.ps1')
. (Join-Path $sourceRoot 'scripts/CdrManualReserveCutover.ps1')
$tokens=$null;$errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile((Join-Path $sourceRoot 'codex-discord-rust-watchdog.ps1'),[ref]$tokens,[ref]$errors)
foreach ($name in @('Get-RustProcessIdentity','Get-HeartbeatHealth')) {
    $fn=$ast.Find({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq $name},$true)
    Invoke-Expression $fn.Extent.Text
}
function Assert-True($Value,$Message) { if (-not $Value) { throw $Message } }
$entryAst=[Management.Automation.Language.Parser]::ParseFile((Join-Path $sourceRoot 'scripts/Invoke-CdrManualReserveCutover.ps1'),[ref]$tokens,[ref]$errors)
$reader=$entryAst.Find({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Read-CdrCutoverStateFile'},$true)
Invoke-Expression $reader.Extent.Text
$script:CutoverBundle=[IO.Path]::GetFullPath($FixtureRoot)
$RepoRoot=$script:CutoverBundle
$BinaryPath=Join-Path $RepoRoot 'installed.exe';$CandidatePath=Join-Path $RepoRoot 'candidate.exe'
$DisablePath=Join-Path $RepoRoot '.codex_discord_bot.disabled';$StopPath=Join-Path $RepoRoot '.codex_discord_rust.stop'
$HeartbeatPath=Join-Path $RepoRoot '.codex_discord_rust.heartbeat';$StderrLog=Join-Path $RepoRoot 'stderr.log'
$script:CutoverStatePath=Join-Path $RepoRoot 'state.json'
$script:CutoverSeal='fixture-cutover'
foreach ($support in @('codex-discord-rust-watchdog.ps1','codex-discord-rust-control.ps1','codex-discord-rust-drain.ps1',
    'scripts/CdrLaunchJournal.ps1','scripts/CdrForceRestart.ps1','scripts/CdrDeploymentRecovery.ps1','scripts/CdrRestartTransaction.ps1',
    'scripts/CdrMaintenanceCompatibility.ps1','scripts/CdrAsyncRecoveryCompatibility.ps1',
    'scripts/CdrRuntimeLaunchCompatibility.ps1',
    'scripts/CdrRestartPreparation.ps1','scripts/CdrRuntimeIdentity.psm1','scripts/CdrBuildProvenance.psm1','scripts/CdrNativeProcess.psm1',
    'scripts/CdrManualReserveCutover.ps1','scripts/Invoke-CdrManualReserveCutover.ps1')) {
    $destination=Join-Path $RepoRoot $support
    [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($destination))
    [IO.File]::Copy((Join-Path $sourceRoot $support),$destination,$true)
}
[IO.File]::Copy($NativeBinary,$BinaryPath,$true);[IO.File]::Copy($NativeBinary,$CandidatePath,$true)
$stream=[IO.File]::Open($CandidatePath,'Append','Write','None');try{$stream.WriteByte(0)}finally{$stream.Dispose()}
$baseline=Get-CdrArtifactHash $BinaryPath;$candidate=Get-CdrArtifactHash $CandidatePath
$script:CurrentIdentity='fixture-old';$script:LaunchCount=0;$script:Fault=''
$realSave=${function:Save-CdrCutoverPhase};$realStart=${function:Start-CdrCutoverCandidate};$realReady=${function:Wait-CdrCutoverReady}
function Save-CdrCutoverPhase($Phase) {
    & $realSave $Phase
    if ($script:Fault -ceq $Phase) { $script:Fault=''; throw ('injected:'+ $Phase) }
}
function Get-VerifiedRuntimeIdentity { $script:CurrentIdentity }
function Get-VerifiedRuntimeProcess { [Diagnostics.Process]::GetCurrentProcess() }
# Phase engine uses offline launch providers; real process stop and WAL backup are tested below.
function Get-CdrOwnedProcessHandles($Process) { return ,@() }
function Stop-CdrCapturedProcesses($Pins) { $script:CurrentIdentity='' }
function Assert-CdrCutoverStopped { Assert-True ($script:CurrentIdentity -eq '') 'Backup/install began before the old writer stopped' }
function Start-CdrCutoverCandidate {
    Assert-CdrCutoverSeal
    if ($script:CurrentIdentity -eq '') { $script:LaunchCount++;$script:CurrentIdentity='fixture-new' }
    $script:CutoverState.CandidateIdentity=$script:CurrentIdentity
}
function Wait-CdrCutoverReady {
    Assert-CdrCutoverSeal
    Assert-True (-not [IO.File]::Exists($StopPath)) 'Candidate started with a shutdown marker'
    $script:CutoverState.Readiness=@{FirstHeartbeat=1;SecondHeartbeat=2;DiscordReady=$true}
}
function Reset-Fixture {
    foreach ($name in @('previous-runtime.exe','replaced-runtime.exe','launch.json','.codex_discord_bot.disabled','.codex_discord_rust.stop')) {
        $path=Join-Path $RepoRoot $name;if ([IO.File]::Exists($path)) {[IO.File]::Delete($path)}
    }
    [IO.File]::Copy($NativeBinary,$BinaryPath,$true)
    $script:CurrentIdentity='fixture-old';$script:LaunchCount=0
    $script:CutoverState=[pscustomobject]@{Phase='stopping';Operation='fixture';UpdatedAt='';Captured=@();BaselineHash=$baseline;CandidateHash=$candidate;
        TargetIdentity='fixture-old';CandidateIdentity='';DatabaseBackup='';DatabaseBackupHash='';Retirement='';Readiness=$null}
    Write-NewCdrMarker $DisablePath $script:CutoverSeal
}
$realNew=${function:Write-NewCdrMarker}
function Write-NewCdrMarker($Path,$Text) {
    & $realNew $Path $Text
    if ($script:PublishFault) {$script:PublishFault=$false;throw 'injected:seal-published'}
}
Reset-Fixture;$script:CutoverState.Phase='prepared';[IO.File]::Delete($DisablePath);$script:PublishFault=$true
try {Invoke-CdrManualReserveCutover;throw 'Seal publication fault missed'}catch{Assert-True ($_.Exception.Message -eq 'injected:seal-published') $_.Exception.Message}
Assert-True ($script:CutoverState.Phase -eq 'prepared' -and [IO.File]::Exists($DisablePath)) 'Publication crash did not retain an owned seal'
$script:Fault='sealed'
try {Invoke-CdrManualReserveCutover;throw 'Sealed phase fault missed'}catch{Assert-True ($_.Exception.Message -eq 'injected:sealed') $_.Exception.Message}
Assert-True ($script:CurrentIdentity -eq 'fixture-old' -and -not [IO.File]::Exists($StopPath)) 'Seal publication killed a writer before capture'
Write-Output 'PASS crashes around disable publication retain the seal without publishing shutdown'
foreach ($phase in @('stopped','backed_up','installed','migrating','migrated','launch_ready','write_possible','launched','complete')) {
    Reset-Fixture;$script:Fault=$phase
    try { Invoke-CdrManualReserveCutover;throw 'Injected crash was missed' } catch { Assert-True ($_.Exception.Message -ceq ('injected:'+$phase)) $_.Exception.Message }
    Assert-True ([IO.File]::Exists($DisablePath)) 'Crash lost the persistent seal'
    Assert-True (-not [IO.File]::Exists($StopPath)) 'Cutover published a stop marker'
    $script:CutoverState=[IO.File]::ReadAllText($script:CutoverStatePath) | ConvertFrom-Json
    Invoke-CdrManualReserveCutover
    Assert-True ($script:CutoverState.Phase -eq 'complete' -and $script:LaunchCount -eq 1) 'Crash recovery duplicated or lost candidate launch'
    Assert-True (-not [IO.File]::Exists($DisablePath)) 'Verified completion retained its seal'
    [IO.File]::WriteAllText((Join-Path $RepoRoot 'backup-result.txt'),$script:CutoverState.DatabaseBackup)
}
Write-Output 'PASS phase crashes preserve seal and resume without a second launch or database rewind'

Reset-Fixture;$script:CutoverState.Phase='launch_ready';$script:CurrentIdentity=''
[IO.File]::WriteAllText($StopPath,'foreign-user-stop')
try {Invoke-CdrManualReserveCutover;throw 'Foreign stop accepted'}catch{Assert-True ($_.Exception.Message -like '*Foreign stop*') 'Wrong stop refusal'}
Assert-True ([IO.File]::ReadAllText($StopPath) -ceq 'foreign-user-stop') 'Foreign stop changed'
[IO.File]::Delete($StopPath)
$script:CutoverState.Phase='write_possible'
$journal=New-CdrLaunchJournal -Path (Join-Path $RepoRoot 'launch.json') -Operation 'manual-reserve:fixture' -ArtifactHash $candidate
$journal.Phase='launching';Save-CdrLaunchJournal (Join-Path $RepoRoot 'launch.json') $journal
[IO.File]::Copy($CandidatePath,$BinaryPath,$true)
try {& $realStart;throw 'Uncertain launch retried'}catch{Assert-True ($_.Exception.Message -like '*launch_outcome_unknown*') $_.Exception.Message}
Assert-True ($script:LaunchCount -eq 0) 'Unknown launch produced another candidate'
Write-Output 'PASS foreign stop preserved; uncertain child launch is never repeated'

# Exercise the real readiness adapter with two advancing heartbeat files.
$HealthHeartbeatMaxAgeSeconds=45;$HealthHeartbeatStartupGraceSeconds=120
$script:CurrentIdentity=Get-RustProcessIdentity ([Diagnostics.Process]::GetCurrentProcess())
$script:CutoverState.CandidateIdentity=$script:CurrentIdentity
$now=[DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
[IO.File]::WriteAllText($HeartbeatPath,"pid=$PID`nupdated_at=$now`n")
[IO.File]::WriteAllText($StderrLog,'discord_ready user=1 application=2')
$pulseScript=Join-Path $RepoRoot 'pulse.ps1'
$pulseBody="Start-Sleep -Seconds 2`n`$now=[DateTimeOffset]::UtcNow.ToUnixTimeSeconds()`n[IO.File]::WriteAllText('$HeartbeatPath',`"pid=$PID``nupdated_at=`$now``n`")"
[IO.File]::WriteAllText($pulseScript,$pulseBody)
$pulse=Start-Process powershell.exe -ArgumentList @('-NoProfile','-File',('"'+$pulseScript+'"')) -WindowStyle Hidden -PassThru
try { & $realReady } finally { if(-not $pulse.WaitForExit(4000)){$pulse.Kill()};$pulse.Dispose() }
Assert-True ($script:CutoverState.Readiness.SecondHeartbeat -gt $script:CutoverState.Readiness.FirstHeartbeat) 'Readiness accepted a bootstrap or non-advancing heartbeat'
Assert-True ([IO.File]::Exists($DisablePath) -and -not [IO.File]::Exists($StopPath)) 'Readiness changed the maintenance seal'
$watchdog=@(& powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $sourceRoot 'codex-discord-rust-watchdog.ps1') -RepoRoot $RepoRoot -BinaryPath $BinaryPath -DryRun)
Assert-True ($LASTEXITCODE -eq 0 -and $watchdog -contains 'disabled') 'Watchdog did not respect disabled seal'
Write-Output 'PASS two real advancing heartbeats and Discord readiness while disabled; watchdog cannot relaunch'

# Real OS process handles: persist descendants before killing the parent, then recover.
. (Join-Path $sourceRoot 'scripts/CdrForceRestart.ps1')
. (Join-Path $sourceRoot 'scripts/CdrManualReserveCutover.ps1')
$children=[Collections.Generic.List[object]]::new()
try {
    $sleeper=Join-Path $RepoRoot 'sleeper.ps1';[IO.File]::WriteAllText($sleeper,'Start-Sleep -Seconds 120')
    $childPidPath=Join-Path $RepoRoot 'child.pid'
    $parentScript=Join-Path $RepoRoot 'parent.ps1'
    $body="`$p=Start-Process powershell.exe -ArgumentList @('-NoProfile','-File','`"$sleeper`"') -WindowStyle Hidden -PassThru`n[IO.File]::WriteAllText('$childPidPath',[string]`$p.Id)`nStart-Sleep -Seconds 120"
    [IO.File]::WriteAllText($parentScript,$body)
    $parent=Start-Process powershell.exe -ArgumentList @('-NoProfile','-File',('"'+$parentScript+'"')) -WindowStyle Hidden -PassThru;$children.Add($parent)
    $sentinel=Start-Process powershell.exe -ArgumentList @('-NoProfile','-File',('"'+$sleeper+'"')) -WindowStyle Hidden -PassThru;$children.Add($sentinel)
    $until=[DateTimeOffset]::UtcNow.AddSeconds(8)
    while (-not [IO.File]::Exists($childPidPath)) {if ([DateTimeOffset]::UtcNow -gt $until) {throw 'Child timeout'};Start-Sleep -Milliseconds 50}
    $descendant=Get-Process -Id ([int][IO.File]::ReadAllText($childPidPath));$children.Add($descendant)
    $handles=Get-CdrOwnedProcessHandles $parent
    $pins=@($handles | ForEach-Object {@{Identity=(Get-RustProcessIdentity $_);Path=$_.Path}})
    [IO.File]::WriteAllText((Join-Path $RepoRoot 'captured.json'),($pins | ConvertTo-Json))
    foreach ($handle in $handles) {$handle.Dispose()}
    $parent=Get-Process -Id ([int]$pins[0].Identity.Split('|')[0]);$children.Add($parent);$parent.Kill();[void]$parent.WaitForExit(3000)
    $persisted=[IO.File]::ReadAllText((Join-Path $RepoRoot 'captured.json')) | ConvertFrom-Json
    Stop-CdrCapturedProcesses $persisted
    Assert-True ($descendant.WaitForExit(3000)) 'Captured child survived parent-death recovery'
    Assert-True (-not $sentinel.HasExited) 'Unrelated process was stopped'
    $bad=[pscustomobject]@{Identity=([string]$sentinel.Id+'|1');Path=$sentinel.Path}
    try {Stop-CdrCapturedProcesses @($bad);throw 'Stale creation identity accepted'}catch{Assert-True ($_.Exception.Message -like '*reused*') $_.Exception.Message}
    Assert-True (-not $sentinel.HasExited) 'Stale identity killed unrelated process'
    Write-Output 'PASS persisted capture survives parent death; stale identity and unrelated process protected'

    # Run the real independent monitor and entry point. After worker death it
    # reenters once, arms its replacement monitor, then safely refuses a missing
    # original runtime. No fake adapter can manufacture a successful deployment.
    if ([IO.File]::Exists($DisablePath)) {[IO.File]::Delete($DisablePath)}
    $dummy=Start-Process powershell.exe -ArgumentList @('-NoProfile','-File',('"'+$sleeper+'"')) -WindowStyle Hidden -PassThru;$children.Add($dummy)
    $workerIdentity=Get-RustProcessIdentity $dummy
    $operation=[guid]::NewGuid().ToString('N')
    $bundle=Join-Path $RepoRoot ('maintenance_backups\manual-reserve\'+$operation)
    [void][IO.Directory]::CreateDirectory($bundle)
    foreach ($rel in @('target\release\cdr-runtime.exe','target\force-bang\release\cdr-runtime.exe')) {
        $dest=Join-Path $RepoRoot $rel;[void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($dest))
        [IO.File]::Copy($NativeBinary,$dest,$true)
    }
    $pins=@('codex-discord-rust-watchdog.ps1','codex-discord-rust-control.ps1','codex-discord-rust-drain.ps1',
        'scripts/CdrLaunchJournal.ps1','scripts/CdrForceRestart.ps1','scripts/CdrManualReserveCutover.ps1','scripts/Invoke-CdrManualReserveCutover.ps1') |
        ForEach-Object {@{Path=$_;Hash=(Get-CdrArtifactHash (Join-Path $RepoRoot $_))}}
    $nativeHash=Get-CdrArtifactHash $NativeBinary
    $request=@{Version=1;Operation=$operation;RepoRoot=$RepoRoot;BaselineHash=$nativeHash;CandidateHash=$nativeHash;TargetIdentity=$workerIdentity;ProgramPins=@($pins)}
    $requestPath=Join-Path $bundle 'request.json';[IO.File]::WriteAllText($requestPath,($request|ConvertTo-Json -Depth 8))
    $state=@{Version=1;Operation=$operation;RepoRoot=$RepoRoot;BaselineHash=$nativeHash;CandidateHash=$nativeHash;TargetIdentity=$workerIdentity;
        RequestHash=(Get-CdrArtifactHash $requestPath);Phase='prepared';WorkerIdentity=$workerIdentity;RecoveryIdentity='';Recoveries=0;LastError='';UpdatedAt='';
        Captured=@();CandidateIdentity='';DatabaseBackup='';DatabaseBackupHash='';Retirement='';Readiness=$null}
    $statePath=Join-Path $bundle 'state.json';[IO.File]::WriteAllText($statePath,($state|ConvertTo-Json -Depth 8))
    $entry=Join-Path $RepoRoot 'scripts/Invoke-CdrManualReserveCutover.ps1'
    $startup=New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{ShowWindow=[uint16]0}
    $created=Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
        CommandLine=('"'+(Join-Path $PSHOME 'powershell.exe')+'" -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "'+$entry+'" -StatePath "'+$statePath+'" -Monitor');
        CurrentDirectory=$RepoRoot;ProcessStartupInformation=$startup}
    Assert-True ($created.ReturnValue -eq 0) 'Independent monitor launch failed'
    $monitor=Get-Process -Id ([int]$created.ProcessId);$children.Add($monitor)
    $until=[DateTimeOffset]::UtcNow.AddSeconds(20);$ready=Join-Path $bundle 'monitor.ready'
    while (-not [IO.File]::Exists($ready)) {if([DateTimeOffset]::UtcNow -gt $until){throw 'Independent monitor did not arm'};Start-Sleep -Milliseconds 100}
    $dummy.Kill();[void]$dummy.WaitForExit(3000)
    do {
        $resumed=Read-CdrCutoverStateFile $statePath
        if([DateTimeOffset]::UtcNow -gt $until){throw 'Independent monitor did not reenter'}
        Start-Sleep -Milliseconds 100
    } while (-not $resumed.LastError)
    Assert-True ($resumed.Recoveries -eq 1 -and $resumed.LastError -like '*Original runtime identity changed*') ('Recovery failed at an unexpected boundary: '+$resumed.LastError)
    Assert-True ($resumed.RecoveryIdentity -and $resumed.WorkerIdentity -cne $workerIdentity) 'Reentry did not arm independent recovery before destructive work'
    Assert-True (-not [IO.File]::Exists($StopPath)) 'Independent reentry published a stop marker'
    Write-Output 'PASS real independent monitor reenters after worker death and refuses changed ownership'
} finally {
    foreach ($child in $children) {try{if(-not $child.HasExited){$child.Kill();[void]$child.WaitForExit(3000)}}catch{};try{$child.Dispose()}catch{}}
}
& (Join-Path $sourceRoot 'scripts/Test-CdrManualReserveCleanup.ps1') -FixtureRoot (Join-Path $FixtureRoot 'cleanup-monitor')
& (Join-Path $sourceRoot 'scripts/Test-CdrManualReserveMonitor.ps1') -FixtureRoot (Join-Path $FixtureRoot 'monitor-races')
Write-Output 'manual_reserve_cutover_tests_passed'
