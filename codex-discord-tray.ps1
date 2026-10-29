[CmdletBinding()]
param([switch]$Once)

$ErrorActionPreference = 'Stop'
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$TrayRuntimePath = Join-Path $ScriptDir 'codex-discord-tray-runtime.ps1'
$TrayRestartRuntimePath = Join-Path $ScriptDir 'codex-discord-tray-restart-runtime.ps1'
$HeadlessLauncher = Join-Path $ScriptDir 'codex-discord-bot-headless.vbs'
$LauncherLogPath = Join-Path $ScriptDir 'discord_launcher.log'
$BotLogPath = Join-Path $ScriptDir 'codex_discord_rust.log'
$StoppedGraceSeconds = 30
$script:MissingSince = $null
$script:LastRunningStatus = $null
$script:LastTrayIcon = $null
$script:LastStatusError = $null
$script:LastStatusErrorAt = [datetime]::MinValue

function Write-LauncherLog {
    param([string]$Message)
    # Diagnostics must never terminate the UI or its error/cleanup handler.
    try {
        $line = ($Message -replace '[\r\n]+', ' ')
        if ($line.Length -gt 700) { $line = $line.Substring(0, 700) }
        $timestamp = (Get-Date).ToString('s')
        Add-Content -LiteralPath $LauncherLogPath -Encoding UTF8 -ErrorAction Stop -Value "[$timestamp] pid=$PID $line"
    } catch { }
}

function Write-TrayStatusError {
    param([string]$Diagnostic)
    $now = [datetime]::UtcNow
    if ($null -eq $script:LastStatusError -or ($now - $script:LastStatusErrorAt).TotalSeconds -ge 60) {
        Write-LauncherLog "tray_status_unavailable $Diagnostic"
        $script:LastStatusErrorAt = $now
    }
    $script:LastStatusError = $Diagnostic
}

function Limit-TrayText {
    param([string]$Text)
    if ($Text.Length -le 63) { return $Text }
    return $Text.Substring(0, 60) + '...'
}

try {
    foreach ($runtimePath in @($TrayRuntimePath, $TrayRestartRuntimePath)) {
        if (-not (Test-Path -LiteralPath $runtimePath -PathType Leaf)) {
            throw "tray runtime script not found: $runtimePath"
        }
    }
    . $TrayRuntimePath
    $RuntimeMode = Resolve-TrayRuntimeMode
    . $TrayRestartRuntimePath
} catch {
    Write-LauncherLog "tray_initialization_failed type=$($_.Exception.GetType().FullName) error=$($_.Exception.Message)"
    throw
}

if ($Once) {
    $status = Get-BotStatus
    if ($status.State -eq 'unknown') { Write-Output 'unknown'; exit 2 }
    if ($status.Running) { Write-Output "running pid=$($status.Pid)"; exit 0 }
    Write-Output 'stopped'
    exit 1
}

function Update-TrayStatus {
    try {
        $status = Get-BotStatus
        if ($status.State -eq 'unknown') {
            # An observation error is not a stopped/restarting or healthy result.
            $script:MissingSince = $null
            $script:LastRunningStatus = $null
            Write-TrayStatusError $status.Diagnostic
        } else {

            if ($status.Running) {
                $script:MissingSince = $null
                $script:LastRunningStatus = $status
            } else {
                if ($null -eq $script:MissingSince) { $script:MissingSince = Get-Date }
                $missingSeconds = ((Get-Date) - $script:MissingSince).TotalSeconds
                if ($null -ne $script:LastRunningStatus -and $missingSeconds -lt $StoppedGraceSeconds) {
                    $status = [pscustomobject]@{
                        State = 'restarting'; Running = $false; Pid = $null
                        Text = "Codex Discord bridge restarting (last PID $($script:LastRunningStatus.Pid))"
                        Icon = 'Application'
                    }
                }
            }
        }
        $statusItem.Text = $status.Text
        $notify.Text = Limit-TrayText $status.Text
        $nextIcon = $status.Icon
        if ($script:LastTrayIcon -ne $nextIcon) {
            $notify.Icon = if ($nextIcon -eq 'Application') { [Drawing.SystemIcons]::Application } else { [Drawing.SystemIcons]::Warning }
            $script:LastTrayIcon = $nextIcon
        }
        # Recovery is real only after BOTH observation and UI publication succeed.
        if ($status.State -ne 'unknown' -and $null -ne $script:LastStatusError) {
            Write-LauncherLog 'tray_status_recovered'
            $script:LastStatusError = $null
        }
    } catch {
        $script:MissingSince = $null
        $script:LastRunningStatus = $null
        $script:LastTrayIcon = $null
        Write-TrayStatusError "ui_refresh_failed type=$($_.Exception.GetType().FullName)"
        # Even a broken UI object or logging destination cannot escape a timer tick.
        try { $statusItem.Text = 'Codex Discord bridge status unavailable' } catch { }
        try { $notify.Text = 'Codex Discord bridge status unavailable'; $notify.Icon = [Drawing.SystemIcons]::Warning } catch { }
    }
}

