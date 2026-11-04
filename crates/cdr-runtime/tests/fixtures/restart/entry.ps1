param([switch]$OldAlive,[switch]$FailFirst,[switch]$StartedAlive)
$ErrorActionPreference = 'Stop'
            $root = $env:RESTART_ENTRY_ROOT
            $source = Get-Content -LiteralPath (Join-Path $root 'codex-discord-rust-watchdog.ps1') -Raw
            $entryIndex = $source.IndexOf('# CONTROL_ENTRY:')
            if ($entryIndex -lt 0) { throw 'watchdog entry boundary missing' }
            # Execute the real initialization and helper definitions, then substitute only
            # process/probe boundaries. Actual normal-entry branching and marker IO run.
            . ([scriptblock]::Create($source.Substring(0, $entryIndex))) `
                -RepoRoot $root -BinaryPath (Join-Path $root 'probe.exe')
            $entry = [scriptblock]::Create($source.Substring($entryIndex))
            . (Join-Path $PSScriptRoot 'artifact.ps1')
            $script:oldAlive = $OldAlive
            $script:startCount = 0
            $script:newAlive = $false
            function Get-VerifiedRuntimeProcess {
                if ($script:oldAlive) { [pscustomobject]@{ Id=42 } }
                elseif ($script:newAlive) { [pscustomobject]@{ Id=77 } } else { $null }
            }
            function Get-RustProcessIdentity { param($Process); if ($null -ne $Process) { "$($Process.Id)|99" } else { '' } }
            function Get-Process { param($Id,$ErrorAction)
                if($Id -eq 77 -and $script:newAlive){[pscustomobject]@{Id=77;Path=$BinaryPath}}
            }
            function Get-HeartbeatHealth { [pscustomobject]@{ Healthy=$true; Bootstrap=$false; State='healthy' } }
            function Enter-RestartDrain { throw 'unexpected second restart preparation' }
            function Wait-RustThreadsQuietForRestart { throw 'unexpected second readiness probe' }
            function Wait-RustRuntimeExit {
                param($Process, $ExpectedIdentity, $Reason)
                if ($ExpectedIdentity -ne '42|99' -or $Reason -ne 'restart_requested') { throw 'wrong exit binding' }
                Write-Output 'WAIT_EXIT'
                $script:oldAlive = $false
            }
            function Start-Process { throw 'test must not start a real process' }
            function Stop-VerifiedRuntime { throw 'test must not kill a process' }
            function Start-RustRuntime {
                param([switch]$ResumeRemoteMcp)
                if (-not $ResumeRemoteMcp) { throw 'restart handoff flag lost' }
                $script:startCount += 1
                Write-Output "START_ATTEMPT=$script:startCount"
                if ($FailFirst -and $script:startCount -eq 1) {
                    if ($StartedAlive) {
                        Set-CdrLaunchStarting
                        $script:RustRestartStartedProcess = [pscustomobject]@{ Id=77; HasExited=$false }
                    }
                    throw 'controlled launch failure'
                }
                $script:newAlive = $true
                Set-CdrLaunchStarting
                Set-CdrLaunchChild ([pscustomobject]@{Id=77;Path=$BinaryPath})
            }
            if ($FailFirst) {
                try { & $entry; throw 'expected launch failure' }
                catch { if ($_.Exception.Message -notmatch 'controlled launch failure') { throw } }
                if ($StartedAlive) {
                    if (Test-Path -LiteralPath $DrainAckPath) { throw 'unsafe restart authorization restored' }
                    $journal=Read-CdrLaunchJournal (Join-Path $RepoRoot '.codex_discord_rust.restart.launch')
                    if($journal.Phase -ne 'launching'){throw 'unknown launch was not fenced'}
                    try { & $entry; throw 'unsafe second start was accepted' }
                    catch { if ($_.Exception.Message -notmatch 'launch_outcome_unknown') { throw } }
                    exit 0
                }
                $journal=Read-CdrLaunchJournal (Join-Path $RepoRoot '.codex_discord_rust.restart.launch')
                if($journal.Phase -ne 'prepared' -or $journal.Fence.Nonce -ne 'current') {
                    throw 'retry authorization journal was lost'
                }
            }
            & $entry
