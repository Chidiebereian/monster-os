# Registers the backup scheduled tasks for the current user (docs/operations/backup-recovery.md):
#   "MonsterOS backup"         daily 04:30, scripts/ops/backup.py
#   "MonsterOS restore check"  Sundays 05:15, scripts/ops/restore_check.py
# Missed runs (machine off or asleep) start when it is next available. Re-running replaces them.
# Remove with: Unregister-ScheduledTask -TaskName "MonsterOS backup","MonsterOS restore check" -Confirm:$false
$cmd = Join-Path $PSScriptRoot "backup-task.cmd"
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -DontStopIfGoingOnBatteries -AllowStartIfOnBatteries -ExecutionTimeLimit (New-TimeSpan -Hours 1)
$tasks = @(
    @{ Name = "MonsterOS backup"; Arg = "backup"; Trigger = New-ScheduledTaskTrigger -Daily -At 4:30am },
    @{ Name = "MonsterOS restore check"; Arg = "restore_check"; Trigger = New-ScheduledTaskTrigger -Weekly -DaysOfWeek Sunday -At 5:15am }
)
foreach ($t in $tasks) {
    $action = New-ScheduledTaskAction -Execute "cmd.exe" -Argument "/c `"$cmd`" $($t.Arg)"
    Register-ScheduledTask -TaskName $t.Name -Action $action -Trigger $t.Trigger -Settings $settings -Description "Monster OS: scripts/ops/$($t.Arg).py" -Force | Out-Null
    Get-ScheduledTask -TaskName $t.Name | Get-ScheduledTaskInfo | Select-Object TaskName, NextRunTime
}
