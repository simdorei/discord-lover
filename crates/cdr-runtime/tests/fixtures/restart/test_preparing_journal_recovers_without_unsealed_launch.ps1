$realAtomic=(Get-Command Write-AtomicRestartMarker).ScriptBlock
function Get-VerifiedRuntimeProcess {
 if($script:oldAlive){[pscustomobject]@{Id=42;Path=$BinaryPath}}
 elseif($script:newAlive){[pscustomobject]@{Id=77;Path=$BinaryPath}}
}
function Wait-RustRuntimeExit {param($Process,$ExpectedIdentity,$Reason)
 if($ExpectedIdentity -cne '42|99'){throw 'wrong original process'};$script:oldAlive=$false
}
function Wait-RustThreadsQuietForRestart {}
function Write-AtomicRestartMarker {param($Path,$Text)
 if($Path.EndsWith('.restart.launch')){
  $body=$Text|ConvertFrom-Json
  if($script:inject -eq 'after_marker' -and $body.Phase -eq 'prepared'){
   $script:inject='';$script:oldAlive=$false;throw 'injected after marker'
  }
 }
 & $realAtomic $Path $Text
 if($Path -ceq $DrainPreparePath){&$realAtomic $DrainAckPath ($Text+"state=sealed`n")}
 if($Path.EndsWith('.restart.launch') -and $script:inject -eq 'before_drain'){
  $script:inject='';throw 'injected before drain'
 }
}
foreach($fault in @('before_drain','after_marker')){
 foreach($name in @('restart','drain.prepare','drain.ack','restart.launch','restart.completed')){
  $path=Join-Path $root ('.codex_discord_rust.'+$name);if([IO.File]::Exists($path)){[IO.File]::Delete($path)}
 }
 [IO.File]::WriteAllText($DrainIdentityPath,"version=1`nruntime_id=runtime-a`npid=42`nstate=open`n")
 $script:oldAlive=$true;$script:newAlive=$false;$script:starts=0;$script:inject=$fault
 $PrepareRestart=$true;$ExpectedRuntimeIdentity='42|99'
 try{&$entry;throw 'injected failure hidden'}catch{if($_.Exception.Message -notlike 'injected *'){throw}}
 $journal=Read-CdrLaunchJournal (Join-Path $root '.codex_discord_rust.restart.launch')
 if($journal.Phase -ne 'preparing' -or $journal.ArtifactHash -cne (Get-CdrArtifactHash $BinaryPath)){throw 'pre-drain artifact binding lost'}
 if($script:starts -ne 0){throw 'launched before preparation completed'}
 if($fault -eq 'before_drain' -and [IO.File]::Exists($DrainPreparePath)){throw 'intake sealed before journal persisted'}
 $PrepareRestart=$false
 &$entry
 if($fault -eq 'before_drain'){
  if($script:starts -ne 0){throw 'preparing recovery launched before published handoff'}
  $PrepareRestart=$false;&$entry
 }
 if($script:starts -ne 1){throw 'preparation recovery did not launch exactly once'}
 if([IO.File]::Exists((Join-Path $root '.codex_discord_rust.restart.launch'))){throw 'completed journal leaked'}
}
