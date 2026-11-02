# CI check: silent per-user install of setup.exe, verify files + PATH, then uninstall and
# verify everything (including the PATH entry) is gone.
param([Parameter(Mandatory)][string]$Setup)
$ErrorActionPreference = 'Stop'
$dir = Join-Path $env:LOCALAPPDATA 'Programs\memdex'
$userPath = { [Environment]::GetEnvironmentVariable('Path', 'User') }
$onPath = { (& $userPath) -split ';' -contains $dir }

Start-Process $Setup -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait
if (-not (Test-Path "$dir\memdex.exe")) { throw "memdex.exe not installed in $dir" }
Write-Host "installed: $(& "$dir\memdex.exe" --version)"
if (-not (& $onPath)) { throw "install dir not added to user PATH" }
Write-Host "user PATH contains $dir"

# Installing twice must not duplicate the PATH entry.
Start-Process $Setup -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait
$count = ((& $userPath) -split ';' | Where-Object { $_ -eq $dir }).Count
if ($count -ne 1) { throw "PATH entry appears $count times after reinstall" }

Start-Process "$dir\unins000.exe" -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait
Start-Sleep -Seconds 2
if (Test-Path "$dir\memdex.exe") { throw "memdex.exe still present after uninstall" }
if (& $onPath) { throw "PATH entry still present after uninstall" }
Write-Host "uninstalled cleanly (files and PATH entry removed)"
