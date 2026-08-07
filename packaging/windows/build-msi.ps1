param(
    [Parameter(Mandatory = $true)][string]$BinaryPath,
    [Parameter(Mandatory = $true)][ValidateSet('x64', 'arm64')][string]$Architecture,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$OutputPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not (Test-Path -LiteralPath $BinaryPath)) {
    throw "Binary not found: $BinaryPath"
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$wxsPath = Join-Path $PSScriptRoot 'git-ai.wxs'
$stageDir = Join-Path $repoRoot "target\package\msi-$Architecture"
$stagedExe = Join-Path $stageDir 'git-ai.exe'
$outputFullPath = [System.IO.Path]::GetFullPath($OutputPath)
$outputDir = [System.IO.Path]::GetDirectoryName($outputFullPath)
$upgradeCode = '4B6D731B-CB6B-48F2-8A0A-A4344C91E1E0'

New-Item -ItemType Directory -Force -Path $stageDir | Out-Null
New-Item -ItemType Directory -Force -Path $outputDir | Out-Null
Copy-Item -Force -LiteralPath $BinaryPath -Destination $stagedExe

$wixVersion = '7.0.0'
$wixUtilExtension = 'WixToolset.Util.wixext/7.0.0'
Write-Host "Installing WiX .NET tool v$wixVersion..."
dotnet tool update --global wix --version $wixVersion | Out-Host
$env:PATH = "$env:USERPROFILE\.dotnet\tools;$env:PATH"
$wix = Get-Command wix -ErrorAction Stop

& $wix.Source eula accept wix7 | Out-Host
if ($LASTEXITCODE -ne 0) {
    throw "WiX EULA acceptance failed with exit code $LASTEXITCODE"
}

& $wix.Source extension add --global $wixUtilExtension | Out-Host
if ($LASTEXITCODE -ne 0) {
    throw "WiX extension install failed with exit code $LASTEXITCODE"
}

$platform = if ($Architecture -eq 'arm64') { 'arm64' } else { 'x64' }
& $wix.Source build -acceptEula wix7 $wxsPath `
    -ext $wixUtilExtension `
    -arch $platform `
    -d "ProductVersion=$Version" `
    -d "GitAiExe=$stagedExe" `
    -d "UpgradeCode={$upgradeCode}" `
    -o $outputFullPath

if ($LASTEXITCODE -ne 0) {
    throw "WiX failed with exit code $LASTEXITCODE"
}

Write-Host "Built MSI: $outputFullPath"
