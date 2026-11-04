Import-SourceFunctions 'codex-discord-rust-restart.ps1' | ForEach-Object {Invoke-Expression $_}
$Watchdog=Join-Path $RepoRoot 'watchdog.ps1'
[IO.File]::WriteAllText($Watchdog,@'
param($RepoRoot,$BinaryPath,$RestartQuietSeconds,$RestartWaitTimeoutSeconds,$CompleteRestartFenceJson,$ExpectedBinaryHash,$ExpectedSourceCommit)
if(-not $CompleteRestartFenceJson){throw 'unbound general watchdog used'}
$f=$CompleteRestartFenceJson | ConvertFrom-Json
if($f.ProcessIdentity -ne '42|99' -or $f.Nonce -ne 'owned'){throw 'wrong authorization'}
[IO.File]::WriteAllText((Join-Path $RepoRoot 'completed'),'yes')
exit 0
'@)
$BinaryPath='fixture'; $EffectiveDelaySeconds=0; $EffectiveQuietSeconds=15; $WaitTimeoutSeconds=0
$script:alive=$true
function Get-VerifiedRustIdentity {if($script:alive){'42|99'}else{''}}
function Invoke-RestartReadinessCheck {
 $script:alive=$false
 [pscustomobject]@{RuntimeId='old-runtime';ProcessIdentity='42|99';Nonce='owned'}
}
Invoke-BoundRustRestart -Identity '42|99'
if([IO.File]::ReadAllText((Join-Path $RepoRoot 'completed')) -ne 'yes'){throw 'completion missing'}
