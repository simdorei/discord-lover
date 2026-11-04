$ErrorActionPreference = 'Stop'

function ConvertTo-WindowsNativeArgument {
    param(
        [AllowEmptyString()]
        [string]$Argument
    )

    if ($Argument.Length -gt 0 -and $Argument -notmatch '[\s"]') {
        return $Argument
    }

    $builder = [System.Text.StringBuilder]::new()
    $null = $builder.Append('"')
    $backslashCount = 0
    foreach ($character in $Argument.ToCharArray()) {
        if ($character -eq [char]92) {
            $backslashCount++
            continue
        }
        if ($character -eq [char]34) {
            if ($backslashCount -gt 0) {
                $null = $builder.Append(
                    [string]::new([char]92, ($backslashCount * 2) + 1)
                )
            } else {
                $null = $builder.Append([char]92)
            }
            $null = $builder.Append([char]34)
            $backslashCount = 0
            continue
        }
        if ($backslashCount -gt 0) {
            $null = $builder.Append([string]::new([char]92, $backslashCount))
            $backslashCount = 0
        }
        $null = $builder.Append($character)
    }
    if ($backslashCount -gt 0) {
        $null = $builder.Append(
            [string]::new([char]92, $backslashCount * 2)
        )
    }
    $null = $builder.Append('"')
    return $builder.ToString()
}

function Invoke-CdrNative {
    param([string]$Executable, [string[]]$Arguments, [AllowNull()][string]$InputText,
        [int]$TimeoutSeconds = 120, [hashtable]$EnvironmentOverrides = @{}, [ref]$ProcessIdentity,
        [string]$WorkingDirectory)

    if ($PSBoundParameters.ContainsKey('ProcessIdentity')) { $ProcessIdentity.Value = $null }

    $exe = $Executable

    $resolvedCommand = Get-Command -Name $exe -ErrorAction SilentlyContinue
    if ($null -ne $resolvedCommand -and
            -not [string]::IsNullOrWhiteSpace($resolvedCommand.Source)) {
        $exe = $resolvedCommand.Source
    }

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    if ($WorkingDirectory) { $startInfo.WorkingDirectory = [IO.Path]::GetFullPath($WorkingDirectory) }
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.RedirectStandardInput = $PSBoundParameters.ContainsKey('InputText')
    foreach ($name in $EnvironmentOverrides.Keys) {
        $startInfo.EnvironmentVariables[$name] = [string]$EnvironmentOverrides[$name]
    }
    $utf8 = [System.Text.UTF8Encoding]::new($false)
    $startInfo.StandardOutputEncoding = $utf8
    $startInfo.StandardErrorEncoding = $utf8
    if ($startInfo.RedirectStandardInput) { $startInfo.StandardInputEncoding = $utf8 }

    $extension = [System.IO.Path]::GetExtension($exe).ToLowerInvariant()
    if ($extension -eq '.ps1') {
        $cmdCandidate = [System.IO.Path]::ChangeExtension($exe, '.cmd')
        if (-not (Test-Path -LiteralPath $cmdCandidate -PathType Leaf)) {
            throw "Codex resolved to a PowerShell shim without a sibling .cmd launcher: $exe. Pass -CodexExe with the Codex .exe or .cmd launcher."
        }
        $exe = $cmdCandidate
        $extension = '.cmd'
    }
    if ($extension -eq '.cmd' -or $extension -eq '.bat') {
        $commandShell = $env:ComSpec
        if ([string]::IsNullOrWhiteSpace($commandShell)) {
            $commandShell = Join-Path $env:SystemRoot 'System32\cmd.exe'
        }
        $variablePrefix = 'CODEX_INSTALL_' + [guid]::NewGuid().ToString('N')
        $exeVariable = $variablePrefix + '_EXE'
        $startInfo.EnvironmentVariables[$exeVariable] = $exe
        $commandParts = [System.Collections.Generic.List[string]]::new()
        $commandParts.Add('"%' + $exeVariable + '%"')
        for ($index = 0; $index -lt $Arguments.Count; $index++) {
            $argument = [string]$Arguments[$index]
            if ($argument.Contains('"')) {
                throw 'Codex .cmd arguments cannot contain a double quote.'
            }
            $argumentVariable = $variablePrefix + '_ARG_' + $index
            $startInfo.EnvironmentVariables[$argumentVariable] = $argument
            $commandParts.Add('"%' + $argumentVariable + '%"')
        }
        $startInfo.FileName = $commandShell
        $startInfo.Arguments = '/d /s /v:off /c "' + ($commandParts -join ' ') + '"'
    } else {
        $startInfo.FileName = $exe
        $nativeArguments = @(
            $Arguments | ForEach-Object {
                ConvertTo-WindowsNativeArgument -Argument ([string]$_)
            }
        )
        $startInfo.Arguments = $nativeArguments -join ' '
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) {
            throw "Codex CLI process did not start: $exe"
        }
        # Cache the OS image name while the child exists. A hard-link launcher
        # such as cargo.exe can actually be reported as rustup.exe by Windows.
        $launchedName = $null
        if ($PSBoundParameters.ContainsKey('ProcessIdentity')) {
            $imageName = $process.ProcessName
            if ([string]::IsNullOrWhiteSpace($imageName)) {
                # A fast exit can leave ProcessName empty. The observer still
                # requires this launch candidate to match the actual ETW image.
                $launchedName = [System.IO.Path]::GetFileName($startInfo.FileName)
            } else {
                $launchedName = $imageName + '.exe'
            }
        }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if ($startInfo.RedirectStandardInput) {
            $process.StandardInput.Write($InputText)
            $process.StandardInput.Close()
        }
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            throw "Native command timed out after $TimeoutSeconds seconds: $exe"
        }
        $stdoutText = $stdoutTask.GetAwaiter().GetResult()
        $stderrText = $stderrTask.GetAwaiter().GetResult()
        $exitCode = $process.ExitCode

        if ($exitCode -ne 0) {
            $details = @()
            if (-not [string]::IsNullOrWhiteSpace($stdoutText)) {
                $details += "stdout:`n$stdoutText"
            }
            if (-not [string]::IsNullOrWhiteSpace($stderrText)) {
                $details += "stderr:`n$stderrText"
            }
            $detailText = if ($details.Count -gt 0) {
                "`n" + ($details -join "`n")
            }
            else {
                ''
            }
            throw "Native command failed with exit code ${exitCode}: $exe $($Arguments -join ' ')${detailText}"
        }

        if ($PSBoundParameters.ContainsKey('ProcessIdentity')) {
            $ProcessIdentity.Value = [pscustomobject]@{
                pid = [uint32]$process.Id; name = $launchedName
                created_tick = [uint64]$process.StartTime.ToUniversalTime().ToFileTimeUtc()
                exited_tick = [uint64]$process.ExitTime.ToUniversalTime().ToFileTimeUtc()
            }
        }
        if (-not [string]::IsNullOrWhiteSpace($stderrText)) {
            Write-Warning $stderrText
        }
        if ($stdoutText.Length -gt 0) {
            Write-Output $stdoutText
        }
    }
    finally {
        $process.Dispose()
    }
}


Export-ModuleMember -Function Invoke-CdrNative
