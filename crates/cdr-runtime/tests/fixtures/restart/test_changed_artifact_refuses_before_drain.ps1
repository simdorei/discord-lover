foreach($path in @($RestartPath,$DrainPreparePath,$DrainAckPath)){[IO.File]::Delete($path)}
[IO.File]::WriteAllText($DrainIdentityPath,"version=1`nruntime_id=runtime-a`npid=42`nstate=open`n")
function Get-VerifiedRuntimeProcess {[pscustomobject]@{Id=42;Path=$BinaryPath}}
function Enter-RestartDrain {throw 'DRAIN MUST NOT RUN'}
$ExpectedBinaryHash=Get-CdrArtifactHash $BinaryPath
[IO.File]::WriteAllText($BinaryPath,'replacement differs from original request')
$PrepareRestart=$true;$ExpectedRuntimeIdentity='42|99'
try{&$entry;throw 'changed artifact accepted'}catch{if($_.Exception.Message -notlike '*artifact mismatch*'){throw}}
if([IO.File]::Exists($DrainPreparePath) -or [IO.File]::Exists((Join-Path $root '.codex_discord_rust.restart.launch'))){throw 'changed binary sealed intake'}
if($script:starts -ne 0){throw 'changed binary started'}
