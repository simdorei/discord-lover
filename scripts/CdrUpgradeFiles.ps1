# Windows PowerShell may inherit PSModulePath from pwsh. Pin its own ACL module
# instead of auto-loading an incompatible PowerShell 7 assembly.
Import-Module (Join-Path $PSHOME 'Modules/Microsoft.PowerShell.Security/Microsoft.PowerShell.Security.psd1') -ErrorAction Stop
function Assert-CdrUpgradePath([string]$Path) {
    $full=[IO.Path]::GetFullPath($Path)
    if ($full -cnotmatch '^[A-Za-z]:\\' -or $full -match '["\r\n]') { throw 'upgrade_requires_local_absolute_path' }
    $cursor=$full
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item=Get-Item -LiteralPath $cursor -Force
            if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'upgrade_reparse_path_refused' }
        }
        $parent=[IO.Path]::GetDirectoryName($cursor)
        if ($parent -eq $cursor) { break }; $cursor=$parent
    }
}
function Copy-CdrUpgradeFile([string]$From,[string]$To) {
    Assert-CdrUpgradePath $From; Assert-CdrUpgradePath $To
    [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($To))
    [IO.File]::Copy($From,$To,$false)
    # Backups of .env retain the original account access rules.
    Set-Acl -LiteralPath $To -AclObject (Get-Acl -LiteralPath $From)
    if ((Get-CdrArtifactHash $From) -cne (Get-CdrArtifactHash $To)) { throw 'upgrade_copy_hash_mismatch' }
}
function Add-CdrUpgradePayload($State,[string]$Relative,[string]$Source) {
    $destination=Join-Path $RepoRoot $Relative; Assert-CdrUpgradePath $destination
    $entry=[pscustomobject]@{Path=$Relative;NewHash=(Get-CdrArtifactHash $Source);OldHash=''}
    Copy-CdrUpgradeFile $Source (Get-CdrUpgradeFile $State $entry 'candidate')
    if ([IO.File]::Exists($destination)) {
        $entry.OldHash=Get-CdrArtifactHash $destination
        Copy-CdrUpgradeFile $destination (Get-CdrUpgradeFile $State $entry 'previous')
    } elseif (Test-Path -LiteralPath $destination) { throw 'upgrade_destination_not_file' }
    $State.Files+=@($entry)
}
function Install-CdrUpgradeFiles($State,[switch]$Previous) {
    Assert-CdrUpgradeNoRuntime
    Assert-CdrUpgradeStable $State
    foreach ($entry in $State.Files) {
        $destination=Join-Path $RepoRoot $entry.Path
        Assert-CdrUpgradePath $destination
        $current=if([IO.File]::Exists($destination)){Get-CdrArtifactHash $destination}else{''}
        if ($current -cne $entry.OldHash -and $current -cne $entry.NewHash) { throw 'upgrade_destination_changed; foreign edit preserved' }
        $wanted=if($Previous){$entry.OldHash}else{$entry.NewHash}
        if ($current -ceq $wanted) { continue }
        if (-not $wanted) { [IO.File]::Delete($destination); continue }
        $area=if($Previous){'previous'}else{'candidate'}
        $staged=$destination+'.upgrade.'+$State.Operation
        if ([IO.File]::Exists($staged)) {
            if ((Get-CdrArtifactHash $staged) -cne $wanted) { throw 'upgrade_foreign_staging_file' }
        } else { Copy-CdrUpgradeFile (Get-CdrUpgradeFile $State $entry $area) $staged }
        # Preserve target ACL when replacing, including restrictive .env-independent guards.
        if ([IO.File]::Exists($destination)) {
            [IO.File]::Replace($staged,$destination,[System.Management.Automation.Language.NullString]::Value)
        } else { [IO.File]::Move($staged,$destination) }
        if ((Get-CdrArtifactHash $destination) -cne $wanted) { throw 'upgrade_publish_hash_mismatch' }
    }
}
function Assert-CdrUpgradeInstalled($State) {
    foreach ($entry in $State.Files) {
        $wanted=if($State.LaunchKind -eq 'previous'){$entry.OldHash}else{$entry.NewHash}
        $path=Join-Path $RepoRoot $entry.Path; Assert-CdrUpgradePath $path
        $actual=if([IO.File]::Exists($path)){Get-CdrArtifactHash $path}else{''}
        if ($wanted -cne $actual) { throw 'upgrade_installed_artifact_mismatch' }
    }
}
function Remove-CdrUpgradeStaging($State) {
    foreach ($entry in $State.Files) {
        $path=(Join-Path $RepoRoot $entry.Path)+'.upgrade.'+$State.Operation
        Assert-CdrUpgradePath $path
        if ([IO.File]::Exists($path)) {
            $hash=Get-CdrArtifactHash $path
            if ($hash -ceq $entry.OldHash -or $hash -ceq $entry.NewHash) { [IO.File]::Delete($path) }
            else { Write-Warning 'Changed staging file preserved; inspect the upgrade backup directory and receipt.' }
        }
    }
}
function Assert-CdrUpgradeNoRuntime {
    foreach ($process in @(Get-Process -Name cdr-runtime -ErrorAction SilentlyContinue)) {
        try { $path=$process.Path } catch { throw 'upgrade_process_identity_unreadable' }
        if (-not $path) { throw 'upgrade_process_identity_unreadable' }
        if ([StringComparer]::OrdinalIgnoreCase.Equals($path,$BinaryPath)) { throw 'upgrade_runtime_still_running' }
    }
    $lockPid=Get-RuntimePid
    if ($lockPid -gt 0 -and (Get-Process -Id $lockPid -ErrorAction SilentlyContinue)) { throw 'upgrade_live_lock_owner' }
}
function Enter-CdrUpgradeExclusive {
    Assert-CdrUpgradeNoRuntime
    $path=Join-Path $RepoRoot '.codex_discord_rust.exclusive.lock'; Assert-CdrUpgradePath $path
    # Shares no access with the runtime's cross-session filesystem lock.
    return [IO.File]::Open($path,'OpenOrCreate','ReadWrite','None')
}
