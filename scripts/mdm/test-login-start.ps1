# End-to-end test for mdm/windows/install-login-start.ps1.
#
# Installs git-ai into $HOME, registers the logon task, starts it as the logon
# trigger would, and checks that the daemon (a) comes up, (b) survives the task
# instance ending, (c) restarts itself on schedule while registered, and
# (d) the task stays healthy throughout. The auto-update scenario additionally
# starts from an older release and waits for the daemon to self-update.
#
# Usage: test-login-start.ps1 [-Scenario lifecycle|auto-update]
#
# Environment: BINARY_SOURCE (local|release), GIT_AI_LOCAL_BINARY,
# GIT_AI_RELEASE_TAG, LATEST_TAG, MDM_TEST_LOG_DIR — same meaning as in
# test-login-start.sh.
param(
    [ValidateSet('lifecycle', 'auto-update')]
    [string]$Scenario = 'lifecycle'
)

$ErrorActionPreference = 'Stop'

$BinarySource = if ($env:BINARY_SOURCE) { $env:BINARY_SOURCE } else { 'local' }
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$LogOut = if ($env:MDM_TEST_LOG_DIR) { $env:MDM_TEST_LOG_DIR } else { Join-Path ($env:RUNNER_TEMP, $env:TEMP | Where-Object { $_ } | Select-Object -First 1) 'mdm-logs' }
$Bin = Join-Path $HOME '.git-ai\bin\git-ai.exe'
$DaemonDir = Join-Path $HOME '.git-ai\internal\daemon'
$TaskPath = '\GitAI\'
$TaskName = 'Start bg at logon'
$MdmScript = Join-Path $RepoRoot 'mdm\windows\install-login-start.ps1'
$StatusRepo = Join-Path ([IO.Path]::GetTempPath()) ("mdm-status-" + [guid]::NewGuid().ToString('N'))

function Write-Log([string]$Message) { Write-Host "[mdm-test] $Message" }
function Fail([string]$Message) { throw "[mdm-test] FAIL: $Message" }

# --- helpers -----------------------------------------------------------------

function Install-Binary {
    switch ($BinarySource) {
        'local' {
            if (-not ($env:GIT_AI_LOCAL_BINARY -and (Test-Path $env:GIT_AI_LOCAL_BINARY))) {
                Fail 'GIT_AI_LOCAL_BINARY must point at a built git-ai.exe'
            }
        }
        'release' { }
        default { Fail 'BINARY_SOURCE must be local or release' }
    }
    Push-Location $RepoRoot
    try {
        pwsh -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
        if ($LASTEXITCODE -ne 0) { Fail "install.ps1 exited $LASTEXITCODE" }
    } finally {
        Pop-Location
    }
    if (-not (Test-Path $Bin)) { Fail "$Bin missing after install" }
    Write-Log "installed $(& $Bin --version)"
}

function Get-InstalledVersion {
    return (& $Bin --version).Trim().Split()[0].TrimStart('v')
}

function Get-DaemonPid {
    $file = Join-Path $DaemonDir 'daemon.pid.json'
    if (-not (Test-Path $file)) { return $null }
    try { return (Get-Content -Raw $file | ConvertFrom-Json).pid } catch { return $null }
}

function Test-PidAlive($ProcessId) {
    return $ProcessId -and (Get-Process -Id $ProcessId -ErrorAction SilentlyContinue)
}

function Test-DaemonUp {
    if (-not (Test-PidAlive (Get-DaemonPid))) { return $false }
    Push-Location $StatusRepo
    try {
        & $Bin bg status *> $null
        return $LASTEXITCODE -eq 0
    } finally {
        Pop-Location
    }
}

function Get-DaemonProcesses {
    Get-CimInstance Win32_Process -Filter "Name = 'git-ai.exe'" |
        Where-Object { $_.CommandLine -match 'bg run' -and $_.ExecutablePath -like "$HOME\*" }
}

# `bg shutdown` returns before the old process has released the daemon lock;
# a logon start racing that window would have to retry.
function Stop-Daemon {
    & $Bin bg shutdown *> $null
    Wait-For 30 'previous daemon exited' { -not (Get-DaemonProcesses) }
}

