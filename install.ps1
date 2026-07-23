$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Start-DaemonIfRequested {
    if ($env:GIT_AI_RESTART_DAEMON_AFTER_INSTALL -ne '1') {
        return
    }

    $daemonExe = Join-Path $HOME '.git-ai\bin\git-ai.exe'
    if (-not (Test-Path $daemonExe)) {
        Write-Warning 'Warning: Failed to locate git-ai.exe for daemon restart after install.'
        return
    }

    try {
        & $daemonExe bg start *> $null
    } catch {
        Write-Warning 'Warning: Failed to restart git-ai background service automatically.'
    }
}

function Write-ErrorAndExit {
    param(
        [Parameter(Mandatory = $true)][string]$Message
    )
    Write-Host "Error: $Message" -ForegroundColor Red
    Start-DaemonIfRequested
    exit 1
}

function Write-Success {
    param(
        [Parameter(Mandatory = $true)][string]$Message
    )
    Write-Host $Message -ForegroundColor Green
}

function Write-Warning {
    param(
        [Parameter(Mandatory = $true)][string]$Message
    )
    Write-Host $Message -ForegroundColor Yellow
}

function Normalize-PathString {
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    try {
        return ([IO.Path]::GetFullPath($Path.Trim())).TrimEnd('\').ToLowerInvariant()
    } catch {
        return ($Path.Trim()).TrimEnd('\').ToLowerInvariant()
    }
}

function Test-FileAvailable {
    param(
        [Parameter(Mandatory = $true)][string]$Path
    )

    try {
        $stream = [System.IO.File]::Open($Path, 'Open', 'Write', 'None')
        $stream.Close()
        return $true
    } catch {
        return $false
    }
}

function Stop-GitAiBackgroundService {
    param(
        [Parameter(Mandatory = $true)][string]$GitAiExe,
        [Parameter(Mandatory = $false)][switch]$Hard
    )

    if (-not (Test-Path -LiteralPath $GitAiExe)) {
        return $false
    }

    $args = @('bg', 'shutdown')
    if ($Hard) {
        $args += '--hard'
    }

    try {
        & $GitAiExe @args *> $null
        return $LASTEXITCODE -eq 0
    } catch {
        return $false
    }
}

function Get-GitAiManagedProcesses {
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir
    )

    $targetPaths = @(
        (Normalize-PathString (Join-Path $InstallDir 'git-ai.exe')),
        (Normalize-PathString (Join-Path $InstallDir 'git.exe'))
    )

    $processes = @(Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {
            $_.ProcessId -ne $PID -and
            $_.ExecutablePath -and
            ($targetPaths -contains (Normalize-PathString $_.ExecutablePath))
        })

    return $processes
}

function Stop-GitAiManagedProcesses {
    param(
        [Parameter(Mandatory = $true)][string]$InstallDir
    )

    $processes = @(Get-GitAiManagedProcesses -InstallDir $InstallDir)
    if ($processes.Count -eq 0) {
        return $false
    }

    $pids = @($processes | Sort-Object ProcessId -Unique | Select-Object -ExpandProperty ProcessId)
    Write-Warning ("Stopping lingering git-ai processes: {0}" -f ($pids -join ', '))

    foreach ($managedPid in $pids) {
        try {
            Stop-Process -Id $managedPid -Force -ErrorAction Stop
        } catch { }
    }

    return $true
}

function Wait-ForFileAvailable {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$InstallDir,
        [Parameter(Mandatory = $false)][int]$MaxWaitSeconds = 300,
        [Parameter(Mandatory = $false)][int]$RetryIntervalSeconds = 5,
        [Parameter(Mandatory = $false)][int]$ForceKillAfterSeconds = 20
    )

    $elapsed = 0
    $gitAiExe = Join-Path $InstallDir 'git-ai.exe'

    [void](Stop-GitAiBackgroundService -GitAiExe $gitAiExe)

    while ($elapsed -lt $MaxWaitSeconds) {
        if (Test-FileAvailable -Path $Path) {
            return $true
        }

        if ($elapsed -ge $ForceKillAfterSeconds) {
            [void](Stop-GitAiBackgroundService -GitAiExe $gitAiExe -Hard)
            [void](Stop-GitAiManagedProcesses -InstallDir $InstallDir)
        }

        if (-not (Test-FileAvailable -Path $Path)) {
            if ($elapsed -eq 0) {
                Write-Host "Waiting for file to be available: $Path" -ForegroundColor Yellow
            }
            Start-Sleep -Seconds $RetryIntervalSeconds
            $elapsed += $RetryIntervalSeconds
        }
    }
    return $false
}

