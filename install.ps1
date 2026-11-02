# Install memdex from GitHub releases on Windows (x64 / ARM64, static memdex.exe).
#
#   irm https://raw.githubusercontent.com/jasondyck/memdex/main/install.ps1 | iex
#
# Options (environment variables):
#   MEMDEX_VERSION=v0.5.0    install a specific release instead of the latest
#   MEMDEX_BIN_DIR=DIR       install location (default %LOCALAPPDATA%\Programs\memdex)
#   MEMDEX_NO_PATH=1         don't add the install location to the user PATH
#   MEMDEX_REPO=owner/name   install from a fork
#   MEMDEX_BASE_URL=URL      download from a mirror (expects <URL>/<asset>)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = if ($env:MEMDEX_REPO) { $env:MEMDEX_REPO } else { 'jasondyck/memdex' }
$Version = if ($env:MEMDEX_VERSION) { $env:MEMDEX_VERSION } else { 'latest' }
$BinDir = if ($env:MEMDEX_BIN_DIR) { $env:MEMDEX_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\memdex' }

# OS architecture (not the process's: x64 PowerShell on ARM64 Windows still gets ARM64).
$osArch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
$arch = switch ($osArch) {
    'X64' { 'x86_64' }
    'Arm64' { 'aarch64' }
    default { throw "memdex: no prebuilt binary for $osArch; build from source (see README)" }
}

$target = "$arch-pc-windows-msvc"
$asset = "memdex-$target.zip"
$base = if ($env:MEMDEX_BASE_URL) { $env:MEMDEX_BASE_URL }
        elseif ($Version -eq 'latest') { "https://github.com/$Repo/releases/latest/download" }
        else { "https://github.com/$Repo/releases/download/$Version" }

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("memdex-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "Downloading $asset ($Version)..."
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile (Join-Path $tmp $asset)
    Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset.sha256" -OutFile (Join-Path $tmp "$asset.sha256")

    $want = ((Get-Content (Join-Path $tmp "$asset.sha256") -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
    $got = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($want -ne $got) { throw "memdex: checksum mismatch, not installing" }
    Write-Host "${asset}: OK"

    Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
    Copy-Item (Join-Path $tmp "memdex-$target\memdex.exe") $BinDir -Force
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

$exe = Join-Path $BinDir 'memdex.exe'
Write-Host "Installed $(& $exe --version) to $exe"

if (-not $env:MEMDEX_NO_PATH) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = if ($userPath) { $userPath -split ';' | Where-Object { $_ } } else { @() }
    if ($entries -notcontains $BinDir) {
        [Environment]::SetEnvironmentVariable('Path', (@($entries) + $BinDir) -join ';', 'User')
        Write-Host "Added $BinDir to your user PATH (open a new terminal to use 'memdex')."
    }
}

Write-Host ""
Write-Host "Next: register it with Claude Code (user scope):"
Write-Host "  claude mcp add --scope user memdex -- `"$exe`" serve"
