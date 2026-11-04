param([string]$Case)
$ErrorActionPreference='Stop'
$env:V2_ROOT=$env:TRAY_CONTRACT_ROOT
$env:V2_SOURCE=$env:TRAY_CONTRACT_SOURCE
. (Join-Path $env:V2_SOURCE 'crates\cdr-runtime\tests\fixtures\maintenance\LOAD.ps1')
[void][IO.Directory]::CreateDirectory((Join-Path $RepoRoot 'scripts'))
foreach ($support in @('scripts/CdrMaintenanceCompatibility.ps1',
    'scripts/CdrAsyncRecoveryCompatibility.ps1','scripts/CdrRuntimeLaunchCompatibility.ps1')) {
    Copy-Item -LiteralPath (Join-Path $env:V2_SOURCE $support) -Destination (Join-Path $RepoRoot $support)
}
. (Join-Path $RepoRoot 'scripts/CdrMaintenanceCompatibility.ps1')
# The fixture owns every path, process and notification provider below.
$state.NotifyChannel='1543277263418826775'
$state.Phase='launch_ready'
$state.Deadline=[DateTimeOffset]::UtcNow.AddSeconds(20).ToString('o')
[IO.File]::WriteAllText($StatePath,($state|ConvertTo-Json -Depth 10))
$StopPath=Join-Path $RepoRoot '.codex_discord_rust.stop'
$DisablePath=Join-Path $RepoRoot '.codex_discord_bot.disabled'
$DrainPreparePath=Join-Path $RepoRoot '.codex_discord_rust.drain.prepare'
$DrainAckPath=Join-Path $RepoRoot '.codex_discord_rust.drain.ack'
$DrainIdentityPath=Join-Path $RepoRoot '.codex_discord_rust.drain.identity'
$EnvPath=Join-Path $RepoRoot 'fixture.env'
$StdoutLog=Join-Path $RepoRoot 'fixture.out';$StderrLog=Join-Path $RepoRoot 'fixture.err'
[IO.File]::WriteAllText($EnvPath,'# isolated fixture only')
[IO.File]::WriteAllText($BinaryPath,'not executable; Start-Process is mocked')
[IO.File]::WriteAllText($DisablePath,$state.Operation)
. (Join-Path $env:V2_SOURCE 'scripts\CdrLaunchJournal.ps1')
. (Join-Path $env:V2_SOURCE 'scripts\CdrMaintenanceLaunch.ps1')
Import-Module (Join-Path $env:V2_SOURCE 'scripts/CdrRuntimeIdentity.psm1')
# Only the executable/child boundaries are simulated in this offline UI test.
function Get-CdrRuntimeArtifact {param($Path,$ExpectedBinaryHash,$ExpectedSourceCommit)
 [pscustomobject]@{ArtifactHash=(Get-CdrArtifactHash $Path);BuildInfo=$null;RequireStartupRecord=$false}
}
$state.CandidateHash=Get-CdrArtifactHash $BinaryPath
[IO.File]::WriteAllText($StatePath,($state|ConvertTo-Json -Depth 10))
$tokens=$null;$errors=$null
$ast=[Management.Automation.Language.Parser]::ParseFile((Join-Path $env:V2_SOURCE 'codex-discord-rust-watchdog.ps1'),[ref]$tokens,[ref]$errors)
if($errors.Count){throw 'watchdog parse failure'}
foreach($name in @('Get-RustProcessIdentity','Start-RustRuntime')) {
    $node=$ast.FindAll({param($n)$n -is [Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -ceq $name},$true)|Select-Object -First 1
    if(-not $node){throw "function missing:$name"}
    . ([scriptblock]::Create($node.Extent.Text))
}
$script:runtimeStarts=0
$fake=[pscustomobject]@{Id=42;Path=$BinaryPath;StartTime=[datetime]'2026-09-01T00:00:00Z';HasExited=$false}
function Get-Process {[CmdletBinding()]param($Id,$Name);if($Id -eq 42){$fake}}
function Start-Process {
    [CmdletBinding()]param($FilePath,$ArgumentList,$WorkingDirectory,$RedirectStandardOutput,$RedirectStandardError,$WindowStyle,[switch]$PassThru)
    if($FilePath -cne $BinaryPath){throw 'UNEXPECTED_NONFIXTURE_START'}
    $script:runtimeStarts++;return $fake
}
function Write-RustWatchdogLog {param($Message)}
function Clear-DeadRuntimeArtifacts {param([switch]$PreserveRestartDrain)}
function Get-VerifiedRuntimeProcess {return $fake}
function Get-VerifiedRuntimeIdentity {Get-RustProcessIdentity $fake}
function Get-CdrMaintenanceChild {param($State,[switch]$RequireFresh);return $fake}
$HeartbeatPath=Join-Path $RepoRoot 'fixture-heartbeat'
$script:heartbeatStamp=1000
function Get-HeartbeatHealth {
    param($Process)
    $script:heartbeatStamp++
    [IO.File]::WriteAllText($HeartbeatPath,"pid=42`nupdated_at=$script:heartbeatStamp`n")
    [pscustomobject]@{Healthy=$true;Bootstrap=$false;State='healthy'}
}
$identity=Get-RustProcessIdentity $fake
# On the old implementation this helper is reached inside the actual launch.
# Its slow variant consumes the remaining deadline; all variants forbid UI work.
$helper=@'
function Start-CdrTrayForRuntime {
    param($Root,$ExpectedRuntimeIdentity)
    [IO.File]::AppendAllText((Join-Path $Root 'forbidden-ui.log'),"called`n")
    if($env:R3_FAULT -eq 'slow') {
        Start-Sleep -Milliseconds 250
        $State.Deadline=[DateTimeOffset]::UtcNow.AddSeconds(-1).ToString('o')
    }
    if($env:R3_FAULT -eq 'throw'){throw 'fixture tray fault'}
    [pscustomobject]@{State='unknown';Pid=$null}
}
'@
[IO.File]::WriteAllText((Join-Path $RepoRoot 'codex-discord-tray-runtime.ps1'),$helper)
$env:R3_FAULT=$Case
$controlGuard=Enter-CdrControl -Root $RepoRoot -MaintenanceV2 -Purpose 'offline-tray-contract'
try {
if($Case -eq 'restart_intent') {
    function Wait-CdrReplacementReady {param($ExpectedIdentity)}
    $script:CdrTrayStartedIdentity=$identity
    Write-CdrRestartCompletion -Fence $state.Fence -ExpectedChildIdentity $identity
    $path=Join-Path $RepoRoot '.codex_discord_rust.restart.completed'
    $record=[IO.File]::ReadAllText($path)|ConvertFrom-Json
    if($record.TrayBootstrapIdentity -cne $identity -or $record.TrayBootstrapRoot -cne $RepoRoot){throw 'restart explicit intent not bound'}
    $script:CdrTrayStartedIdentity=''
    Write-CdrRestartCompletion -Fence $state.Fence -ExpectedChildIdentity $identity
    $record=[IO.File]::ReadAllText($path)|ConvertFrom-Json
    if($record.TrayBootstrapIdentity){throw 'restart reentry invented UI intent'}
} else {
    Invoke-CdrMaintenanceEngine -StatePath $StatePath -ExpectedOperation $state.Operation
    if($script:runtimeStarts -ne 1){throw 'runtime not launched exactly once'}
    if([IO.File]::Exists((Join-Path $RepoRoot 'forbidden-ui.log'))){throw 'maintenance entered UI helper'}
    if([IO.File]::Exists($StatePath) -or [IO.File]::Exists($DisablePath)){throw 'maintenance completion did not release ownership'}
    $receipt=[IO.File]::ReadAllText($StatePath+'.completed')|ConvertFrom-Json
    if($receipt.Phase -cne 'verified' -or $receipt.Halted){throw 'maintenance completion changed by tray'}
    if($receipt.TrayBootstrapIdentity -cne $identity){throw 'missing exact tray completion intent'}
    $receipt.PSObject.Properties.Remove('TrayBootstrapIdentity')
    $script:CdrTrayStartedIdentity=''
    Invoke-CdrMaintenanceLaunch $receipt $StatePath
    if($receipt.TrayBootstrapIdentity -or $script:runtimeStarts -ne 1){throw 'reentry invented intent or relaunched'}
}
} finally { $controlGuard.Dispose() }
Write-Output "PASS maintenance=$Case"