function Verify-Checksum {
    param(
        [Parameter(Mandatory = $true)][string]$File,
        [Parameter(Mandatory = $true)][string]$BinaryName
    )

    # Skip verification if no checksums are embedded
    if ($EmbeddedChecksums -eq '__CHECKSUMS_PLACEHOLDER__') {
        return
    }

    # Extract expected checksum for this binary
    $expected = $null
    $entries = $EmbeddedChecksums -split '\|'
    foreach ($entry in $entries) {
        if ($entry -match "^([0-9a-fA-F]+)\s+$([regex]::Escape($BinaryName))$") {
            $expected = $Matches[1]
            break
        }
    }

    if (-not $expected) {
        Write-ErrorAndExit "No checksum found for $BinaryName"
    }

    # Calculate actual checksum
    $hashCommand = Get-Command Get-FileHash -ErrorAction SilentlyContinue
    if ($null -ne $hashCommand) {
        $actual = (Get-FileHash -Path $File -Algorithm SHA256).Hash.ToLower()
    } else {
        $stream = [System.IO.File]::OpenRead($File)
        try {
            $sha256 = [System.Security.Cryptography.SHA256]::Create()
            $hashBytes = $sha256.ComputeHash($stream)
            $actual = ([System.BitConverter]::ToString($hashBytes)).Replace('-', '').ToLower()
        } finally {
            $stream.Dispose()
            if ($sha256) {
                $sha256.Dispose()
            }
        }
    }

    if ($expected -ne $actual) {
        Remove-Item -Force -ErrorAction SilentlyContinue $File
        Write-ErrorAndExit "Checksum verification failed for $BinaryName`nExpected: $expected`nActual:   $actual"
    }

    Write-Success "Checksum verified for $BinaryName"
}

# GitHub repository details
# Replaced during release builds with the actual repository (e.g., "git-ai-project/git-ai")
# When set to __REPO_PLACEHOLDER__, defaults to "git-ai-project/git-ai"
$Repo = '__REPO_PLACEHOLDER__'
if ($Repo -eq '__REPO_PLACEHOLDER__') {
    $Repo = 'git-ai-project/git-ai'
}

# Version placeholder - replaced during release builds with actual version (e.g., "v1.0.24")
# When set to __VERSION_PLACEHOLDER__, defaults to "latest"
$PinnedVersion = '__VERSION_PLACEHOLDER__'

# Embedded checksums - replaced during release builds with actual SHA256 checksums
# Format: "hash  filename|hash  filename|..." (pipe-separated)
# When set to __CHECKSUMS_PLACEHOLDER__, checksum verification is skipped
$EmbeddedChecksums = '__CHECKSUMS_PLACEHOLDER__'

# Ensure TLS 1.2 for GitHub downloads on older PowerShell versions
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch { }

function Get-Architecture {
    try {
        $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
        switch ($arch) {
            'X64' { return 'x64' }
            'Arm64' { return 'arm64' }
            default { return $null }
        }
    } catch {
        $pa = $env:PROCESSOR_ARCHITECTURE
        if ($pa -match 'ARM64') { return 'arm64' }
        elseif ($pa -match '64') { return 'x64' }
        else { return $null }
    }
}

# Detect architecture and OS
$arch = Get-Architecture
if (-not $arch) { Write-ErrorAndExit "Unsupported architecture: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture)" }
$os = 'windows'

# Determine binary name and download URLs
$binaryName = "git-ai-$os-$arch"

