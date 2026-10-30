function Get-CdrInstallArtifactHash {
    param([string]$Path)
    $stream = [IO.File]::OpenRead($Path)
    $sha = [Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-', '') }
    finally { $sha.Dispose(); $stream.Dispose() }
}

function Install-CdrRuntimeArtifact {
    param([string]$RepoRoot, [string]$Source, [switch]$DryRun)
    $ErrorActionPreference = 'Stop'
    $destination = Join-Path $RepoRoot 'target\release\cdr-runtime.exe'
    if ($DryRun) {
        Write-Output 'Would verify the runtime at the launcher path without replacing an installed version.'
        return
    }
    $expected = Get-CdrInstallArtifactHash $Source
    if ([IO.File]::Exists($destination)) {
        if ((Get-CdrInstallArtifactHash $destination) -cne $expected) {
            throw 'Existing runtime differs; use update.ps1 -TargetRoot <existing Rust folder> from a separate Discord Lover checkout for verified deployment. The installed bot was not stopped or changed.'
        }
        return
    }
    $directory = Split-Path -Parent $destination
    $null = New-Item -ItemType Directory -Path $directory -Force
    $staged = Join-Path $directory ("cdr-install-" + [guid]::NewGuid().ToString('N') + '.tmp')
    try {
        [IO.File]::Copy($Source, $staged, $false)
        if ((Get-CdrInstallArtifactHash $staged) -cne $expected) {
            throw 'Runtime artifact changed while staging; installation was not published.'
        }
        # Move refuses an existing destination, including one published by another installer.
        [IO.File]::Move($staged, $destination)
    } finally {
        if ([IO.File]::Exists($staged)) { [IO.File]::Delete($staged) }
    }
}
Export-ModuleMember -Function Install-CdrRuntimeArtifact, Get-CdrInstallArtifactHash
