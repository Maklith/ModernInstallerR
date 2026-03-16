param(
    [switch]$Debug
)

$ErrorActionPreference = "Stop"

function Resolve-PythonCommand {
    $pyLauncher = Get-Command py -ErrorAction SilentlyContinue
    if ($null -ne $pyLauncher) {
        $pyExe = & py -3 -c "import sys; print(sys.executable)" 2>$null
        if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($pyExe)) {
            return $pyExe.Trim()
        }
    }

    $python = Get-Command python -ErrorAction SilentlyContinue
    if ($null -ne $python) {
        return $python.Source
    }
    return $null
}

function Ensure-FontTools {
    $pythonCmd = Resolve-PythonCommand
    if ([string]::IsNullOrWhiteSpace($pythonCmd)) {
        Write-Warning "Python not found. Font subsetting will fallback to full font."
        return
    }

    $env:PYTHON = $pythonCmd
    & $pythonCmd -c "import fontTools" *> $null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "fontTools already available. (python: $pythonCmd)"
        return
    }

    Write-Host "fontTools not found, installing..."
    & $pythonCmd -m pip install --user fonttools
    if ($LASTEXITCODE -ne 0) {
        Write-Warning "fontTools install failed. Font subsetting will fallback to full font."
        return
    }

    & $pythonCmd -c "import fontTools" *> $null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "fontTools installed. (python: $pythonCmd)"
    } else {
        Write-Warning "fontTools verification failed. Font subsetting will fallback to full font."
    }
}

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
Ensure-FontTools

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
