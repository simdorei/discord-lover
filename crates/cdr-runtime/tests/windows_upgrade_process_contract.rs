#![cfg(windows)]
#[path = "support/upgrade.rs"]
mod support;
use std::{fs, process::Command};
use support::{bounded, repo, success};

#[test]
fn exited_processes_are_skipped_but_unreadable_live_processes_still_hold_upgrade() {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("process-contract.ps1");
    fs::write(
        &script,
        r"
param([string]$SourceRoot)
$ErrorActionPreference='Stop'
. (Join-Path $SourceRoot 'scripts/CdrUpgradeFiles.ps1')
$BinaryPath='C:\isolated\target\release\cdr-runtime.exe'
function Get-RuntimePid { return 0 }
function Get-Process {
    param($Name,$Id,$ErrorAction)
    $fixture=[pscustomobject]@{HasExited=$script:FixtureExited}
    Add-Member -InputObject $fixture -MemberType ScriptProperty -Name Path -Value {
        if ($script:FixtureThrows) { throw 'Fixture process handle is gone' }
        return $null
    }
    return $fixture
}
foreach ($throws in @($false,$true)) {
    $script:FixtureThrows=$throws
    $script:FixtureExited=$true
    Assert-CdrUpgradeNoRuntime
    $script:FixtureExited=$false
    $held=$false
    try { Assert-CdrUpgradeNoRuntime }
    catch {
        if ($_.Exception.Message -cne 'upgrade_process_identity_unreadable') { throw }
        $held=$true
    }
    if (-not $held) { throw 'An unreadable live process was not held' }
}
",
    )
    .unwrap();
    let mut command = Command::new("powershell.exe");
    command
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .arg(repo());
    success(&bounded(command));
}
