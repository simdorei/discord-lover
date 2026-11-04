[CmdletBinding()]
param(
    [string]$CodexExe = $env:CODEX_EXE,
    [string]$CodexHome = $env:CODEX_HOME,
    [ValidateSet('rust')][string]$Runtime = 'rust',
    [string]$BinaryPath,
    [switch]$SkipBuild,
    [switch]$SkipDependencies,
    [switch]$SkipEnvFile,
    [switch]$SkipSteeringConfig,
    [switch]$SkipCodexPlugin,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$ScriptDir = $PSScriptRoot
$CodexExeWasExplicit = $PSBoundParameters.ContainsKey('CodexExe')
$EnvPath = Join-Path $ScriptDir '.env'
$PluginRef = 'codex-discord-remote@codex-discord-remote'
Import-Module (Join-Path $ScriptDir 'scripts\CdrNativeProcess.psm1') -Force
Import-Module (Join-Path $ScriptDir 'scripts\CdrInstallEnvironment.psm1') -Force
Import-Module (Join-Path $ScriptDir 'scripts\CdrInstallPluginHelper.psm1') -Force
Import-Module (Join-Path $ScriptDir 'scripts\CdrInstallRuntime.psm1') -Force
$savedHome = Get-EnvFileValue -EnvPath $EnvPath -Name 'CODEX_HOME'
$homeInput = if (-not $PSBoundParameters.ContainsKey('CodexHome') -and $savedHome) { $savedHome } else { $CodexHome }
$installationHome = Resolve-CodexHomePath -CodexHome $homeInput
$target = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { 'target' }
if (-not [IO.Path]::IsPathRooted($target)) { $target = Join-Path $ScriptDir $target }
$buildOutput = [IO.Path]::GetFullPath((Join-Path $target 'release\cdr-runtime.exe'))
if ([string]::IsNullOrWhiteSpace($BinaryPath)) { $BinaryPath = $buildOutput }
$BinaryPath = [IO.Path]::GetFullPath($BinaryPath)
if (-not $SkipBuild -and $BinaryPath -ine $buildOutput) {
    throw 'BinaryPath differs from the Cargo build output. Set CARGO_TARGET_DIR to that build directory or use -SkipBuild with an already verified artifact.'
}

function Invoke-Admin {
    param([string[]]$Arguments)
    if ($DryRun) {
        Write-Output "Would run Rust management: $($Arguments -join ' ')"
        return
    }
    Invoke-CdrNative -Executable $BinaryPath -Arguments (@('--admin') + $Arguments + @('--repo-root', $ScriptDir))
}

function Resolve-CodexCommand {
    if (-not [string]::IsNullOrWhiteSpace($CodexExe)) { return $CodexExe.Trim().Trim('"').Trim("'") }
    $existing = Get-EnvFileValue -EnvPath $EnvPath -Name 'CODEX_EXE'
    if ($existing) { return $existing }
    $command = Get-Command codex -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    if ($DryRun) { return 'codex' }
    return [string](Invoke-Admin -Arguments @('discover-codex'))
}

function Install-Plugin {
    Install-CdrPluginHelper -RepoRoot $ScriptDir -RuntimeBinary $BinaryPath -SkipBuild:$SkipBuild -DryRun:$DryRun
    $marketplace = Join-Path $ScriptDir '.agents\plugins\marketplace.json'
    $manifest = Join-Path $ScriptDir 'plugins\codex-discord-remote\.codex-plugin\plugin.json'
    if ($DryRun) {
        Write-Output "Would install and verify Codex plugin: $PluginRef"
        return
    }
    foreach ($path in @($marketplace, $manifest)) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "INSTALL_INCOMPLETE: required plugin file was not found: $path" }
    }
    $exe = Resolve-CodexCommand
    $pluginEnvironment = @{ CODEX_HOME = $installationHome }
    $marketplaces = [IO.Path]::GetTempFileName()
    $plugins = [IO.Path]::GetTempFileName()
    try {
        Invoke-CdrNative -Executable $exe -EnvironmentOverrides $pluginEnvironment -Arguments @('plugin', 'marketplace', 'add', $ScriptDir)
        Invoke-CdrNative -Executable $exe -EnvironmentOverrides $pluginEnvironment -Arguments @('plugin', 'add', $PluginRef)
        $marketplaceJson = [string](Invoke-CdrNative -Executable $exe -EnvironmentOverrides $pluginEnvironment -Arguments @('plugin', 'marketplace', 'list', '--json'))
        $pluginJson = [string](Invoke-CdrNative -Executable $exe -EnvironmentOverrides $pluginEnvironment -Arguments @('plugin', 'list', '--json'))
        $utf8 = [Text.UTF8Encoding]::new($false)
        [IO.File]::WriteAllText($marketplaces, $marketplaceJson, $utf8)
        [IO.File]::WriteAllText($plugins, $pluginJson, $utf8)
        Invoke-Admin -Arguments @('verify-plugin-inventory', '--marketplace-inventory', $marketplaces, '--plugin-inventory', $plugins, '--plugin-manifest', $manifest)
    } catch {
        throw "INSTALL_INCOMPLETE: Codex plugin installation or verification failed. Update Codex or pass -CodexExe with a compatible CLI. $($_.Exception.Message)"
    } finally {
        Remove-Item -LiteralPath $marketplaces, $plugins -Force
    }
}

