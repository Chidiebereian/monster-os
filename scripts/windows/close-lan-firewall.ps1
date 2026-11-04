# Removes the inbound firewall rules that let other machines reach Monster OS (ops backlog B2).
#
# Windows creates an "allow" rule (all ports, TCP and UDP) the first time a program listens on
# the network and someone clicks "Allow access". Every Monster OS service had one for the Public
# profile, the profile this PC uses on guest Wi-Fi. The services now listen on loopback only
# (common::bind_addrs, BIND_ADDR overrides), so these rules are no longer needed.
#
# Run in an elevated PowerShell (Run as administrator):
#   powershell -ExecutionPolicy Bypass -File scripts\windows\close-lan-firewall.ps1           # list only
#   powershell -ExecutionPolicy Bypass -File scripts\windows\close-lan-firewall.ps1 -Apply    # remove them
# Optional, also remove the rules of shared tools (they apply to every program using them):
#   -Node     node.exe (C:\Program Files\nodejs)          -Python  C:\Python313\python.exe
#   -Docker   Docker Desktop backend (not needed: published ports are on 127.0.0.1)
# Only rules tied to these programs are touched; nothing else in the firewall changes.
param([switch]$Apply, [switch]$Node, [switch]$Python, [switch]$Docker)

$repo = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$programs = @(Get-ChildItem (Join-Path $repo "target\debug\*.exe") | ForEach-Object { $_.FullName })
if ($Node) { $programs += "C:\Program Files\nodejs\node.exe" }
if ($Python) { $programs += "C:\Python313\python.exe" }
if ($Docker) { $programs += "C:\Program Files\Docker\Docker\resources\com.docker.backend.exe" }

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if ($Apply -and -not $admin) { throw "Run this from an elevated PowerShell (Run as administrator) to remove rules." }

# inbound rules by program path, from one verbose dump (`show rule` cannot filter by program;
# `delete rule` can). Windows stores the paths lowercased, so compare case-insensitively.
$byProgram = @{}
$name = $null
foreach ($line in (netsh advfirewall firewall show rule name=all dir=in verbose)) {
    if ($line -match '^Rule Name:\s+(.*)$') { $name = $Matches[1].Trim() }
    elseif ($line -match '^Program:\s+(.*)$' -and $name) {
        $k = $Matches[1].Trim().ToLowerInvariant()
        if (-not $byProgram.ContainsKey($k)) { $byProgram[$k] = @() }
        $byProgram[$k] += $name
    }
}

$found = 0
foreach ($p in $programs) {
    $names = @($byProgram[$p.ToLowerInvariant()] | Where-Object { $_ })
    if ($names.Count -eq 0) { continue }
    $found += $names.Count
    Write-Output ("{0,-3} rule(s)  {1}" -f $names.Count, $p)
    if ($Apply) { netsh advfirewall firewall delete rule name=all dir=in program="$p" | Out-Null }
}
if ($found -eq 0) { Write-Output "No inbound rules found for these programs." }
elseif ($Apply) { Write-Output "Removed $found inbound rule(s)." }
else { Write-Output "$found inbound rule(s) listed; nothing changed. Re-run with -Apply (as administrator) to remove them." }
