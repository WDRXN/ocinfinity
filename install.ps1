# ocinfinity installer — Windows.
#
#   irm https://raw.githubusercontent.com/WDRXN/ocinfinity/main/install.ps1 | iex
#
# Environment overrides:
#   OCNINFINITY_VERSION      release tag to install (default: the latest release)
#   OCNINFINITY_INSTALL_DIR  destination directory
#                            (default: %LOCALAPPDATA%\Programs\ocinfinity)
#   OCNINFINITY_UNINSTALL    set to 1 to remove the installed binary
#   GITHUB_TOKEN             only needed if the repository is private
#
# Written for Windows PowerShell 5.1 as well as PowerShell 7, and for execution
# through Invoke-Expression, so it has no param() block — options come from the
# environment.

$ErrorActionPreference = 'Stop'

$Repo = 'WDRXN/ocinfinity'
$Bin = 'ocinfinity.exe'
$InstallDir = if ($env:OCNINFINITY_INSTALL_DIR) {
    $env:OCNINFINITY_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA 'Programs\ocinfinity'
}
$Target = Join-Path $InstallDir $Bin

function Say($message) { Write-Host $message }
function Fail($message) { Write-Host "error: $message" -ForegroundColor Red; exit 1 }

# PowerShell 5.1 still negotiates TLS 1.0 by default on some builds, which
# GitHub rejects outright.
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch {
    # Older runtimes without the enum member; the default is good enough there.
}

function Get-Headers {
    $headers = @{ 'User-Agent' = 'ocinfinity-installer' }
    if ($env:GITHUB_TOKEN) { $headers['Authorization'] = "Bearer $($env:GITHUB_TOKEN)" }
    return $headers
}

# Use the same file name the public API expects so that a browser download and
# this installer produce byte-identical files.
$headers = Get-Headers

# ---------------------------------------------------------------- uninstall ---

if ($env:OCNINFINITY_UNINSTALL -eq '1') {
    if (Test-Path $Target) {
        Remove-Item $Target -Force
        Say "removed $Target"
    } else {
        Say "nothing to remove at $Target"
    }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -and ($userPath.Split(';') -contains $InstallDir)) {
        $cleaned = ($userPath.Split(';') | Where-Object { $_ -and $_ -ne $InstallDir }) -join ';'
        [Environment]::SetEnvironmentVariable('Path', $cleaned, 'User')
        Say "removed $InstallDir from the user PATH"
    }
    Say 'configuration is left in place'
    return
}

# --------------------------------------------------------------- discovery ---

$arch = $env:PROCESSOR_ARCHITECTURE
if (-not $arch) { $arch = (Get-CimInstance Win32_Processor).Architecture }
if ($arch -eq 'ARM64' -or $arch -eq 12) {
    Fail 'no Windows arm64 build is published; build from source instead'
}
if ($arch -notin @('AMD64', 'x86', '0', '9')) {
    Say "warning: unexpected architecture '$arch', continuing with the x64 build"
}

$tag = $env:OCNINFINITY_VERSION
if (-not $tag) {
    Say 'resolving the latest release...'
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers -TimeoutSec 30
        $tag = $release.tag_name
    } catch {
        Fail "could not query the latest release: $($_.Exception.Message)"
    }
}
if (-not $tag) { Fail 'could not determine the latest tag; set OCNINFINITY_VERSION' }

Say "installing ocinfinity $tag (windows/x64)"

$base = "https://github.com/$Repo/releases/download/$tag"
$asset = 'ocinfinity-windows.exe'

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("ocinfinity-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null

try {
    $downloaded = Join-Path $tmp $asset
    try {
        Invoke-WebRequest -Uri "$base/$asset" -Headers $headers -OutFile $downloaded -TimeoutSec 120 -UseBasicParsing
    } catch {
        Fail "download failed: $base/$asset ($($_.Exception.Message))"
    }

    # ------------------------------------------------------------- verify ---
    try {
        $sums = Invoke-RestMethod -Uri "$base/SHA256SUMS" -Headers $headers -TimeoutSec 30
        $line = ($sums -split "`n" | Where-Object { $_ -match "\s$([regex]::Escape($asset))$" } | Select-Object -First 1)
        if ($line) {
            $expected = ($line -split '\s+')[0]
            $actual = (Get-FileHash -Path $downloaded -Algorithm SHA256).Hash.ToLower()
            if ($actual -ne $expected.ToLower()) {
                Fail "checksum mismatch for $asset`n  expected $expected`n  actual   $actual"
            }
            Say 'checksum verified'
        }
    } catch {
        if ($_.Exception.Message -like '*checksum mismatch*') { throw }
        Say 'warning: SHA256SUMS not available, skipping verification'
    }

    # ------------------------------------------------------------ install ---
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item $downloaded $Target -Force

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $userPath) { $userPath = '' }
    if (-not ($userPath.Split(';') -contains $InstallDir)) {
        $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Say "added $InstallDir to the user PATH (new terminals only)"
    }
    $env:Path = "$env:Path;$InstallDir"

    Say "installed $Target"
    Say ''
    Say 'next:'
    Say "  ocinfinity doctor                 # check the local setup"
    Say '  ocinfinity connect claude-code    # start the bridge and open Claude Code'
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
