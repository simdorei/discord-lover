#![cfg(windows)]
use std::{path::Path, process::Command};

fn run(body: &str) {
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../codex-discord-tray-agy.ps1");
    let script = format!(
        r"
$ErrorActionPreference='Stop'
. $env:AGY_TRAY_HELPER
function Assert($Condition,$Message) {{if (!$Condition) {{throw $Message}}}}
$script:AgyStatusItem=[pscustomobject]@{{Text='';ToolTipText=''}}
$script:AgyItems=@{{}}
foreach($name in @('on','off','stop','login','configure','recheck')) {{$script:AgyItems[$name]=[pscustomobject]@{{Enabled=$false}}}}
$script:AgyPending=$null;$script:AgyJob=$null;$script:AgyError='';$script:AgyEditing=$false
$script:AgyNextRead=[datetime]::UtcNow.AddHours(1)
function Reply($Runtime,$Sequence,$Revision,$Phase) {{[pscustomobject]@{{status=[pscustomobject]@{{runtime_uuid=$Runtime;sequence=$Sequence;config_revision=$Revision;phase=$Phase;detail='fixture'}};outcome='status';error=$null}}}}
{body}
"
    );
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .env("AGY_TRAY_HELPER", helper)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn tray_rejects_stale_and_reordered_status_and_never_replays_a_mutation() {
    run(r"
function Start-CdrAgyCall {param($Operation);$script:Recorded=$Operation;return $true}
function Deliver($Reply,$Operation) {
 $pipe=[pscustomobject]@{Reply=($Reply|ConvertTo-Json -Depth 10 -Compress);HadErrors=$false;Streams=[pscustomobject]@{Error=@()}}
 $pipe|Add-Member ScriptMethod EndInvoke {param($Async);$this.Reply}
 $pipe|Add-Member ScriptMethod Dispose {}
 $script:AgyJob=[pscustomobject]@{Pipeline=$pipe;Async=[pscustomobject]@{IsCompleted=$true};Operation=@{operation=$Operation}}
 Update-CdrAgyBackground
}
$script:AgyCache=Reply 'runtime-b' 20 3 'Off';$script:AgyCacheAt=[datetime]::UtcNow
Assert ((Get-CdrAgySettingsRevision ([pscustomobject]@{revision=2})) -eq 2) 'Editor used newer status revision with older settings'
Assert ((Get-CdrAgySettingsRevision $null) -eq 0) 'Missing settings must not borrow status revision'
Deliver (Reply 'runtime-b' 19 2 'Ready') 'status'
Assert ($script:AgyCache.status.phase -eq 'Off') 'Old Ready replaced newer Off'
Assert $script:AgyItems.on.Enabled 'Fresh Off should permit Enable'
$script:AgyCacheAt=[datetime]::UtcNow.AddSeconds(-13);Update-CdrAgyMenu
Assert ($script:AgyStatusItem.Text -eq 'AGY: Unavailable') 'Stale state not exposed'
Assert (@($script:AgyItems.Values|Where-Object Enabled).Count -eq 0) 'Stale menu accepted a command'
$script:AgyCacheAt=[datetime]::UtcNow;$script:AgyPending='runtime-a/1/request'
Deliver (Reply 'runtime-a' 99 4 'Ready') 'result'
Assert ($null -eq $script:AgyPending) 'Previous mutation survived runtime change'
Assert ($script:AgyCache.status.runtime_uuid -eq 'runtime-b') 'Late old runtime response was accepted'
Assert ($script:Recorded.operation -eq 'status') 'Mutation was replayed instead of reading status'
Deliver (Reply 'runtime-c' 1 4 'Off') 'status'
Assert ($script:AgyCache.status.runtime_uuid -eq 'runtime-c') 'Fresh runtime status was not adopted'
$script:AgyPending='pending';$script:Recorded=$null;Invoke-CdrAgyMenu 'on'
Assert ($null -eq $script:Recorded) 'Pending mutation was duplicated'
$script:AgyEditing=$true;$script:AgyNextRead=[datetime]::MinValue;Update-CdrAgyBackground
Assert ($null -eq $script:Recorded) 'Settings editor allowed a concurrent refresh'
");
}

#[test]
fn fresh_menu_commands_and_invalid_settings_do_not_leave_the_editor_stuck() {
    run(r"
$script:AgyCache=Reply 'runtime' 1 3 'Off';$script:AgyCacheAt=[datetime]::UtcNow
function Send-CdrAgyMutation {param($Command);$script:Recorded=$Command}
foreach ($name in @('on','off','stop','login','recheck')) {
 $script:Recorded=$null;Invoke-CdrAgyMenu $name
 Assert ($null -ne $script:Recorded) ('Menu did not dispatch: '+$name)
 Assert ($script:Recorded.expected_revision -eq 3) ('Menu revision mismatch: '+$name)
 $expected=switch($name){'stop' {'off'};default {$name}}
 Assert ($script:Recorded.command -eq $expected) ('Menu command mismatch: '+$name)
 if($name -eq 'stop'){Assert $script:Recorded.immediate 'Stop must be immediate'}
 if($name -eq 'off'){Assert (!$script:Recorded.immediate) 'Off must drain'}
}
$script:Allocated=$false
function New-Object {param($TypeName);$script:Allocated=$true;return [pscustomobject]@{}}
$invalid=$false
try {Show-CdrAgySettings ([pscustomobject]@{})}catch {$invalid=$true}
Assert $invalid 'Missing revision must reject editing'
Assert (!$script:AgyEditing -and !$script:Allocated) 'Invalid settings leaked editing state or form'
function Start-CdrAgyCall {param($Operation);$script:Recorded=$Operation;return $true}
$script:Recorded=$null;$script:AgyNextRead=[datetime]::MinValue
Update-CdrAgyBackground
Assert ($script:Recorded.operation -eq 'status') 'Invalid settings blocked later status refresh'
");
}

#[test]
fn tray_control_failure_stays_asynchronous_without_starting_or_restarting_a_bot() {
    run(r"
$script:AgyRoot=Join-Path ([IO.Path]::GetTempPath()) ([guid]::NewGuid().ToString())
$script:AgyPool=[RunspaceFactory]::CreateRunspacePool(1,1);$script:AgyPool.Open()
$script:AgyCache=Reply 'runtime' 1 1 'Ready';$script:AgyCacheAt=[datetime]::UtcNow
try {
 $clock=[Diagnostics.Stopwatch]::StartNew()
 Assert (Start-CdrAgyCall @{operation='status'}) 'Call was not scheduled'
 Assert ($clock.ElapsedMilliseconds -lt 500) 'UI waited for control process'
 $deadline=[datetime]::UtcNow.AddSeconds(5)
 while (!$script:AgyJob.Async.IsCompleted -and [datetime]::UtcNow -lt $deadline) {Start-Sleep -Milliseconds 10}
 Assert $script:AgyJob.Async.IsCompleted 'Background failure did not finish'
 Update-CdrAgyBackground
 Assert ($script:AgyStatusItem.Text -eq 'AGY: Unavailable') 'Failure retained old Ready'
 Assert ($null -eq $script:AgyJob) 'Failed helper was not disposed'
} finally {$script:AgyPool.Close();$script:AgyPool.Dispose()}
");
}
