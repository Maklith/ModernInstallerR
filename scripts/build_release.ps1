param(
    [switch]$Debug
)

$ErrorActionPreference = "Stop"

function Copy-WithRetry {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination,
        [int]$MaxAttempts = 8
    )

    for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
        try {
            Copy-Item $Source $Destination -Force
            return
        } catch {
            if ($attempt -eq $MaxAttempts) {
                throw
            }
            Start-Sleep -Milliseconds 500
        }
    }
}

$root = Split-Path -Parent $PSScriptRoot
$infoPath = Join-Path $root "installer_assets\info.json"
if (-not (Test-Path $infoPath)) {
    throw "missing installer config: $infoPath"
}

$info = Get-Content $infoPath -Raw | ConvertFrom-Json
$target = if ($info.Is64) { "x86_64-pc-windows-msvc" } else { "i686-pc-windows-msvc" }
$profile = if ($Debug) { "debug" } else { "release" }

Write-Host "Target architecture: $target"
Write-Host "Build profile: $profile"

$uninstallerArgs = @("build", "--target", $target, "--bin", "modern_uninstaller_r")
if (-not $Debug) { $uninstallerArgs += "--release" }
& cargo @uninstallerArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$uninstallerExe = Join-Path $root "target\$target\$profile\modern_uninstaller_r.exe"
$embeddedUninstaller = Join-Path $root "installer_assets\ModernInstaller.Uninstaller.exe"
Copy-WithRetry -Source $uninstallerExe -Destination $embeddedUninstaller
Write-Host "Updated embedded uninstaller: $embeddedUninstaller"

$installerArgs = @("build", "--target", $target, "--bin", "modern_installer_r")
if (-not $Debug) { $installerArgs += "--release" }
& cargo @installerArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$installerExe = Join-Path $root "target\$target\$profile\modern_installer_r.exe"
$distDir = Join-Path $root "dist\$target"
New-Item -ItemType Directory -Path $distDir -Force | Out-Null
Copy-WithRetry -Source $installerExe -Destination (Join-Path $distDir "ModernInstaller.exe")
Copy-WithRetry -Source $uninstallerExe -Destination (Join-Path $distDir "ModernInstaller.Uninstaller.exe")

Write-Host "Done. Outputs:"
Write-Host "  $(Join-Path $distDir 'ModernInstaller.exe')"
Write-Host "  $(Join-Path $distDir 'ModernInstaller.Uninstaller.exe')"