$notify = $null; $menu = $null; $timer = $null; $mutex = $null
$mutexOwned = $false
try {
    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -AssemblyName System.Drawing
    [Windows.Forms.Application]::EnableVisualStyles()
    $mutexName = Get-CdrTrayMutexName $ScriptDir
    $mutex = New-Object Threading.Mutex($false, $mutexName)
    try { $mutexOwned = $mutex.WaitOne(0) }
    catch [Threading.AbandonedMutexException] { $mutexOwned = $true }
    if (-not $mutexOwned) {
        Write-LauncherLog "tray_duplicate_exit script=$PSCommandPath"
        exit 0
    }

    $notify = New-Object Windows.Forms.NotifyIcon
    $notify.Icon = [Drawing.SystemIcons]::Information
    $notify.Text = 'Codex Discord bridge starting'
    $notify.Visible = $true
    $menu = New-Object Windows.Forms.ContextMenuStrip
    $statusItem = New-Object Windows.Forms.ToolStripMenuItem
    $statusItem.Text = 'Starting...'
    $statusItem.Enabled = $false
    [void]$menu.Items.Add($statusItem)

    # Optional feature initialization cannot break existing restart/status controls.
    try {
        . (Join-Path $ScriptDir 'codex-discord-tray-agy.ps1')
        Initialize-CdrAgyTray -Menu $menu -Root $ScriptDir
    } catch { Write-LauncherLog "tray_agy_unavailable type=$($_.Exception.GetType().FullName)" }

    $openLogItem = New-Object Windows.Forms.ToolStripMenuItem
    $openLogItem.Text = 'Open bot log'
    $openLogItem.Add_Click({
        try {
            if (Test-Path -LiteralPath $BotLogPath) { Start-Process -FilePath 'notepad.exe' -ArgumentList @("`"$BotLogPath`"") }
        } catch { Write-LauncherLog "tray_open_log_failed type=$($_.Exception.GetType().FullName)" }
    })
    [void]$menu.Items.Add($openLogItem)
    $openFolderItem = New-Object Windows.Forms.ToolStripMenuItem
    $openFolderItem.Text = 'Open bridge folder'
    $openFolderItem.Add_Click({
        try { Start-Process -FilePath 'explorer.exe' -ArgumentList @("`"$ScriptDir`"") }
        catch { Write-LauncherLog "tray_open_folder_failed type=$($_.Exception.GetType().FullName)" }
    })
    [void]$menu.Items.Add($openFolderItem)

    $restartItem = New-Object Windows.Forms.ToolStripMenuItem
    $restartItem.Text = 'Restart bot'
    $restartItem.Add_Click({
        try {
            Request-BotRestart
            $notify.ShowBalloonTip(3000, 'Codex Discord bridge', 'Restart requested.', [Windows.Forms.ToolTipIcon]::Info)
        } catch {
            Write-LauncherLog "tray_restart_failed type=$($_.Exception.GetType().FullName) error=$($_.Exception.Message)"
            try { $notify.ShowBalloonTip(3000, 'Codex Discord bridge', 'Restart request failed. Check discord_launcher.log.', [Windows.Forms.ToolTipIcon]::Error) } catch { }
        }
    })
    [void]$menu.Items.Add($restartItem)
    $forceRestartItem = New-Object Windows.Forms.ToolStripMenuItem
    $forceRestartItem.Text = 'Force restart bot + app-server (interrupt work)'
    $forceRestartItem.Add_Click({
        try {
            Request-BotForceRestart
            $notify.ShowBalloonTip(3000, 'Codex Discord bridge', 'Force restart requested. Active work will be interrupted.', [Windows.Forms.ToolTipIcon]::Warning)
        } catch {
            Write-LauncherLog "tray_force_restart_failed type=$($_.Exception.GetType().FullName) error=$($_.Exception.Message)"
            try { $notify.ShowBalloonTip(3000, 'Codex Discord bridge', 'Force restart request failed. Check discord_launcher.log.', [Windows.Forms.ToolTipIcon]::Error) } catch { }
        }
    })
    [void]$menu.Items.Add($forceRestartItem)
    [void]$menu.Items.Add((New-Object Windows.Forms.ToolStripSeparator))
    $exitItem = New-Object Windows.Forms.ToolStripMenuItem
    $exitItem.Text = 'Exit tray icon'
    $exitItem.Add_Click({
        try { [Windows.Forms.Application]::Exit() }
        catch { Write-LauncherLog "tray_exit_request_failed type=$($_.Exception.GetType().FullName)" }
    })
    [void]$menu.Items.Add($exitItem)
    $notify.ContextMenuStrip = $menu

    $timer = New-Object Windows.Forms.Timer
    $timer.Interval = 5000
    $timer.Add_Tick({ Update-TrayStatus })
    Write-LauncherLog "tray_start script=$PSCommandPath"
    Update-TrayStatus
    $timer.Start()
    [Windows.Forms.Application]::Run()
} catch {
    Write-LauncherLog "tray_fatal type=$($_.Exception.GetType().FullName) error=$($_.Exception.Message)"
    throw
} finally {
    try {if (Get-Command Close-CdrAgyTray -ErrorAction SilentlyContinue) {Close-CdrAgyTray}} catch { }
    # Each release is independent: partial initialization must not leak ownership.
    if ($null -ne $timer) { try { $timer.Stop() } catch { }; try { $timer.Dispose() } catch { } }
    if ($null -ne $notify) { try { $notify.Visible = $false } catch { }; try { $notify.Dispose() } catch { } }
    if ($null -ne $menu) { try { $menu.Dispose() } catch { } }
    if ($null -ne $mutex) {
        if ($mutexOwned) { try { $mutex.ReleaseMutex() } catch { Write-LauncherLog 'tray_mutex_release_failed' } }
        try { $mutex.Dispose() } catch { }
    }
    Write-LauncherLog "tray_exit script=$PSCommandPath"
}
