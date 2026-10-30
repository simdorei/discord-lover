. (Join-Path $PSScriptRoot 'CdrMaintenanceDiagnostics.ps1')
function ConvertTo-CdrUpgradeArgument([string]$Value) {
    if ($Value -match '["\r\n]') { throw 'upgrade_argument_unsupported' }
    $trimmed=$Value.TrimEnd('\')
    return '"'+$trimmed+('\'*(2*($Value.Length-$trimmed.Length)))+'"'
}
function Invoke-CdrUpgradeCommand($State,[string]$Executable,[string[]]$Arguments,[int]$Seconds=30) {
    if ($null -ne $State -and $null -ne $State.Command) { throw 'upgrade_helper_outcome_unknown; no helper replay' }
    $info=[Diagnostics.ProcessStartInfo]::new()
    $info.FileName=$Executable; $info.Arguments=(@($Arguments|ForEach-Object {ConvertTo-CdrUpgradeArgument $_}) -join ' ')
    $info.WorkingDirectory=$RepoRoot; $info.UseShellExecute=$false; $info.CreateNoWindow=$true
    $info.RedirectStandardOutput=$true; $info.RedirectStandardError=$true
    $info.StandardOutputEncoding=[Text.UTF8Encoding]::new($false); $info.StandardErrorEncoding=$info.StandardOutputEncoding
    $process=[Diagnostics.Process]::new(); $process.StartInfo=$info
    if ($null -ne $State) {
        $State.Command=[pscustomobject]@{Phase='launching';Identity='';File=$Executable}
        Save-CdrUpgradeState $State
    }
    try {
        if (-not $process.Start()) { throw 'upgrade_helper_start_unknown' }
        if ($null -ne $State) {
            $State.Command.Phase='child'; $State.Command.Identity=Get-RustProcessIdentity $process
            Save-CdrUpgradeState $State
        }
        $stdout=$process.StandardOutput.ReadToEndAsync(); $stderr=$process.StandardError.ReadToEndAsync()
        $deadline=[DateTimeOffset]::UtcNow.AddSeconds($Seconds)
        while ($true) {
            $exited=$process.WaitForExit(100)
            if ([DateTimeOffset]::UtcNow -ge $deadline) {
                # The initial, read-only metadata probe owns no descendant. Other
                # commands are journaled and retained, never killed/replayed.
                if ($null -eq $State -and -not $exited) { $process.Kill(); [void]$process.WaitForExit(3000) }
                throw 'upgrade_helper_timeout; inspect the recorded helper before recovery'
            }
            if ($exited -and $stdout.IsCompleted -and $stderr.IsCompleted) { break }
        }
        $output=$stdout.GetAwaiter().GetResult(); $errors=$stderr.GetAwaiter().GetResult(); $exitCode=$process.ExitCode
        if ($null -ne $State) { $State.Command=$null; Save-CdrUpgradeState $State }
        if ($exitCode -ne 0) { throw ('upgrade_helper_failed exit='+$exitCode+' '+(Get-CdrMaintenanceDiagnostic $errors $EnvPath)) }
        return $output
    } finally { $process.Dispose() }
}
function Read-CdrUpgradeBinding($State,[string]$Executable,[switch]$Backup) {
    $command=if($Backup){'backup-upgrade'}else{'inspect-upgrade'}
    $output=Invoke-CdrUpgradeCommand $State $Executable @('--admin',$command,'--repo-root',$RepoRoot)
    if ($output.Length -gt 16384) { throw 'upgrade_inspection_too_large' }
    $proof=$output|ConvertFrom-Json
    if ($proof.protocol -cne 'discord-lover-upgrade-v1' -or $proof.compatible -ne $true -or
        $proof.agy_enabled -ne $false -or $proof.admission_authorized -ne $false -or
        $proof.recovery_authorized -ne $false -or $proof.read_only -ne (-not $Backup) -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals(($proof.root -replace '^\\\\\?\\',''),$RepoRoot)) {
        throw 'upgrade_inspection_invalid'
    }
    foreach ($name in @('database','agy_settings')) { Assert-CdrUpgradePath ($proof.$name -replace '^\\\\\?\\','') }
    if ($State) {
        foreach ($name in @('root','database','environment_sha256','effective_environment_sha256','agy_settings','agy_settings_sha256','store_schema')) {
            if ($proof.$name -cne $State.Binding.$name) { throw ('upgrade_binding_changed: '+$name) }
        }
    }
    return $proof
}
function New-CdrUpgradeSnapshot($State,[string]$Name) {
    $entry=@($State.Files|Where-Object {$_.Path -eq 'target/release/cdr-runtime.exe'})[0]
    $proof=Read-CdrUpgradeBinding $State (Get-CdrUpgradeFile $State $entry 'candidate') -Backup
    $path=([string]$proof.backup -replace '^\\\\\?\\',''); Assert-CdrUpgradePath $path
    $directory=Join-Path ([IO.Path]::GetDirectoryName(([string]$State.Binding.database -replace '^\\\\\?\\',''))) '.codex-discord-backups'
    if (-not [StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetDirectoryName($path),$directory)) { throw 'upgrade_snapshot_outside_database_backup_directory' }
    $State.$Name=[pscustomobject]@{Path=$path;Hash=(Get-CdrArtifactHash $path)}; Save-CdrUpgradeState $State
}
