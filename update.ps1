[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$TargetRoot,
    [string]$CandidatePath,
    [switch]$SkipBuild,
    [switch]$Preview,
    [switch]$Resume,
    [ValidateRange(1,86400)][int]$WaitTimeoutSeconds=900,
    [ValidateRange(15,600)][int]$StartupTimeoutSeconds=120
)
$ErrorActionPreference='Stop'
$UpgradeSourceRoot=[IO.Path]::GetFullPath($PSScriptRoot).TrimEnd('\')
$TargetRoot=[IO.Path]::GetFullPath($TargetRoot).TrimEnd('\')
. (Join-Path $UpgradeSourceRoot 'scripts/CdrUpgradeFiles.ps1')
Assert-CdrUpgradePath $TargetRoot; Assert-CdrUpgradePath $UpgradeSourceRoot
if ($TargetRoot.Equals($UpgradeSourceRoot,[StringComparison]::OrdinalIgnoreCase) -or
    $UpgradeSourceRoot.StartsWith($TargetRoot+'\',[StringComparison]::OrdinalIgnoreCase)) {
    throw 'Run the updater from a separate Discord Lover checkout, outside the operating installation.'
}
if ($Resume -and $Preview) { throw 'Preview and Resume cannot be combined.' }
if (-not $Resume) {
    if (-not $CandidatePath) { $CandidatePath=Join-Path $UpgradeSourceRoot 'target/release/cdr-runtime.exe' }
    $CandidatePath=[IO.Path]::GetFullPath($CandidatePath); Assert-CdrUpgradePath $CandidatePath
    if ($CandidatePath.StartsWith($TargetRoot+'\',[StringComparison]::OrdinalIgnoreCase)) { throw 'Do not build/stage inside the operating installation.' }
    if (-not $SkipBuild -and -not $Preview) {
        if ($PSBoundParameters.ContainsKey('CandidatePath')) { throw 'Use -SkipBuild with an explicitly built CandidatePath.' }
        & cargo build --manifest-path (Join-Path $UpgradeSourceRoot 'Cargo.toml') --locked --release -p cdr-runtime `
            --bin cdr-runtime --target-dir (Join-Path $UpgradeSourceRoot 'target')
        if ($LASTEXITCODE -ne 0) { throw 'Candidate build failed; the installed bot was not changed.' }
    }
    if (-not [IO.File]::Exists($CandidatePath)) { throw 'Build the candidate first, or pass -CandidatePath with -SkipBuild.' }
}
# Load the reviewed functions without entering the watchdog's control/action path.
. (Join-Path $UpgradeSourceRoot 'codex-discord-rust-watchdog.ps1') -RepoRoot $TargetRoot -LibraryOnly
foreach ($module in @('State','Command','Compatibility','Engine')) {
    . (Join-Path $UpgradeSourceRoot ('scripts/CdrUpgrade'+$module+'.ps1'))
}
$controlGuard=$null; $envPin=$null; $candidatePin=$null
try {
    $controlGuard=Enter-CdrControl $RepoRoot -Purpose 'maintenance'
    Assert-CdrUpgradePath $EnvPath
    $envPin=[IO.File]::Open($EnvPath,'Open','Read','Read')
    if ($Resume) {
        $state=Read-CdrUpgradeState
    } else {
        if (Test-Path -LiteralPath (Get-CdrUpgradeStatePath)) { throw 'An upgrade is already pending. Use -Resume with the original updater.' }
        Assert-CdrNoPendingRestart $RepoRoot
        foreach ($path in @($DisablePath,$StopPath)) { if(Test-Path -LiteralPath $path){throw 'Existing stop/disabled intent preserved; upgrade refused.'} }
        $running=Get-VerifiedRuntimeProcess
        if (-not $running) { throw 'The target must be the running Rust installation; no process was started.' }
        if ($null -eq (Read-CdrUpgradeHeartbeat $running)) { throw 'The existing runtime has no verified fresh heartbeat.' }
        $oldIdentity=Get-RustProcessIdentity $running
        $drain=Get-RuntimeDrainIdentity $running $oldIdentity
        $modePath=Join-Path $RepoRoot '.codex_discord_runtime'
        if ([IO.File]::Exists($modePath) -and [IO.File]::ReadAllText($modePath).Trim() -cne 'rust') { throw 'This updater only replaces a Rust installation.' }
        $candidatePin=[IO.File]::Open($CandidatePath,'Open','Read','Read')
        $binding=Read-CdrUpgradeBinding $null $CandidatePath
        $null=Read-CdrUpgradeCompatibility (Get-CdrArtifactHash $BinaryPath) (Get-CdrArtifactHash $EnvPath) $binding
        $targetPins=@(foreach ($name in (Get-CdrUpgradeTargetPaths)) {
            $path=Join-Path $RepoRoot $name; Assert-CdrUpgradePath $path
            [pscustomobject]@{Path=$name;Hash=(Get-CdrArtifactHash $path)}
        })
        if ($Preview) { Write-Output 'upgrade_preview_ok; no backup, drain, stop, replacement or restart performed'; return }
        $same=(Get-CdrArtifactHash $BinaryPath) -ceq (Get-CdrArtifactHash $CandidatePath)
        foreach ($name in @('codex-discord-tray.ps1','codex-discord-tray-agy.ps1')) {
            $path=Join-Path $RepoRoot $name
            $same=$same -and [IO.File]::Exists($path) -and ((Get-CdrArtifactHash $path) -ceq (Get-CdrArtifactHash (Join-Path $UpgradeSourceRoot $name)))
        }
        if ($same) { Write-Output ('upgrade_already_installed sha256='+(Get-CdrArtifactHash $BinaryPath)); return }
        $operation=[guid]::NewGuid().ToString('N')
        $state=[pscustomobject]@{
            Version=1;Operation=$operation;Root=$RepoRoot;Source=$UpgradeSourceRoot;Phase='prepared';LaunchKind='candidate'
            EnvironmentHash=(Get-CdrArtifactHash $EnvPath);ModeHash=$(if([IO.File]::Exists($modePath)){Get-CdrArtifactHash $modePath}else{''})
            Binding=$binding;Files=@();Guarded=$false;TargetPins=$targetPins
            CodePins=@(foreach($name in (Get-CdrUpgradeCodePaths)){[pscustomobject]@{Path=$name;Hash=(Get-CdrArtifactHash (Join-Path $UpgradeSourceRoot $name))}})
            Fence=[pscustomobject]@{RuntimeId=$drain.RuntimeId;ProcessIdentity=$oldIdentity;Nonce=$operation}
            BeforeStopBackup=$null;AfterStopBackup=$null;Command=$null;LastError='';Heartbeats=@();ChildIdentity=''
        }
        [void][IO.Directory]::CreateDirectory((Get-CdrUpgradeBundle $state))
        Add-CdrUpgradePayload $state 'target/release/cdr-runtime.exe' $CandidatePath
        foreach ($name in @('codex-discord-tray.ps1','codex-discord-tray-agy.ps1')) {
            Add-CdrUpgradePayload $state $name (Join-Path $UpgradeSourceRoot $name)
        }
        Copy-CdrUpgradeFile $EnvPath (Join-Path (Get-CdrUpgradeBundle $state) 'previous/.env')
        Add-CdrUpgradeCompatibility $state
        if ((Get-VerifiedRuntimeIdentity) -cne $oldIdentity) { throw 'Runtime changed during preparation; no drain requested.' }
        Save-CdrUpgradeState $state
    }
    Invoke-CdrUpgradeEngine $state
} finally {
    if($candidatePin){$candidatePin.Dispose()}; if($envPin){$envPin.Dispose()}; if($controlGuard){$controlGuard.Dispose()}
}
