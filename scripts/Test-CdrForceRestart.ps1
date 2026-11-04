[CmdletBinding()]
param([Parameter(Mandatory)][string]$RuntimeBinary)
$ErrorActionPreference = 'Stop'
$sourceRoot = Split-Path -Parent $PSScriptRoot
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('cdr-force-test-' + [guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($testRoot)
. (Join-Path $sourceRoot 'codex-discord-rust-control.ps1')
. (Join-Path $sourceRoot 'codex-discord-rust-drain.ps1')
. (Join-Path $sourceRoot 'scripts/CdrLaunchJournal.ps1')
. (Join-Path $sourceRoot 'scripts/CdrForceRestart.ps1')
Import-Module (Join-Path $sourceRoot 'scripts/CdrRuntimeIdentity.psm1') -Force
# Load the real watchdog's pure identity function without executing its entry.
$tokens=$null; $errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile((Join-Path $sourceRoot 'codex-discord-rust-watchdog.ps1'),[ref]$tokens,[ref]$errors)
$identityFunction=$ast.Find({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Get-RustProcessIdentity'},$true)
Invoke-Expression $identityFunction.Extent.Text
function Assert-True { param($Value,[string]$Message); if (-not $Value) { throw $Message } }
function Write-RustWatchdogLog { param($Message) }
function Wait-File { param($Path); $until=[datetime]::UtcNow.AddSeconds(8); while (-not [IO.File]::Exists($Path)) { if ([datetime]::UtcNow -gt $until) { throw 'Fixture start timeout' }; Start-Sleep -Milliseconds 30 } }
function Start-Fixture {
    param([string]$Name,[string]$Body)
    $path=Join-Path $testRoot ($Name+'.ps1')
    [IO.File]::WriteAllText($path,$Body)
    Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',('"'+$path+'"')) -PassThru -WindowStyle Hidden
}
$children=[Collections.Generic.List[object]]::new()
try {
    # A controller really holds the OS control lock while simulating a stuck drain.
    $RepoRoot=Join-Path $testRoot 'controller'; [void][IO.Directory]::CreateDirectory($RepoRoot)
    $ready=Join-Path $RepoRoot 'ready'
    $controlPath=(Join-Path $sourceRoot 'codex-discord-rust-control.ps1').Replace("'","''")
    $body=". '$controlPath'`n`$guard=Enter-CdrControl -Root '$RepoRoot' -Purpose 'watchdog'`n[IO.File]::WriteAllText('$ready','ready')`nStart-Sleep -Seconds 120"
    $waiting=Start-Fixture 'waiting-controller' $body; $children.Add($waiting)
    Wait-File $ready
    $force=Enter-CdrForceGate $RepoRoot
    try {
        $started=[Diagnostics.Stopwatch]::StartNew()
        $guard=Enter-CdrForceControl $RepoRoot
        try {
            Assert-True ($waiting.WaitForExit(1000)) 'Stuck drain controller survived force cancellation'
            Assert-True ($started.Elapsed.TotalSeconds -lt 8) 'Force waited for active work'
            try { $other=Enter-CdrControl $RepoRoot; $other.Dispose(); throw 'Ordinary controller entered during force' }
            catch { Assert-True ($_.Exception.Message -like 'cdr_force_restart_in_progress:*') 'Wrong force exclusion error' }
        } finally { $guard.Dispose() }
    } finally { $force.Dispose() }
    Write-Output 'PASS stuck drain cancelled; competing controller excluded'

    # A deployment owner is distinct from a turn waiting to finish.
    Remove-Item -LiteralPath $ready
    $body=$body.Replace("-Purpose 'watchdog'","-Purpose 'maintenance'")
    $deployment=Start-Fixture 'deployment-controller' $body; $children.Add($deployment)
    Wait-File $ready
    $force=Enter-CdrForceGate $RepoRoot
    try {
        try { $guard=Enter-CdrForceControl $RepoRoot; $guard.Dispose(); throw 'Deployment owner was cancelled' }
        catch { Assert-True ($_.Exception.Message -like '*unverified or deployment owner*') 'Wrong deployment refusal' }
        Assert-True (-not $deployment.HasExited) 'Deployment process was killed'
    } finally { $force.Dispose(); $deployment.Kill(); [void]$deployment.WaitForExit(3000) }
    Write-Output 'PASS deployment owner preserved'

    # Actual PID/creation-bound tree kill, with a separate desktop-like sentinel.
    $sentinel=Start-Fixture 'unrelated' 'Start-Sleep -Seconds 120'; $children.Add($sentinel)
    $childPath=Join-Path $testRoot 'child.ps1'; [IO.File]::WriteAllText($childPath,'Start-Sleep -Seconds 120')
    $childPidPath=Join-Path $testRoot 'child.pid'
    $body="`$child=Start-Process powershell.exe -ArgumentList @('-NoProfile','-File','`"$childPath`"') -WindowStyle Hidden -PassThru`n[IO.File]::WriteAllText('$childPidPath',[string]`$child.Id)`nStart-Sleep -Seconds 120"
    $runtime=Start-Fixture 'runtime' $body; $children.Add($runtime)
    Wait-File $childPidPath
    $descendant=Get-Process -Id ([int][IO.File]::ReadAllText($childPidPath)); $children.Add($descendant)
    $runtimeIdentity=Get-RustProcessIdentity $runtime
    function Get-VerifiedRuntimeIdentity { return $runtimeIdentity }
    try { Stop-CdrOwnedTreeNow $runtime '99999|1'; throw 'Stale identity was accepted' }
    catch { Assert-True ($_.Exception.Message -like '*identity changed*') 'Wrong stale identity refusal' }
    # Stop-CdrOwnedTreeNow disposes pinned handles, so reopen the same live fixture.
    $runtime=Get-Process -Id ([int]$runtimeIdentity.Split('|')[0]); $children.Add($runtime)
    Assert-True (-not $runtime.HasExited) 'Stale request killed the runtime'
    Stop-CdrOwnedTreeNow $runtime $runtimeIdentity
    Assert-True ($descendant.WaitForExit(2000)) 'Owned app-server child survived'
    Assert-True (-not $sentinel.HasExited) 'Unrelated desktop-like process was terminated'
    Write-Output 'PASS stale identity refused; owned tree killed; unrelated process preserved'

    # Exercise the production transaction with offline process/launch providers.
    $RepoRoot=Join-Path $testRoot 'transaction'; [void][IO.Directory]::CreateDirectory($RepoRoot)
    $BinaryPath=Join-Path $RepoRoot 'cdr-runtime.exe'; $EnvPath=Join-Path $RepoRoot '.env'
    Copy-Item -LiteralPath $RuntimeBinary -Destination $BinaryPath
    $DisablePath=Join-Path $RepoRoot '.codex_discord_bot.disabled'
    [IO.File]::WriteAllText($EnvPath,'fixture=1')
    [IO.File]::WriteAllText((Join-Path $RepoRoot '.codex_discord_rust.drain.prepare'),'waiting forever')
    [IO.File]::WriteAllText((Join-Path $RepoRoot 'queue.sqlite'),'preserved queue fixture')
    $script:fake=[pscustomobject]@{Id=42;Path=$BinaryPath;StartTime=[datetime]'2026-09-01T00:00:00Z'}
    $old=Get-RustProcessIdentity $script:fake
    $script:launches=0; $script:stops=0; $script:failLaunch=$true
    $script:probe=$null
    function Get-Process {
        param($Name,$Id,$ErrorAction)
        if ($Name -and $script:probe -and -not $script:probe.HasExited) { return $script:probe }
        if ($Id -and $script:fake -and $script:fake.Id -eq $Id) { return $script:fake }
    }
    function Get-VerifiedRuntimeProcess { return $script:fake }
    function Get-VerifiedRuntimeIdentity { return Get-RustProcessIdentity $script:fake }
    function Stop-CdrOwnedTreeNow { param($Process,$ExpectedIdentity); $script:stops++; $script:fake=$null }
    function Clear-DeadRuntimeArtifacts { }
    function Enter-RestartDrain { throw 'FORBIDDEN_DRAIN_WAIT' }
    function Wait-RustThreadsQuietForRestart { throw 'FORBIDDEN_QUIET_WAIT' }
    function Wait-CdrReplacementReady { param($ExpectedIdentity); Assert-True ((Get-VerifiedRuntimeIdentity) -ceq $ExpectedIdentity) 'Wrong replacement identity' }
    function Start-RustRuntime {
        param([switch]$ResumeRemoteMcp)
        if ($script:failLaunch) { throw 'fixture launch unavailable' }
        $script:launches++
        Set-CdrLaunchStarting
        $script:fake=[pscustomobject]@{Id=43;Path=$BinaryPath;StartTime=[datetime]'2026-09-02T00:00:00Z'}
        Set-CdrLaunchChild $script:fake
    }
    $script:probe=[pscustomobject]@{Id=44;Path=$BinaryPath;StartTime=[datetime]'2026-09-01T01:00:00Z';Handle=1;HasExited=$false}
    $script:probe | Add-Member ScriptMethod Kill { $this.HasExited=$true }
    $script:probe | Add-Member ScriptMethod WaitForExit { param($Timeout); return $this.HasExited }
    $script:probe | Add-Member ScriptMethod Dispose { }
    $script:probeCommand='unknown second runtime'
    function Get-CimInstance {
        param($ClassName,$Filter,$Property)
        [pscustomobject]@{ProcessId=44;CreationDate=$script:probe.StartTime;CommandLine=$script:probeCommand}
    }
    try { Stop-CdrRestartReadinessProbes; throw 'Unknown second runtime accepted' }
    catch { Assert-True ($_.Exception.Message -like '*not a bound restart-readiness probe*') 'Wrong probe identity refusal' }
    Assert-True (-not $script:probe.HasExited) 'Unknown second process was killed'
    $script:probeCommand='cdr-runtime.exe --restart-readiness --env "' + $EnvPath + '"'
    Stop-CdrRestartReadinessProbes -Preview
    Assert-True (-not $script:probe.HasExited) 'Preview killed a readiness probe'
    Stop-CdrRestartReadinessProbes
    Assert-True $script:probe.HasExited 'Graceful readiness probe survived cancellation'
    Write-Output 'PASS readiness probe cancelled without confusing it with another bot'
    Invoke-CdrForceRestart -Preview
    Assert-True ($script:stops -eq 0 -and $script:launches -eq 0) 'Dry run mutated runtime'
    try { Invoke-CdrForceRestart -ExpectedIdentity $old; throw 'Expected launch failure' }
    catch { Assert-True ($_.Exception.Message -eq 'fixture launch unavailable') 'Wrong launch failure' }
    Assert-True ([IO.File]::Exists((Join-Path $RepoRoot '.codex_discord_rust.force.launch'))) 'Recovery journal lost'
    $script:failLaunch=$false
    Invoke-CdrForceRestart
    Assert-True ($script:stops -eq 1 -and $script:launches -eq 1) 'Recovery repeated kill or launch'
    Assert-True (-not [IO.File]::Exists((Join-Path $RepoRoot '.codex_discord_rust.drain.prepare'))) 'Stale drain blocked replacement'
    Assert-True ([IO.File]::ReadAllText((Join-Path $RepoRoot 'queue.sqlite')) -eq 'preserved queue fixture') 'Queue was changed'
    $receipt=[IO.File]::ReadAllText((Join-Path $RepoRoot '.codex_discord_rust.force.completed')) | ConvertFrom-Json
    Assert-True ($receipt.InterruptedIdentity -ceq $old -and -not $receipt.WaitedForActiveWork) 'Incorrect interruption receipt'
    Assert-True ([IO.File]::Exists((Join-Path $receipt.BackupRoot '.codex_discord_rust.drain.prepare'))) 'Superseded marker backup missing'
    Write-Output 'PASS busy restart bypass; dry run; failed launch recovery; queue and markers preserved'

    # Public CLI routing must bypass the existing minimum delay and preflight.
    $entryRoot=Join-Path $testRoot 'entry'; [void][IO.Directory]::CreateDirectory($entryRoot)
    [void][IO.Directory]::CreateDirectory((Join-Path $entryRoot 'scripts'))
    Copy-Item -LiteralPath (Join-Path $sourceRoot 'scripts/CdrForceRestart.ps1') -Destination (Join-Path $entryRoot 'scripts')
    foreach($file in @('CdrRuntimeIdentity.psm1','CdrBuildProvenance.psm1','CdrNativeProcess.psm1','CdrAsyncRecoveryCompatibility.ps1')) {
        Copy-Item -LiteralPath (Join-Path $sourceRoot ('scripts/'+$file)) -Destination (Join-Path $entryRoot 'scripts')
    }
    [void][IO.Directory]::CreateDirectory((Join-Path $entryRoot 'target/release'))
    Copy-Item -LiteralPath $RuntimeBinary -Destination (Join-Path $entryRoot 'target/release/cdr-runtime.exe')
    $entryPath=Join-Path $sourceRoot 'codex-discord-rust-restart.ps1'
    $entryText=[IO.File]::ReadAllText($entryPath)
    $entryAst=[Management.Automation.Language.Parser]::ParseFile($entryPath,[ref]$tokens,[ref]$errors)
    $provider=$entryAst.Find({param($n) $n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Get-VerifiedRustIdentity'},$true)
    $entryText=$entryText.Replace($provider.Extent.Text,"function Get-VerifiedRustIdentity { return '42|99' }")
    [IO.File]::WriteAllText((Join-Path $entryRoot 'codex-discord-rust-restart.ps1'),$entryText)
    $watchdogStub=@'
param($RepoRoot,$BinaryPath,[switch]$ForceRestart,$ExpectedRuntimeIdentity,$ExpectedBinaryHash,$ExpectedSourceCommit,[switch]$DryRun)
if (-not $ForceRestart -or $ExpectedRuntimeIdentity -ne '42|99') { throw 'Force route lost identity or mode' }
Import-Module (Join-Path $RepoRoot 'scripts/CdrRuntimeIdentity.psm1')
$null=Get-CdrRuntimeArtifact $BinaryPath $ExpectedBinaryHash
Write-Output 'force_entry_verified'
exit 0
'@
    [IO.File]::WriteAllText((Join-Path $entryRoot 'codex-discord-rust-watchdog.ps1'),$watchdogStub)
    $result=& powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $entryRoot 'codex-discord-rust-restart.ps1') -Force -ExpectedBotIdentity '42|99' -DelaySeconds 999 -QuietSeconds 999
    Assert-True ($LASTEXITCODE -eq 0 -and $result -contains 'force_entry_verified') 'Public force entry went through graceful path'

    $ScriptDir=$entryRoot; $RuntimeMode='rust'
    . (Join-Path $sourceRoot 'codex-discord-tray-restart-runtime.ps1')
    function Get-RustTrayProcess { return 'fixture' }
    function Get-RustTrayProcessIdentity { param($Process); return '42|99' }
    function Write-LauncherLog { param($Message) }
    function Start-Process {
        param($FilePath,$ArgumentList,$WindowStyle,[switch]$PassThru)
        Assert-True ($ArgumentList -contains '-Force') 'Tray did not request force mode'
        Assert-True ($ArgumentList -contains '42|99') 'Tray lost process binding'
        Assert-True ($WindowStyle -eq 'Hidden') 'Tray helper was not hidden'
        Assert-True (-not ($ArgumentList -contains '-Deferred')) 'Tray queued a graceful wait'
        return [pscustomobject]@{Id=77}
    }
    Request-BotForceRestart
    Write-Output 'PASS public force command and detached tray route'

    $workerReceipt=Join-Path $entryRoot 'independent-worker.json'
    $workerStub=@'
param($RepoRoot,[switch]$Force,[switch]$ForceWorker,$ExpectedBotIdentity,$ExpectedBinaryHash,$ExpectedSourceCommit)
$me=Get-CimInstance Win32_Process -Filter "ProcessId = $PID"
$record=@{Force=[bool]$Force;Worker=[bool]$ForceWorker;Identity=$ExpectedBotIdentity;Hash=$ExpectedBinaryHash;Commit=$ExpectedSourceCommit;Pid=$PID;Parent=$me.ParentProcessId}
[IO.File]::WriteAllText((Join-Path $RepoRoot 'independent-worker.json'),($record | ConvertTo-Json -Compress))
'@
    [IO.File]::WriteAllText((Join-Path $entryRoot 'codex-discord-rust-restart.ps1'),$workerStub)
    $worker=Start-CdrDetachedForceRestart -Root $entryRoot -Identity '42|99' -ExpectedBinaryHash ('A'*64) -ExpectedSourceCommit ('b'*40)
    Wait-File $workerReceipt
    $received=[IO.File]::ReadAllText($workerReceipt) | ConvertFrom-Json
    Assert-True ($received.Force -and $received.Worker -and $received.Identity -ceq '42|99') 'Independent worker lost force parameters'
    Assert-True ($received.Hash -ceq ('A'*64) -and $received.Commit -ceq ('b'*40)) 'Independent worker lost artifact pins'
    Assert-True ($received.Pid -eq $worker -and $received.Parent -ne $PID) 'Worker did not detach from requesting process'
    Write-Output 'PASS independent OS worker receives force request outside caller ancestry'
    Write-Output 'force_restart_tests_passed'
} finally {
    # Exact handles from this fixture only; never enumerate or stop live bot PIDs.
    foreach ($child in $children) {
        try { if (-not $child.HasExited) { $child.Kill(); [void]$child.WaitForExit(3000) } } catch { }
        try { $child.Dispose() } catch { }
    }
    $absolute=[IO.Path]::GetFullPath($testRoot)
    $tempParent=[IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
    if ([IO.Path]::GetDirectoryName($absolute) -ieq $tempParent -and [IO.Path]::GetFileName($absolute) -like 'cdr-force-test-*') {
        Remove-Item -LiteralPath $absolute -Recurse -Force
    }
}