if (-not $SkipBuild) {
    if ($DryRun) {
        Write-Output "Would build the pinned Rust runtime: $BinaryPath"
    } else {
        $canonical = Join-Path $ScriptDir 'target\release\cdr-runtime.exe'
        if ([IO.File]::Exists($canonical)) {
            throw 'An installed runtime exists at the build destination; build externally and use verified deployment.'
        }
        Import-Module (Join-Path $ScriptDir 'scripts/CdrBuildProvenance.psm1') -Force
        $null = Invoke-CdrRuntimeBuild -Root $ScriptDir -BinaryPath $buildOutput `
            -CargoArguments @('build','--release','--locked','-p','cdr-runtime','--bin','cdr-runtime','--target-dir',$target)
    }
}
if (-not $DryRun -and -not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
    throw "Rust runtime was not found: $BinaryPath. Build it before using -SkipBuild."
}
if ($SkipDependencies) { Write-Output 'SkipDependencies is retained for command compatibility; Cargo manages the Rust dependencies.' }
Install-CdrRuntimeArtifact -RepoRoot $ScriptDir -Source $BinaryPath -DryRun:$DryRun

if (-not $SkipEnvFile) {
    if (-not (Test-Path -LiteralPath $EnvPath)) {
        $example = Join-Path $ScriptDir '.env.example'
        if ($DryRun) { Write-Output 'Would create .env from .env.example.' }
        elseif (Test-Path -LiteralPath $example) { Copy-Item -LiteralPath $example -Destination $EnvPath }
        else { throw '.env.example was not found; installation environment was not created.' }
    }
    $configure = @('configure-install', '--codex-home', $installationHome)
    if ($CodexExeWasExplicit) { $configure += @('--codex-exe', $CodexExe.Trim().Trim('"').Trim("'")) }
    Invoke-Admin -Arguments $configure
    if (-not $CodexExeWasExplicit) {
        if ($CodexExe) { Write-Output 'Inherited CODEX_EXE was not saved; existing explicit settings remain unchanged.' }
        else { Write-Output 'PATH-discovered Codex command was not saved; automatic executable discovery remains enabled.' }
    }
}
$discover = @('discover-codex')
if ($CodexExe) { $discover += @('--codex-exe', $CodexExe) }
Invoke-Admin -Arguments $discover
if ($SkipSteeringConfig) { Write-Output 'Installer does not change Codex Desktop follow-up mode.' }
if ($SkipCodexPlugin) { Write-Output 'Skipping Codex plugin install.' } else { Install-Plugin }

if ($DryRun) {
    Write-Output 'Dry run complete. Plugin inventory was not verified.'
} else {
    [IO.File]::WriteAllText((Join-Path $ScriptDir '.codex_discord_runtime'), "rust`n", [Text.UTF8Encoding]::new($false))
    Write-Output 'Install complete. Run .\setup-discord-bot.ps1 to configure the Discord bot.'
    Write-Output 'Restart Codex after plugin installation so its skills reload.'
}
