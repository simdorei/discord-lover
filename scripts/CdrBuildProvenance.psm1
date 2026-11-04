# Build inputs belong to the Cargo child, never the operator's persistent environment.
Import-Module (Join-Path $PSScriptRoot 'CdrNativeProcess.psm1')

function Get-CdrBuildFileHash {
    param([string]$Path)
    $sha=[Security.Cryptography.SHA256]::Create();$stream=[IO.File]::OpenRead($Path)
    try{return [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-','')}
    finally{$stream.Dispose();$sha.Dispose()}
}

function Get-CdrBuildSourceState {
    param([Parameter(Mandatory)][string]$Root)
    $commit = (Invoke-CdrNative git @('-C',$Root,'rev-parse','HEAD')).Trim()
    $tree = (Invoke-CdrNative git @('-C',$Root,'rev-parse','HEAD^{tree}')).Trim()
    if ($commit -cnotmatch '^[0-9a-f]{40}$' -or $tree -cnotmatch '^[0-9a-f]{40}$') { throw 'Invalid source revision.' }
    $status = [string](Invoke-CdrNative git @('-C',$Root,'status','--porcelain','--untracked-files=all'))
    $state = if ([string]::IsNullOrWhiteSpace($status)) { 'clean' } else { 'dirty' }
    # Include actual changes, including untracked source, in the before/after gate.
    $parts = [Collections.Generic.List[string]]::new()
    $parts.Add("${commit}:${tree}:${state}")
    $parts.Add([string](Invoke-CdrNative git @('-C',$Root,'diff','HEAD','--binary','--no-ext-diff')))
    $untracked = [string](Invoke-CdrNative git @('-C',$Root,'ls-files','--others','--exclude-standard','-z'))
    foreach ($relative in @($untracked -split [char]0)) {
        if ($relative.Length) { $parts.Add($relative + ':' + (Get-CdrBuildFileHash (Join-Path $Root $relative))) }
    }
    $hash = [Security.Cryptography.SHA256]::Create()
    try { $fingerprint = [BitConverter]::ToString($hash.ComputeHash([Text.Encoding]::UTF8.GetBytes(($parts -join "`0")))).Replace('-','').ToLowerInvariant() }
    finally { $hash.Dispose() }
    [pscustomobject]@{ Input="${commit}:${tree}:${state}"; Commit=$commit; Tree=$tree; State=$state; Fingerprint=$fingerprint }
}

function Read-CdrBuildInfo {
    param([Parameter(Mandatory)][string]$Path)
    $info = (Invoke-CdrNative -Executable $Path -Arguments @('--build-info') -TimeoutSeconds 10) | ConvertFrom-Json
    if ($info.schema_version -ne 1 -or -not $info.package_version -or $info.source_state -notin @('clean','dirty','unknown')) {
        throw 'Unsupported embedded build information.'
    }
    if ($info.source_state -eq 'unknown') {
        if ($info.source_commit -or $info.source_tree) { throw 'Unknown build contains a source claim.' }
    } elseif ($info.source_commit -cnotmatch '^[0-9a-f]{40}$' -or $info.source_tree -cnotmatch '^[0-9a-f]{40}$') {
        throw 'Invalid embedded source revision.'
    }
    return $info
}

function Assert-CdrBuildSource {
    param([string]$Path, $Source)
    $info = Read-CdrBuildInfo $Path
    if ($info.source_commit -cne $Source.Commit -or $info.source_tree -cne $Source.Tree -or $info.source_state -cne $Source.State) {
        throw 'Binary provenance does not match this source invocation; refusing to relabel the artifact.'
    }
    return $info
}

function Invoke-CdrRuntimeBuild {
    param([string]$Root, [string]$BinaryPath, [string[]]$CargoArguments, [hashtable]$EnvironmentOverrides=@{})
    $before = Get-CdrBuildSourceState $Root
    $child = @{} + $EnvironmentOverrides
    $child['CDR_BUILD_SOURCE_STATE'] = $before.Input
    Push-Location -LiteralPath $Root
    try { Invoke-CdrNative -Executable cargo -Arguments $CargoArguments -TimeoutSeconds 3600 -EnvironmentOverrides $child -WorkingDirectory $Root | Out-Host }
    finally { Pop-Location }
    $after = Get-CdrBuildSourceState $Root
    if ($before.Fingerprint -cne $after.Fingerprint) { throw 'Source changed during compilation; artifact is not verified.' }
    $null = Assert-CdrBuildSource -Path $BinaryPath -Source $before
    return $before
}

Export-ModuleMember -Function Get-CdrBuildSourceState,Read-CdrBuildInfo,Assert-CdrBuildSource,Invoke-CdrRuntimeBuild,Get-CdrBuildFileHash
