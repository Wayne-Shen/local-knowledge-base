param(
    [Parameter(Mandatory=$true)]
    [ValidateSet("Install", "Status", "Remove")]
    [string]$Action,
    [string]$Config = $env:KB_CONFIG
)

$ErrorActionPreference = "Stop"
$Project = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$Task = "LocalKnowledgeBase-$env:USERNAME"

switch ($Action) {
    "Install" {
        $Binary = (Resolve-Path (Join-Path $Project "target/release/kb.exe")).Path
        if (-not $Config) { $Config = Join-Path $Project "config.yaml" }
        $Config = (Resolve-Path $Config).Path
        $Identity = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
        $Run = New-ScheduledTaskAction -Execute $Binary -Argument ('--config "{0}" serve' -f $Config) -WorkingDirectory $Project
        $Trigger = New-ScheduledTaskTrigger -AtLogOn -User $Identity
        $Principal = New-ScheduledTaskPrincipal -UserId $Identity -LogonType Interactive -RunLevel Limited
        $Settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -RestartCount 5 -RestartInterval (New-TimeSpan -Minutes 1) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        $Existing = Get-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue
        if ($Existing -and $Existing.State -eq 'Running') {
            Stop-ScheduledTask -TaskName $Task
        }
        Register-ScheduledTask -TaskName $Task -Action $Run -Trigger $Trigger -Principal $Principal -Settings $Settings -Force | Out-Null
        Start-ScheduledTask -TaskName $Task
    }
    "Status" {
        Get-ScheduledTask -TaskName $Task
        Get-ScheduledTaskInfo -TaskName $Task
    }
    "Remove" {
        $Existing = Get-ScheduledTask -TaskName $Task -ErrorAction SilentlyContinue
        if ($Existing) {
            if ($Existing.State -eq 'Running') { Stop-ScheduledTask -TaskName $Task }
            Unregister-ScheduledTask -TaskName $Task -Confirm:$false
        }
    }
}
