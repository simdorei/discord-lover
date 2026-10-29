# Optional AGY UI. Network/process waits occur only in a background runspace.
# This helper never writes configuration or starts/restarts the bridge.
function Initialize-CdrAgyTray {
    param($Menu,[string]$Root)
    $script:AgyRoot=$Root
    $script:AgyCache=$null; $script:AgyCacheAt=[datetime]::MinValue
    $script:AgyPending=$null; $script:AgyJob=$null; $script:AgyError='Connecting...'
    $script:AgyNextRead=[datetime]::MinValue
    $script:AgyPool=[RunspaceFactory]::CreateRunspacePool(1,1)
    $script:AgyPool.Open()
    if (-not ('CdrAgyClock' -as [type])) {
        Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public static class CdrAgyClock { [DllImport("kernel32.dll")] public static extern ulong GetTickCount64(); }'
    }
    $script:AgyMenu=New-Object Windows.Forms.ToolStripMenuItem('AGY (optional)')
    $script:AgyStatusItem=New-Object Windows.Forms.ToolStripMenuItem('Status unavailable')
    $script:AgyStatusItem.Enabled=$false
    [void]$script:AgyMenu.DropDownItems.Add($script:AgyStatusItem)
    $script:AgyItems=@{}
    foreach ($entry in @(@('on','Enable AGY'),@('off','Disable after current work'),@('stop','Stop AGY work now'),@('login','Open official AGY login'),@('recheck','Check again'),@('configure','AGY settings...'))) {
        $item=New-Object Windows.Forms.ToolStripMenuItem($entry[1]);$item.Tag=$entry[0]
        $item.Add_Click({param($sender,$eventArgs) Invoke-CdrAgyMenu ([string]$sender.Tag)})
        $script:AgyItems[$entry[0]]=$item;[void]$script:AgyMenu.DropDownItems.Add($item)
    }
    [void]$Menu.Items.Add($script:AgyMenu)
    $script:AgyTimer=New-Object Windows.Forms.Timer
    $script:AgyTimer.Interval=100
    $script:AgyTimer.Add_Tick({ try { Update-CdrAgyBackground } catch { $script:AgyError=$_.Exception.Message; $script:AgyCacheAt=[datetime]::MinValue; Update-CdrAgyMenu } })
    $script:AgyTimer.Start()
}

function Start-CdrAgyCall {
    param($Operation)
    if ($null -ne $script:AgyJob) {return $false}
    $pipeline=[PowerShell]::Create();$pipeline.RunspacePool=$script:AgyPool
    $worker={param($exe,$root,$payload)
        $process=New-Object Diagnostics.Process
        try {
            $start=New-Object Diagnostics.ProcessStartInfo
            $start.FileName=$exe;$start.Arguments='--agy-control';$start.WorkingDirectory=$root
            $start.UseShellExecute=$false;$start.CreateNoWindow=$true
            $start.RedirectStandardInput=$true;$start.RedirectStandardOutput=$true;$start.RedirectStandardError=$true
            $start.StandardOutputEncoding=New-Object Text.UTF8Encoding($false)
            $start.StandardErrorEncoding=New-Object Text.UTF8Encoding($false)
            $process.StartInfo=$start
            if (-not $process.Start()) {throw 'AGY control helper did not start'}
            $output=$process.StandardOutput.ReadToEndAsync();$errors=$process.StandardError.ReadToEndAsync()
            $bytes=[Text.Encoding]::UTF8.GetBytes($payload)
            $process.StandardInput.BaseStream.Write($bytes,0,$bytes.Length);$process.StandardInput.Close()
            if (-not $process.WaitForExit(4500)) {try {$process.Kill()} catch {};throw 'AGY status unavailable. The command may already be accepted; its result will be queried.'}
            $text=$output.GetAwaiter().GetResult();$diagnostic=$errors.GetAwaiter().GetResult()
            if ($process.ExitCode -ne 0) {throw $diagnostic}
            $text
        } finally {$process.Dispose()}
    }
    [void]$pipeline.AddScript($worker.ToString()).AddArgument((Join-Path $script:AgyRoot 'target\release\cdr-runtime.exe')).AddArgument($script:AgyRoot).AddArgument(($Operation | ConvertTo-Json -Depth 20 -Compress))
    $script:AgyJob=[pscustomobject]@{Pipeline=$pipeline;Async=$pipeline.BeginInvoke();Operation=$Operation}
    return $true
}

