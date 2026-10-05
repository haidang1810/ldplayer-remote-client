# Registers a logon task that starts the agent hidden in the background (log: agent.log).
# LDPlayer runs in the user's desktop session, so the agent starts at logon rather than as a service.
# Usage (PowerShell, from the project folder):  powershell -ExecutionPolicy Bypass -File scripts\install-agent-task.ps1
# Remove:                                        Unregister-ScheduledTask -TaskName LDPlayerRemoteAgent

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
if (-not (Test-Path (Join-Path $root 'agent.env'))) {
    throw "Missing $root\agent.env (needs RELAY_URL and AGENT_KEY)."
}
$node = (Get-Command node).Source
$command = "Set-Location -LiteralPath '$root'; & '$node' --env-file=agent.env agent/index.js *> agent.log"

$action = New-ScheduledTaskAction -Execute 'powershell.exe' `
    -Argument "-NoProfile -WindowStyle Hidden -Command `"$command`"" -WorkingDirectory $root
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)

Register-ScheduledTask -TaskName 'LDPlayerRemoteAgent' -Action $action -Trigger $trigger -Settings $settings `
    -Description 'LDPlayer Remote agent (connects this PC to the relay)' -Force | Out-Null
Start-ScheduledTask -TaskName 'LDPlayerRemoteAgent'
Write-Host "Agent task registered and started. Log: $root\agent.log"
