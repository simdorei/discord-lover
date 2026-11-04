function Read-RestartDrainFields {
    param([string]$Path, [string[]]$Required)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    $text = Get-Content -LiteralPath $Path -Raw -ErrorAction Stop
    if ($text.Length -gt 4096) { throw "Restart drain marker is too large: $Path" }
    $fields = @{}
    foreach ($line in ($text -split "`r?`n")) {
        if ([string]::IsNullOrEmpty($line)) { continue }
        $parts = $line -split '=', 2
        if ($parts.Count -ne 2 -or [string]::IsNullOrEmpty($parts[0]) -or
            [string]::IsNullOrEmpty($parts[1]) -or $fields.ContainsKey($parts[0])) {
            throw "Restart drain marker is malformed: $Path"
        }
        $fields[$parts[0]] = $parts[1]
    }
    foreach ($name in $Required) {
        if (-not $fields.ContainsKey($name)) {
            throw "Restart drain marker is missing $name`: $Path"
        }
    }
    if ($fields['version'] -ne '1') {
        throw "Restart drain marker version is unsupported: $Path"
    }
    return $fields
}

function Get-RuntimeDrainIdentity {
    param($Process, [string]$ExpectedProcessIdentity)
    $fields = Read-RestartDrainFields -Path $DrainIdentityPath `
        -Required @('version', 'runtime_id', 'pid', 'state')
    if ($null -eq $fields -or $fields['state'] -ne 'open') {
        throw 'Rust runtime did not publish an open restart drain identity.'
    }
    if ([int]$fields['pid'] -ne [int]$Process.Id -or
        (Get-RustProcessIdentity -Process $Process) -ne $ExpectedProcessIdentity) {
        throw 'Rust restart drain identity does not match the verified process.'
    }
    return [pscustomobject]@{
        RuntimeId = [string]$fields['runtime_id']
        ProcessIdentity = $ExpectedProcessIdentity
    }
}

function Get-RestartDrainFence {
    param([string]$Path, [switch]$RequireSealed)
    $required = @('version', 'runtime_id', 'process_identity', 'nonce')
    if ($RequireSealed) { $required += 'state' }
    $fields = Read-RestartDrainFields -Path $Path -Required $required
    if ($null -eq $fields) { return $null }
    if ($RequireSealed -and $fields['state'] -ne 'sealed') {
        throw "Restart drain acknowledgement is not sealed: $Path"
    }
    if ($fields['runtime_id'] -notmatch '^[A-Za-z0-9_-]{1,128}$' -or
        $fields['process_identity'] -notmatch '^\d+\|\d+$' -or
        $fields['nonce'] -notmatch '^[A-Za-z0-9_-]{1,128}$') {
        throw "Restart drain fence identity is malformed: $Path"
    }
    return [pscustomobject]@{
        RuntimeId = [string]$fields['runtime_id']
        ProcessIdentity = [string]$fields['process_identity']
        Nonce = [string]$fields['nonce']
    }
}

function Test-RestartDrainFenceMatch {
    param($Left, $Right)
    return $null -ne $Left -and $null -ne $Right -and
        $Left.RuntimeId -ceq $Right.RuntimeId -and
        $Left.ProcessIdentity -ceq $Right.ProcessIdentity -and
        $Left.Nonce -ceq $Right.Nonce
}

function Write-AtomicRestartMarker {
    param([string]$Path, [string]$Text)
    $temporary = Join-Path ([IO.Path]::GetDirectoryName($Path)) (
        '.' + [IO.Path]::GetFileName($Path) + '.tmp.' + [guid]::NewGuid().ToString('N')
    )
    $utf8NoBom = [Text.UTF8Encoding]::new($false)
    try {
        [IO.File]::WriteAllText($temporary, $Text, $utf8NoBom)
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            [IO.File]::Replace($temporary, $Path, [System.Management.Automation.Language.NullString]::Value)
        } else {
            [IO.File]::Move($temporary, $Path)
        }
    } finally {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
    }
}

function Enter-RestartDrain {
    param($Process, [string]$ExpectedProcessIdentity, [int]$TimeoutSeconds, $PreparedFence=$null)
    $identity = Get-RuntimeDrainIdentity -Process $Process `
        -ExpectedProcessIdentity $ExpectedProcessIdentity
    $fence = Get-RestartDrainFence -Path $DrainPreparePath
    if ($null -eq $fence) {
        $fence = if ($null -ne $PreparedFence) { $PreparedFence } else { [pscustomobject]@{
            RuntimeId = $identity.RuntimeId
            ProcessIdentity = $ExpectedProcessIdentity
            Nonce = [guid]::NewGuid().ToString('N')
        } }
        if ($fence.RuntimeId -cne $identity.RuntimeId -or $fence.ProcessIdentity -cne $ExpectedProcessIdentity) {
            throw 'Prepared restart journal differs from the running instance.'
        }
        Write-AtomicRestartMarker -Path $DrainPreparePath -Text (
            "version=1`nruntime_id=$($fence.RuntimeId)`n" +
            "process_identity=$($fence.ProcessIdentity)`nnonce=$($fence.Nonce)`n"
        )
    } elseif ($fence.RuntimeId -cne $identity.RuntimeId -or
        $fence.ProcessIdentity -cne $ExpectedProcessIdentity) {
        throw 'Existing restart drain targets a different runtime identity.'
    }
    if ($null -ne $PreparedFence -and -not (Test-RestartDrainFenceMatch $fence $PreparedFence)) {
        throw 'Drain prepare differs from the pinned restart journal.'
    }

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    do {
        if ((Get-VerifiedRuntimeIdentity) -ne $ExpectedProcessIdentity) {
            throw 'Rust process changed while waiting for restart drain acknowledgement.'
        }
        $currentIdentity = Get-RuntimeDrainIdentity -Process (Get-VerifiedRuntimeProcess) `
            -ExpectedProcessIdentity $ExpectedProcessIdentity
        if ($currentIdentity.RuntimeId -cne $fence.RuntimeId) {
            throw 'Rust runtime identity changed while waiting for restart drain acknowledgement.'
        }
        $ack = Get-RestartDrainFence -Path $DrainAckPath -RequireSealed
        if (Test-RestartDrainFenceMatch -Left $ack -Right $fence) { return $fence }
        if ($null -ne $ack) {
            throw 'Restart drain acknowledgement does not match the active prepare nonce.'
        }
        Start-Sleep -Milliseconds 100
    } while ((Get-Date) -lt $deadline)
    throw (
        "restart_drain_ack_timeout identity=$ExpectedProcessIdentity " +
        "timeout_seconds=$TimeoutSeconds runtime_remains_sealed=true"
    )
}

function Assert-RestartDrainBound {
    param($Fence, [string]$ExpectedProcessIdentity)
    if ((Get-VerifiedRuntimeIdentity) -ne $ExpectedProcessIdentity) {
        throw 'Rust process changed after restart drain acknowledgement.'
    }
    $prepare = Get-RestartDrainFence -Path $DrainPreparePath
    $ack = Get-RestartDrainFence -Path $DrainAckPath -RequireSealed
    if (-not (Test-RestartDrainFenceMatch $prepare $Fence) -or
        -not (Test-RestartDrainFenceMatch $ack $Fence)) {
        throw 'Restart drain fence changed after acknowledgement.'
    }
}

function Write-BoundRestartMarker {
    param($Fence)
    Write-NewCdrMarker -Path $RestartPath -Text (
        "version=1`nruntime_id=$($Fence.RuntimeId)`n" +
        "process_identity=$($Fence.ProcessIdentity)`nnonce=$($Fence.Nonce)`n"
    )
}

function Clear-CompletedRestartDrain {
    param($Fence)
    $prepare = Get-RestartDrainFence -Path $DrainPreparePath
    $ack = Get-RestartDrainFence -Path $DrainAckPath -RequireSealed
    if (-not (Test-RestartDrainFenceMatch $prepare $Fence) -or
        -not (Test-RestartDrainFenceMatch $ack $Fence)) {
        throw 'Completed restart drain state no longer matches the claimed restart.'
    }
    Remove-Item -LiteralPath $DrainPreparePath -Force
    Remove-Item -LiteralPath $DrainAckPath -Force
    Remove-Item -LiteralPath $DrainIdentityPath -Force -ErrorAction SilentlyContinue
}

function Clear-OrphanedRestartDrainArtifacts {
    if ($null -ne (Get-VerifiedRuntimeProcess)) {
        throw 'Refusing to clear restart drain state while a verified runtime is alive.'
    }
    foreach ($path in @($DrainPreparePath, $DrainAckPath, $DrainIdentityPath)) {
        Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    }
}

function Restore-RestartDrainAfterFailedLaunch {
    param($Fence, $StartedProcess)
    # The old fence was validated before its files were removed for startup.
    # Restore only after a definite failed launch, never while a replacement may live.
    if ($null -ne $StartedProcess -and -not $StartedProcess.HasExited) {
        throw "Replacement process may still be alive: pid=$($StartedProcess.Id)"
    }
    if ($null -ne (Get-VerifiedRuntimeProcess)) {
        throw 'A verified runtime is alive after the failed launch.'
    }
    $lockedPid = Get-RuntimePid
    if ($lockedPid -gt 0 -and $null -ne (Get-Process -Id $lockedPid -ErrorAction SilentlyContinue)) {
        throw "Runtime lock still belongs to a live process: pid=$lockedPid"
    }
    foreach ($path in @($DrainPreparePath, $DrainAckPath)) {
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            $existing = Get-RestartDrainFence -Path $path
            if (-not (Test-RestartDrainFenceMatch $existing $Fence)) {
                throw 'Restart drain state changed during the failed launch.'
            }
        }
    }
    $text = (
        "version=1`nruntime_id=$($Fence.RuntimeId)`n" +
        "process_identity=$($Fence.ProcessIdentity)`nnonce=$($Fence.Nonce)`n"
    )
    Write-AtomicRestartMarker -Path $DrainPreparePath -Text $text
    Write-AtomicRestartMarker -Path $DrainAckPath -Text ($text + "state=sealed`n")
}