function Wait-For([int]$Seconds, [string]$What, [scriptblock]$Condition) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while (-not (& $Condition)) {
        if ((Get-Date) -ge $deadline) { Fail "timed out after ${Seconds}s waiting for $What" }
        Start-Sleep -Seconds 1
    }
    Write-Log $What
}

function Invoke-MdmScript {
    # Windows PowerShell, as MDM tooling and `irm | iex` would use it.
    & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $MdmScript @args
    if ($LASTEXITCODE -ne 0) { Fail "install-login-start.ps1 $args exited $LASTEXITCODE" }
}

function Get-Task {
    return Get-ScheduledTask -TaskPath $TaskPath -TaskName $TaskName -ErrorAction SilentlyContinue
}

function Test-Registered { return [bool](Get-Task) }

function Wait-TaskIdle {
    Wait-For 60 'task instance finished' { (Get-Task).State -ne 'Running' }
}

function Test-MechanismSane {
    $task = Get-Task
    if (-not $task) { Fail 'scheduled task missing' }
    Wait-TaskIdle
    $info = Get-ScheduledTaskInfo -TaskPath $TaskPath -TaskName $TaskName
    if ($info.LastTaskResult -ne 0) { Fail "task last result 0x$($info.LastTaskResult.ToString('X'))" }
    Write-Log 'login mechanism healthy'
}

function Invoke-Lint {
    if (Get-Module -ListAvailable PSScriptAnalyzer) {
        $findings = Invoke-ScriptAnalyzer -Path $MdmScript -Severity Error
        if ($findings) { $findings | Format-Table | Out-String | Write-Host; Fail 'PSScriptAnalyzer errors' }
    }
}

function Get-DaemonStartedVersions {
    Get-ChildItem (Join-Path $DaemonDir 'logs') -Filter '*.log' -ErrorAction SilentlyContinue |
        Select-String -Pattern 'daemon started .*version="([^"]*)"' |
        ForEach-Object { $_.Matches[0].Groups[1].Value }
}

function Invoke-Cleanup {
    New-Item -ItemType Directory -Path $LogOut -Force | Out-Null
    Copy-Item (Join-Path $DaemonDir 'logs') (Join-Path $LogOut 'daemon-logs') -Recurse -Force -ErrorAction SilentlyContinue
    Get-Task | Get-ScheduledTaskInfo -ErrorAction SilentlyContinue | Out-File (Join-Path $LogOut 'task-info.txt')
    Copy-Item (Join-Path $HOME '.git-ai\login\start-bg.ps1') $LogOut -Force -ErrorAction SilentlyContinue
    & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File $MdmScript --uninstall *> $null
    if (Test-Path $Bin) { & $Bin bg shutdown *> $null }
    Remove-Item $StatusRepo -Recurse -Force -ErrorAction SilentlyContinue
}

# --- scenarios ---------------------------------------------------------------

function Register-AndWaitForDaemon {
    Invoke-MdmScript @args
    if (-not (Test-Registered)) { Fail 'login start not registered after install' }
    Wait-For 45 'daemon up after logon trigger' { Test-DaemonUp }
}