# Determine release tag
# Priority: 1. Local binary override, 2. Pinned version (for release builds), 3. Environment variable, 4. "latest"
if (-not [string]::IsNullOrWhiteSpace($env:GIT_AI_LOCAL_BINARY)) {
    $releaseTag = 'local'
} elseif ($PinnedVersion -ne '__VERSION_PLACEHOLDER__') {
    # Version-pinned install script from a release
    $releaseTag = $PinnedVersion
    $downloadUrlExe = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName.exe"
    $downloadUrlNoExt = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName"
} elseif (-not [string]::IsNullOrWhiteSpace($env:GIT_AI_RELEASE_TAG) -and $env:GIT_AI_RELEASE_TAG -ne 'latest') {
    # Environment variable override
    $releaseTag = $env:GIT_AI_RELEASE_TAG
    $downloadUrlExe = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName.exe"
    $downloadUrlNoExt = "https://github.com/$Repo/releases/download/$releaseTag/$binaryName"
} else {
    # Default to latest
    $releaseTag = 'latest'
    $downloadUrlExe = "https://github.com/$Repo/releases/latest/download/$binaryName.exe"
    $downloadUrlNoExt = "https://github.com/$Repo/releases/latest/download/$binaryName"
}

# ============================================================
# Warn when installing as Administrator (not recommended).
# Running elevated creates files that normal-user processes
# cannot access, causing persistent daemon lock failures.
# ============================================================
$isElevated = $false
try {
    # Detect explicit UAC elevation ("Run as Administrator") via TokenElevationType.
    # Type 1 (Default) = no split token (UAC disabled or built-in Admin) -> no warn
    # Type 2 (Full)    = elevated half of a split token -> WARN (this is the danger case)
    # Type 3 (Limited) = non-elevated half of a split token -> no warn
    # We only warn on type 2: user explicitly elevated, so files will be admin-owned
    # but normal processes won't be, causing the daemon.lock mismatch from issue #1287.
    Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class GitAiElevation {
    [DllImport("advapi32.dll", SetLastError=true)]
    static extern bool OpenProcessToken(IntPtr h, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError=true)]
    static extern bool GetTokenInformation(IntPtr token, int cls, ref int info, int len, out int ret);
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr h);
    public static bool IsElevated() {
        IntPtr tok;
        if (!OpenProcessToken(GetCurrentProcess(), 0x0008, out tok)) return false;
        try {
            int elevType = 0; int sz;
            // TokenElevationType = class 18; returns 1/2/3
            if (!GetTokenInformation(tok, 18, ref elevType, 4, out sz)) return false;
            return elevType == 2; // TokenElevationTypeFull
        } finally { CloseHandle(tok); }
    }
}
"@ -ErrorAction SilentlyContinue
    $isElevated = [GitAiElevation]::IsElevated()
} catch { }