function Update-CdrAgyBackground {
    if ($script:AgyEditing) {return}
    if ($null -ne $script:AgyJob -and $script:AgyJob.Async.IsCompleted) {
        $job=$script:AgyJob;$script:AgyJob=$null
        try {
            $output=$job.Pipeline.EndInvoke($job.Async)
            if ($job.Pipeline.HadErrors) {throw ($job.Pipeline.Streams.Error | Out-String)}
            $reply=($output | Out-String | ConvertFrom-Json)
            $status=$reply.status
            $prior=$script:AgyCache
            if ($null -ne $prior -and $prior.status.runtime_uuid -ne $status.runtime_uuid) {
                # Commands are never replayed into a new runtime.
                if ($null -ne $script:AgyPending) {$script:AgyError='Bridge changed. Previous command was not replayed.'}
                $script:AgyPending=$null
                if ($job.Operation.operation -notin @('status','settings')) {
                    $script:AgyCacheAt=[datetime]::MinValue
                    $script:AgyNextRead=[datetime]::MinValue
                    throw 'Bridge changed. Fetching current state; previous command was not replayed.'
                }
                $prior=$null
            }
            if ($null -eq $prior -or ($status.sequence -ge $prior.status.sequence -and $status.config_revision -ge $prior.status.config_revision)) {
                $script:AgyCache=$reply;$script:AgyCacheAt=[datetime]::UtcNow
                $script:AgyError=[string]$reply.error
            }
            if ($null -ne $script:AgyPending -and $job.Operation.operation -in @('mutate','result') -and $reply.outcome -in @('complete','rejected','expired')) {
                $script:AgyPending=$null
                if ($reply.error) {[void][Windows.Forms.MessageBox]::Show([string]$reply.error,'AGY command result')}
            }
            if ($job.Operation.operation -eq 'settings') {Show-CdrAgySettings $reply.settings}
        } catch {
            $script:AgyCacheAt=[datetime]::MinValue;$script:AgyError=$_.Exception.Message
        } finally {$job.Pipeline.Dispose()}
        Update-CdrAgyMenu
    }
    if ($null -eq $script:AgyJob -and [datetime]::UtcNow -ge $script:AgyNextRead) {
        $operation=if ($null -ne $script:AgyPending) {@{operation='result';id=$script:AgyPending}}else{@{operation='status'}}
        [void](Start-CdrAgyCall $operation)
        $script:AgyNextRead=[datetime]::UtcNow.AddSeconds(5)
    }
    Update-CdrAgyMenu
}

function Update-CdrAgyMenu {
    $fresh=$null -ne $script:AgyCache -and ([datetime]::UtcNow-$script:AgyCacheAt).TotalSeconds -lt 12
    $phase=if ($fresh) {[string]$script:AgyCache.status.phase}else{'Unavailable'}
    $script:AgyStatusItem.Text="AGY: $phase"
    $script:AgyStatusItem.ToolTipText=if ($script:AgyError) {$script:AgyError}elseif ($fresh) {[string]$script:AgyCache.status.detail}else{'Cannot verify current AGY state.'}
    $idle=$fresh -and $null -eq $script:AgyPending -and $null -eq $script:AgyJob
    $inactive=$phase -in @('Off','SetupRequired','AuthRequired','VerificationRequired','Faulted')
    foreach ($name in @('on','login','recheck','configure')) {$script:AgyItems[$name].Enabled=$idle -and $inactive}
    $script:AgyItems['off'].Enabled=$idle -and $phase -notin @('Off','Draining')
    $script:AgyItems['stop'].Enabled=$idle -and $phase -in @('Ready','Checking','Provisioning','Draining','FaultedCleaning','FaultedBlocked')
}

function Invoke-CdrAgyMenu {
    param([string]$Name)
    try {
        if ($null -ne $script:AgyPending -or $null -ne $script:AgyJob) {return}
        if ($Name -eq 'configure') {[void](Start-CdrAgyCall @{operation='settings'});return}
        $revision=[System.UInt64]$script:AgyCache.status.config_revision
        $command=switch ($Name) {
            'on' {@{command='on';expected_revision=$revision}}
            'off' {@{command='off';expected_revision=$revision;immediate=$false}}
            'stop' {@{command='off';expected_revision=$revision;immediate=$true}}
            'login' {@{command='login';expected_revision=$revision}}
            'recheck' {@{command='recheck';expected_revision=$revision}}
            default {throw 'Unknown AGY menu command'}
        }
        Send-CdrAgyMutation $command
    } catch {$script:AgyError=$_.Exception.Message;Update-CdrAgyMenu}
}
function Send-CdrAgyMutation {
    param($Command)
    $runtime=[string]$script:AgyCache.status.runtime_uuid
    $id="$runtime/$([CdrAgyClock]::GetTickCount64())/$([guid]::NewGuid().ToString())"
    $script:AgyPending=$id
    if (-not (Start-CdrAgyCall @{operation='mutate';id=$id;command=$Command})) {$script:AgyPending=$null;throw 'Another AGY operation is pending'}
    $script:AgyNextRead=[datetime]::UtcNow.AddSeconds(1)
    Update-CdrAgyMenu
}