function Invoke-LifecycleScenario {
    Install-Binary
    # Keep the uptime restart deterministic: no network update checks.
    & $Bin config set disable_auto_updates true
    Stop-Daemon

    Register-AndWaitForDaemon --env GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL=5 --env GIT_AI_DAEMON_MAX_UPTIME_SECS=25
    $pid1 = Get-DaemonPid

    Start-Sleep -Seconds 10
    if (-not (Test-PidAlive $pid1)) { Fail "daemon $pid1 died after the task instance ended (job object torn down?)" }
    if (-not (Test-DaemonUp)) { Fail 'daemon not answering after task instance ended' }
    Write-Log 'daemon survived task instance ending'

    # Max uptime (25s) is past the survival check above but well inside this wait.
    Wait-For 60 'daemon restarted itself on schedule' { $p = Get-DaemonPid; $p -and $p -ne $pid1 }
    $pid2 = Get-DaemonPid
    Wait-For 15 'restarted daemon healthy' { Test-DaemonUp }
    Test-MechanismSane

    Start-ScheduledTask -TaskPath $TaskPath -TaskName $TaskName
    Wait-TaskIdle
    Start-Sleep -Seconds 3
    if (-not (Test-DaemonUp)) { Fail 'daemon unhealthy after re-trigger' }
    if ((Get-DaemonPid) -ne $pid2) { Fail "re-triggering logon restarted the daemon (pid $pid2 -> $(Get-DaemonPid))" }
    Test-MechanismSane
    Write-Log 're-triggered logon start was a no-op'

    Invoke-Lint

    Invoke-MdmScript --uninstall
    if (Test-Registered) { Fail 'task still registered after --uninstall' }
    Write-Log 'uninstall clean'

    Stop-Daemon
    Invoke-UnusualBinaryPathScenario
}

# --bin must cope with every path an admin might install to: spaces, quotes,
# parentheses, percent signs, non-ASCII.
function Invoke-UnusualBinaryPathScenario {
    $dir = Join-Path $HOME "mdm test (weird) 100% 'quoted' ünïcode"
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    Copy-Item $Bin (Join-Path $dir 'git-ai.exe') -Force

    Invoke-MdmScript --bin (Join-Path $dir 'git-ai.exe')
    if (-not (Test-Registered)) { Fail 'login start not registered with unusual --bin' }
    Wait-For 45 'daemon up from unusual binary path' { Test-DaemonUp }
    $running = @(Get-DaemonProcesses | Where-Object { $_.ExecutablePath -like "$dir\*" })
    if (-not $running) { Fail "daemon is not running from $dir" }
    Write-Log 'daemon runs from the unusual path'
    Test-MechanismSane

    Invoke-MdmScript --uninstall
    Stop-Daemon
}

function Invoke-AutoUpdateScenario {
    if ($BinarySource -ne 'release') { Fail 'auto-update needs BINARY_SOURCE=release' }
    if (-not $env:GIT_AI_RELEASE_TAG) { Fail 'auto-update needs GIT_AI_RELEASE_TAG (an older release)' }
    if (-not $env:LATEST_TAG) { Fail 'auto-update needs LATEST_TAG' }
    $latest = $env:LATEST_TAG.TrimStart('v')

    Install-Binary
    $before = Get-InstalledVersion
    if ($before -eq $latest) { Fail 'GIT_AI_RELEASE_TAG must be older than LATEST_TAG' }
    Stop-Daemon

    Register-AndWaitForDaemon --env GIT_AI_DAEMON_UPDATE_CHECK_INTERVAL=10
    $pid1 = Get-DaemonPid

    # The Windows installer runs detached and waits for the exe lock to clear.
    Wait-For 300 "binary updated to $latest" { (Get-InstalledVersion) -eq $latest }
    Wait-For 120 'daemon restarted after update' { $p = Get-DaemonPid; $p -and $p -ne $pid1 }
    Wait-For 30 'updated daemon healthy' { Test-DaemonUp }
    if (-not ((Get-DaemonStartedVersions) -contains $latest)) { Fail "no 'daemon started' log line for version $latest" }
    Test-MechanismSane

    Invoke-MdmScript --uninstall
    if (Test-Registered) { Fail 'task still registered after --uninstall' }
    Write-Log "auto-update $before -> $latest completed under logon start"
}

New-Item -ItemType Directory -Path $StatusRepo -Force | Out-Null
git -C $StatusRepo init -q
try {
    switch ($Scenario) {
        'lifecycle' { Invoke-LifecycleScenario }
        'auto-update' { Invoke-AutoUpdateScenario }
    }
    Write-Log "PASS $Scenario on windows"
} finally {
    Invoke-Cleanup
}
# Cleanup's best-effort `bg shutdown` leaves a non-zero $LASTEXITCODE when no
# daemon is running; do not let pwsh -File report that as the script's result.
exit 0