if ($isElevated -and $env:GIT_AI_ALLOW_SUPERUSER -ne '1') {
    # Auto-allow in CI environments and daemon-triggered self-updates
    $isCi = $env:CI -or $env:GITHUB_ACTIONS -or $env:GITLAB_CI -or $env:JENKINS_URL `
        -or $env:BUILDKITE -or $env:CIRCLECI -or $env:CODEBUILD_BUILD_ID `
        -or $env:AGENT_OS -or $env:KUBERNETES_SERVICE_HOST `
        -or $env:GIT_AI_DAEMON_UPGRADE -or $env:container

    if (-not $isCi) {
        Write-Host ''
        Write-Host 'Warning: installing git-ai as Administrator is not recommended.' -ForegroundColor Yellow
        Write-Host ''
        Write-Host 'Running with elevated privileges creates files owned by Administrator that'
        Write-Host 'become inaccessible to your normal user account, causing persistent daemon'
        Write-Host 'lock failures. A future version may refuse to install in this configuration.'
        Write-Host ''
        Write-Host 'To suppress this warning, either:'
        Write-Host '  - Run this installer from a normal (non-elevated) PowerShell window (recommended), or'
        Write-Host '  - Set $env:GIT_AI_ALLOW_SUPERUSER = "1"' -ForegroundColor Yellow
        Write-Host ''
    }
    # Propagate to child git-ai invocations (install-hooks, exchange-nonce, login)
    $env:GIT_AI_ALLOW_SUPERUSER = '1'
}

# Install directory: %USERPROFILE%\.git-ai\bin
$installDir = Join-Path $HOME ".git-ai\bin"
New-Item -ItemType Directory -Force -Path $installDir | Out-Null

Write-Host ("Downloading git-ai (release: {0})..." -f $releaseTag)
$tmpFile = Join-Path $installDir "git-ai.tmp.$PID.exe"

function Try-Download {
    param(
        [Parameter(Mandatory = $true)][string]$Url
    )
    try {
        # Disable progress bar to avoid extreme slowdown caused by PowerShell's
        # progress-stream rendering (can make downloads 10-50x slower).
        $oldProgressPreference = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'
        try {
            Invoke-WebRequest -Uri $Url -OutFile $tmpFile -UseBasicParsing -ErrorAction Stop
        } finally {
            $ProgressPreference = $oldProgressPreference
        }
        return $true
    } catch {
        return $false
    }
}

# Track which download URL succeeded for checksum verification
$downloadedBinaryName = $null
if (-not [string]::IsNullOrWhiteSpace($env:GIT_AI_LOCAL_BINARY)) {
    if (-not (Test-Path -LiteralPath $env:GIT_AI_LOCAL_BINARY)) {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
        Write-ErrorAndExit "Local binary not found at $($env:GIT_AI_LOCAL_BINARY)"
    }
    Copy-Item -Force -Path $env:GIT_AI_LOCAL_BINARY -Destination $tmpFile
    $downloadedBinaryName = "$binaryName.exe"
} elseif (Try-Download -Url $downloadUrlExe) {
    $downloadedBinaryName = "$binaryName.exe"
} elseif (Try-Download -Url $downloadUrlNoExt) {
    $downloadedBinaryName = $binaryName
}

if (-not $downloadedBinaryName) {
    Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
    Write-ErrorAndExit 'Failed to download binary (HTTP error)'
}

try {
    if ((Get-Item $tmpFile).Length -le 0) {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
        Write-ErrorAndExit 'Downloaded file is empty'
    }
} catch {
    Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
    Write-ErrorAndExit 'Download failed'
}

# Verify checksum if embedded (release builds only)
Verify-Checksum -File $tmpFile -BinaryName $downloadedBinaryName

$finalExe = Join-Path $installDir 'git-ai.exe'

# Wait for git-ai.exe to be available if it exists and is in use
if (Test-Path -LiteralPath $finalExe) {
    if (-not (Wait-ForFileAvailable -Path $finalExe -InstallDir $installDir -MaxWaitSeconds 300 -RetryIntervalSeconds 5)) {
        Remove-Item -Force -ErrorAction SilentlyContinue $tmpFile
        Write-ErrorAndExit "Timeout waiting for $finalExe to be available. Please close any running git-ai processes and try again."
    }
}

Move-Item -Force -Path $tmpFile -Destination $finalExe
try { Unblock-File -Path $finalExe -ErrorAction SilentlyContinue } catch { }

# Refresh git.exe for existing wrapper users (it's a copy, not a symlink on Windows)
$gitShim = Join-Path $installDir 'git.exe'
if (Test-Path -LiteralPath $gitShim) {
    if (-not (Wait-ForFileAvailable -Path $gitShim -InstallDir $installDir -MaxWaitSeconds 300 -RetryIntervalSeconds 5)) {
        Write-ErrorAndExit "Timeout waiting for $gitShim to be available. Please close any running git processes and try again."
    }
    Copy-Item -Force -Path $finalExe -Destination $gitShim
    try { Unblock-File -Path $gitShim -ErrorAction SilentlyContinue } catch { }
}

# Login user with install token if provided
$needLogin = $false
if ($env:INSTALL_NONCE -and $env:API_BASE) {
    try {
        & $finalExe exchange-nonce | Out-Host
        if ($LASTEXITCODE -ne 0) {
            $needLogin = $true
        }
    } catch {
        $needLogin = $true
    }
}

# Install hooks
Write-Host 'Setting up IDE/agent hooks...'
try {
    & $finalExe install-hooks | Out-Host
    Write-Success 'Successfully set up IDE/agent hooks'
} catch {
    Write-Warning "Warning: Failed to set up IDE/agent hooks. Please try running 'git-ai install-hooks' manually."
}

# Best-effort restart only for daemon-initiated self-updates.
Start-DaemonIfRequested

Write-Success "Successfully installed git-ai into $installDir"
Write-Success "You can now run 'git-ai' from your terminal"

# If nonce exchange failed, run interactive login
if ($needLogin) {
    Write-Host ''
    Write-Host 'Launching login...'
    & $finalExe login
}