function Show-CdrAgySettings {
    param($Settings)
    $expectedRevision=Get-CdrAgySettingsRevision $Settings
    $form=New-Object Windows.Forms.Form
    $script:AgyEditing=$true
    try {
        $form.Text='AGY settings';$form.Width=660;$form.Height=430;$form.StartPosition='CenterScreen'
        $label=New-Object Windows.Forms.Label;$label.Text='Official AGY executable';$label.SetBounds(12,12,450,22);$form.Controls.Add($label)
        $exe=New-Object Windows.Forms.TextBox;$exe.SetBounds(12,38,530,25);$form.Controls.Add($exe)
        $browse=New-Object Windows.Forms.Button;$browse.Text='Browse...';$browse.SetBounds(550,36,85,28);$form.Controls.Add($browse)
        $browse.Add_Click({$dialog=New-Object Windows.Forms.OpenFileDialog;try {if ($dialog.ShowDialog() -eq 'OK') {$exe.Text=$dialog.FileName}}finally{$dialog.Dispose()}}.GetNewClosure())
        $hint=New-Object Windows.Forms.Label;$hint.Text='Projects: lowercase name and existing full folder path. Existing conversation links are retained.';$hint.SetBounds(12,76,620,35);$form.Controls.Add($hint)
        $grid=New-Object Windows.Forms.DataGridView;$grid.SetBounds(12,115,620,190);$grid.AllowUserToAddRows=$true;$grid.AllowUserToDeleteRows=$true
        [void]$grid.Columns.Add('name','Project name');[void]$grid.Columns.Add('cwd','Full project folder');$grid.Columns[1].Width=420;$form.Controls.Add($grid)
        $existing=@{}
        if ($null -ne $Settings) {
            $exe.Text=[string]$Settings.config.executable
            foreach ($project in $Settings.config.projects) {[void]$grid.Rows.Add([string]$project.name,[string]$project.cwd);$existing[[string]$project.name]=$project}
        }
        $save=New-Object Windows.Forms.Button;$save.Text='Save (keep AGY off)';$save.SetBounds(420,325,210,32);$save.DialogResult=[Windows.Forms.DialogResult]::OK;$form.Controls.Add($save)
        $cancel=New-Object Windows.Forms.Button;$cancel.Text='Cancel';$cancel.SetBounds(310,325,100,32);$cancel.DialogResult=[Windows.Forms.DialogResult]::Cancel;$form.Controls.Add($cancel)
        if ($form.ShowDialog() -ne [Windows.Forms.DialogResult]::OK) {return}
        $projects=@()
        foreach ($row in $grid.Rows) {if (-not $row.IsNewRow) {
            $name=[string]$row.Cells[0].Value;$cwd=[string]$row.Cells[1].Value
            $channel=if ($existing.ContainsKey($name)) {$existing[$name].channel_id}else{$null}
            $projects+=@{name=$name;cwd=$cwd;channel_id=$channel}
        }}
        $config=@{executable=$exe.Text;projects=$projects;category_id=$null;model=$null;auto_approve=$false}
        if ($null -ne $Settings) {$config.category_id=$Settings.config.category_id;$config.model=$Settings.config.model;$config.auto_approve=$Settings.config.auto_approve}
        Send-CdrAgyMutation @{command='configure';expected_revision=$expectedRevision;config=$config}
    } finally {$script:AgyEditing=$false;$form.Dispose()}
}
function Get-CdrAgySettingsRevision {
    param($Settings)
    # Revision belongs to the displayed settings, never a separately refreshed status.
    if ($null -eq $Settings) {return [System.UInt64]0}
    if ($null -eq $Settings.revision) {throw 'Settings revision is missing. Fetch the settings again.'}
    return [System.UInt64]$Settings.revision
}
function Close-CdrAgyTray {
    if ($null -ne $script:AgyTimer) {$script:AgyTimer.Stop();$script:AgyTimer.Dispose()}
    # Closing the tray never cancels an accepted runtime command or kills the bot.
    if ($null -ne $script:AgyJob) {$script:AgyJob.Pipeline.BeginStop($null,$null) | Out-Null}
    if ($null -ne $script:AgyPool) {$script:AgyPool.CloseAsync()}
}
