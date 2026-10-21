# Registers (or replaces) the "MonsterOS projection-service" scheduled task:
# starts run-projection-service.ps1 at logon for the current user, hidden, with
# no time limit. Run once; then Start-ScheduledTask "MonsterOS projection-service".

$name = "MonsterOS projection-service"
$script = Join-Path $PSScriptRoot "run-projection-service.ps1"
$action = New-ScheduledTaskAction -Execute "powershell.exe" `
    -Argument "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$script`""
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName $name -Action $action -Trigger $trigger -Settings $settings `
    -Description "MonsterOS projection-service + learning autopilot (supervised; logs in %LOCALAPPDATA%\MonsterOS\logs)" -Force | Out-Null
Write-Output "registered: $name"
