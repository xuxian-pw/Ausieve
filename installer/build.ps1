# Builds release binaries with local paths stripped, then the installer.
# Usage: powershell -File installer\build.ps1
# Output: target\installer\Ausieve-setup-<version>.exe
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value

# Panic messages of dependencies embed absolute source paths; replace the parts
# that contain the builder's user name.
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
# Separate target dir: keeps the dev build's cache and works while a dev Ausieve.exe is running.
$env:CARGO_TARGET_DIR = Join-Path $root 'target\dist'
$env:RUSTFLAGS ="--remap-path-prefix=$cargoHome=cargo --remap-path-prefix=$root=ausieve"

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

foreach ($exe in 'Ausieve.exe', 'capture.exe') {
    $bytes = [IO.File]::ReadAllBytes((Join-Path $root "target\dist\release\$exe"))
    $text = [Text.Encoding]::ASCII.GetString($bytes)
    if ($text.IndexOf($env:USERPROFILE, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        throw "$exe still contains $env:USERPROFILE"
    }
}

$iscc = @(
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw "Inno Setup 6 (ISCC.exe) not found" }

& $iscc /Q "/DAppVersion=$version" "/DBinDir=..\target\dist\release" installer\ausieve.iss
if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
Write-Host "target\installer\Ausieve-setup-$version.exe"
