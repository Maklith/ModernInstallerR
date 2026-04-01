param(
    [switch]$Debug
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.IO.Compression.FileSystem

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

function Compress-ToGzip {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    $inputStream = [System.IO.File]::OpenRead($Source)
    try {
        $outputStream = [System.IO.File]::Create($Destination)
        try {
            $gzip = New-Object System.IO.Compression.GZipStream(
                $outputStream,
                [System.IO.Compression.CompressionLevel]::Optimal
            )
            try {
                $inputStream.CopyTo($gzip)
            } finally {
                $gzip.Dispose()
            }
        } finally {
            $outputStream.Dispose()
        }
    } finally {
        $inputStream.Dispose()
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

$uninstallerManifest = Join-Path $root "modern_uninstaller_r\Cargo.toml"
$uninstallerTargetDir = Join-Path $root "target\standalone-uninstaller"
$uninstallerArgs = @(
    "build",
    "--manifest-path",
    $uninstallerManifest,
    "--target",
    $target,
    "--target-dir",
    $uninstallerTargetDir
)
if (-not $Debug) { $uninstallerArgs += "--release" }
& cargo @uninstallerArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$uninstallerExe = Join-Path $uninstallerTargetDir "$target\$profile\modern_uninstaller_r.exe"
Write-Host "Built standalone uninstaller: $uninstallerExe"

$installerArgs = @("build", "--target", $target, "--bin", "modern_installer_r")
if (-not $Debug) { $installerArgs += "--release" }
& cargo @installerArgs
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

$installerExe = Join-Path $root "target\$target\$profile\modern_installer_r.exe"
$distDir = Join-Path $root "dist\$target"
New-Item -ItemType Directory -Path $distDir -Force | Out-Null
$installerDist = Join-Path $distDir "ModernInstaller.exe"
$uninstallerDist = Join-Path $distDir "ModernInstaller.Uninstaller.exe"
Copy-WithRetry -Source $installerExe -Destination $installerDist
Copy-WithRetry -Source $uninstallerExe -Destination $uninstallerDist

Compress-ToGzip -Source $installerDist -Destination (Join-Path $distDir "ModernInstaller.exe.gz")
Compress-ToGzip -Source $uninstallerDist -Destination (Join-Path $distDir "ModernInstaller.Uninstaller.exe.gz")

Write-Host "Done. Outputs:"
Write-Host "  $installerDist"
Write-Host "  $uninstallerDist"
Write-Host "  $(Join-Path $distDir 'ModernInstaller.exe.gz')"
Write-Host "  $(Join-Path $distDir 'ModernInstaller.Uninstaller.exe.gz')"
