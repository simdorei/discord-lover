Import-Module (Join-Path $PSScriptRoot 'CdrBuildProvenance.psm1')
. (Join-Path $PSScriptRoot 'CdrAsyncRecoveryCompatibility.ps1')

function Get-CdrRuntimeArtifact {
    param([string]$Path, [string]$ExpectedBinaryHash, [string]$ExpectedSourceCommit)
    if ($ExpectedBinaryHash -and $ExpectedBinaryHash -notmatch '^[a-fA-F0-9]{64}$') { throw 'Invalid expected binary hash.' }
    if ($ExpectedSourceCommit -and $ExpectedSourceCommit -cnotmatch '^[a-f0-9]{40}$') { throw 'Expected source commit must be a full lowercase SHA.' }
    $hash=Get-CdrBuildFileHash $Path
    if ($ExpectedBinaryHash -and $hash -cne $ExpectedBinaryHash.ToUpperInvariant()) { throw 'Installed runtime differs from the requested artifact; no restart performed.' }
    $pin=Get-CdrPinnedReadHandle $Path $hash.ToLowerInvariant()
    try {
        $info=$null
        try { $info=Read-CdrBuildInfo $Path }
        catch {
            # Old approved binaries have no build-info command. Do not attribute
            # checkout metadata to them, or hide malformed modern metadata.
            if ($_.Exception.Message -notmatch 'unknown argument: --build-info') { throw }
        }
        if ($ExpectedSourceCommit) {
            if ($null -eq $info -or $info.source_commit -cne $ExpectedSourceCommit -or
                $info.source_state -eq 'unknown') { throw 'Embedded source does not match the requested commit.' }
            if ($info.source_state -eq 'dirty' -and -not $ExpectedBinaryHash) {
                throw 'A dirty build requires an explicit binary hash as well as its source commit.'
            }
        }
        return [pscustomobject]@{ ArtifactHash=$hash; BuildInfo=$info; RequireStartupRecord=($null -ne $info) }
    } finally { $pin.Dispose() }
}

function Get-CdrRuntimeArtifactPin {
    param([string]$Path, [string]$ExpectedHash)
    return Get-CdrPinnedReadHandle $Path $ExpectedHash.ToLowerInvariant()
}

function Read-CdrRuntimeStartup {
    param([string]$Root, [string]$BinaryPath, [string]$ExpectedIdentity)
    $path=Join-Path $Root '.codex_discord_rust.runtime.lock'
    if (-not [IO.File]::Exists($path)) { return $null }
    $text=[IO.File]::ReadAllText($path)
    if ($text.Length -gt 16384) { throw 'Runtime startup record exceeds bound.' }
    $lines=@($text -split "`r?`n" | Where-Object { $_.StartsWith('startup=') })
    if ($lines.Count -eq 0) { return $null }
    if ($lines.Count -ne 1) { throw 'Ambiguous runtime startup record.' }
    $record=$lines[0].Substring(8) | ConvertFrom-Json
    if ($record.schema_version -ne 1 -or $record.process_identity -cne $ExpectedIdentity -or
        -not [StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($record.executable_path),[IO.Path]::GetFullPath($BinaryPath)) -or
        $record.artifact_sha256 -cnotmatch '^[a-f0-9]{64}$') { return $null }
    return $record
}

function Assert-CdrStartedArtifact {
    param([string]$Root, [string]$BinaryPath, [string]$ExpectedIdentity, $Artifact)
    if ($null -eq $Artifact) { return }
    $record=Read-CdrRuntimeStartup $Root $BinaryPath $ExpectedIdentity
    $requires=$Artifact.PSObject.Properties['RequireStartupRecord'] -and $Artifact.RequireStartupRecord
    if ($null -eq $record) {
        if ($requires) { throw 'Replacement has no matching startup artifact record.' }
        return
    }
    if ($record.artifact_sha256 -cne $Artifact.ArtifactHash.ToLowerInvariant()) { throw 'Running child differs from the pinned restart artifact.' }
    if ($Artifact.PSObject.Properties['BuildInfo'] -and $null -ne $Artifact.BuildInfo) {
        foreach($field in @('schema_version','package_version','source_commit','source_tree','source_state')) {
            if ($record.build_info.$field -cne $Artifact.BuildInfo.$field) { throw 'Running child build provenance differs from the requested artifact.' }
        }
    }
}

Export-ModuleMember -Function Get-CdrRuntimeArtifact,Get-CdrRuntimeArtifactPin,Read-CdrRuntimeStartup,Assert-CdrStartedArtifact
