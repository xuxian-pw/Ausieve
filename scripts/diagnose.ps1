# Collects what is needed to diagnose Ausieve on another machine into one text file.
# Usage: powershell -ExecutionPolicy Bypass -File diagnose.ps1 [-OutFile PATH] [-Seconds N]
# Output: Ausieve-diagnose-<time>.txt on the desktop (or -OutFile).
#
# Read-only except for one short capture.exe run (only when Ausieve.exe is not running,
# because the output device is opened exclusively). It never stops Ausieve.exe.
# The user profile path is replaced with %USERPROFILE%; from the RustDesk config only the
# audio-input line is read (the file also holds password data).
param(
    [string]$OutFile = '',
    [int]$Seconds = 5
)
$ErrorActionPreference = 'Continue'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
if (-not $OutFile) {
    $OutFile = Join-Path ([Environment]::GetFolderPath('Desktop')) "Ausieve-diagnose-$stamp.txt"
}
$lines = New-Object System.Collections.Generic.List[string]
# capture.exe writes UTF-8 when piped; without this, non-ASCII device names are decoded
# with the OEM code page (e.g. GBK) and come out garbled.
[Console]::OutputEncoding = [Text.Encoding]::UTF8

function Hide-Profile([string]$s) {
    if ($env:USERPROFILE) {
        $s = $s -ireplace [regex]::Escape($env:USERPROFILE), '%USERPROFILE%'
        # JSON escapes backslashes (config.json)
        $s = $s -ireplace [regex]::Escape($env:USERPROFILE.Replace('\', '\\')), '%USERPROFILE%'
    }
    $s
}
function Add-Line([string]$s = '') { $lines.Add((Hide-Profile $s)) }
function Add-Section([string]$title) { Add-Line ''; Add-Line "=== $title ===" }
function Add-Block($obj) {
    ($obj | Out-String -Width 200).TrimEnd() -split "`r?`n" | ForEach-Object { Add-Line $_ }
}
function Invoke-Safely([string]$what, [scriptblock]$body) {
    try { & $body } catch { Add-Line "  ($what failed: $($_.Exception.Message))" }
}

Write-Host 'Collecting Ausieve diagnostics...'
Add-Line "Ausieve diagnostics  $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss zzz')"

# --- System -------------------------------------------------------------------
Add-Section 'System'
Invoke-Safely 'OS query' {
    $os = Get-CimInstance Win32_OperatingSystem
    $ubr = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -ErrorAction SilentlyContinue).UBR
    $build = [int]$os.BuildNumber
    Add-Line "OS:            $($os.Caption)"
    Add-Line "Version:       $($os.Version) (build $build.$ubr)"
    Add-Line "Architecture:  $($os.OSArchitecture)"
    $ok = ($build -ge 19041) -and ($os.OSArchitecture -match '64')
    Add-Line "Requirement:   build >= 19041, 64-bit -> $(if ($ok) { 'OK' } else { 'NOT MET' })"
    Add-Line "PowerShell:    $($PSVersionTable.PSVersion)"
}

# --- Ausieve files and processes ------------------------------------------------
Add-Section 'Ausieve install'
$dirs = @(
    $PSScriptRoot,
    (Join-Path $env:LOCALAPPDATA 'Programs\Ausieve'),
    (Join-Path $env:ProgramFiles 'Ausieve'),
    # Installs from before the rename (audio-exclude, tray.exe)
    (Join-Path $env:LOCALAPPDATA 'Programs\audio-exclude'),
    (Join-Path $env:ProgramFiles 'audio-exclude'),
    (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release')
) | Where-Object { $_ } | Select-Object -Unique
$capture = $null
foreach ($d in $dirs) {
    foreach ($name in 'Ausieve.exe', 'tray.exe', 'capture.exe') {
        $f = Join-Path $d $name
        if (Test-Path $f) {
            $i = Get-Item $f
            Add-Line ("{0}  {1:N0} bytes  {2:yyyy-MM-dd HH:mm}" -f $i.FullName, $i.Length, $i.LastWriteTime)
            if ($name -eq 'capture.exe' -and -not $capture) { $capture = $i.FullName }
        }
    }
}
if (-not $capture) { Add-Line ('capture.exe not found (searched: ' + ($dirs -join '; ') + ')') }

$running = @(Get-Process Ausieve -ErrorAction SilentlyContinue)
if ($running.Count) {
    foreach ($p in $running) { Add-Line "Running: Ausieve.exe pid $($p.Id)  $($p.Path)  started $($p.StartTime)" }
} else {
    Add-Line 'Running: Ausieve.exe is not running'
}
$old = @(Get-Process tray -ErrorAction SilentlyContinue | Where-Object { $_.Path -match 'audio-exclude' })
foreach ($p in $old) { Add-Line "Running: old tray.exe (audio-exclude) pid $($p.Id)  $($p.Path)" }

# --- Config ---------------------------------------------------------------------
Add-Section 'Config'
$cfgPath = if ($env:AUSIEVE_CONFIG) { $env:AUSIEVE_CONFIG } else { Join-Path $env:APPDATA 'Ausieve\config.json' }
$legacy = Join-Path $env:APPDATA 'audio-exclude\config.json'
# The old tray.exe only knows its own path; use it when that is what is running.
if ($old.Count -and -not $running.Count -and -not $env:AUSIEVE_CONFIG -and (Test-Path $legacy)) { $cfgPath = $legacy }
Add-Line "Path: $cfgPath"
$cfg = $null
if (Test-Path $cfgPath) {
    $raw = Get-Content -Raw -Encoding UTF8 $cfgPath
    Add-Block $raw
    try { $cfg = $raw | ConvertFrom-Json } catch { Add-Line "  (not valid JSON: $($_.Exception.Message))" }
} else {
    Add-Line '(no config file: defaults are used)'
}
if (Test-Path "$cfgPath.bad") { Add-Line "Found $cfgPath.bad (a broken config was set aside)" }
if ($cfg -and $cfg.selected) {
    $sel = [string]$cfg.selected
    $name = [IO.Path]::GetFileNameWithoutExtension($sel)
    $procs = @(Get-Process -Name $name -ErrorAction SilentlyContinue)
    $same = @($procs | Where-Object { $_.Path -and ($_.Path -ieq $sel) })
    Add-Line "Selected program exists on disk: $(Test-Path $sel)"
    Add-Line "Processes named $name.exe: $($procs.Count); with exactly this path: $($same.Count)"
    foreach ($p in $procs) { if ($p.Path -and ($p.Path -ine $sel)) { Add-Line "  other path: $($p.Path)" } }
}

# --- Audio ----------------------------------------------------------------------
Add-Section 'Audio services'
Invoke-Safely 'service query' {
    Add-Block (Get-Service Audiosrv, AudioEndpointBuilder | Select-Object Name, Status, StartType)
}

Add-Section 'Audio endpoints (PnP, includes disabled ones)'
Invoke-Safely 'PnP query' {
    Add-Block (Get-PnpDevice -Class AudioEndpoint -ErrorAction Stop |
        Sort-Object FriendlyName | Select-Object Status, FriendlyName)
}

Add-Section 'Sound drivers'
Invoke-Safely 'driver query' {
    Add-Block (Get-CimInstance Win32_SoundDevice | Select-Object Status, Name, Manufacturer)
}

Add-Section 'capture.exe --list-devices'
if ($capture) {
    Invoke-Safely 'list-devices' { Add-Block (& $capture --list-devices 2>&1) }
}

# --- Capture test ---------------------------------------------------------------
Add-Section "Capture test ($Seconds s)"
$output = if ($cfg -and $cfg.output) { [string]$cfg.output } else { 'Line 1 (Virtual Audio Cable)' }
if (-not $capture) {
    Add-Line 'Skipped: capture.exe not found'
} elseif ($running.Count -or $old.Count) {
    Add-Line 'Skipped: Ausieve.exe (or the old tray.exe) is running and holds the output device.'
    Add-Line 'To include this test, quit it from its tray menu and run the script again.'
} else {
    $capArgs = @('--output', "`"$output`"")
    if ($cfg -and $cfg.selected) { $capArgs += @('--exclude-path', "`"$($cfg.selected)`"") }
    if ($cfg -and $cfg.buffer_ms) { $capArgs += @('--buffer-ms', [string]$cfg.buffer_ms) }
    Add-Line "Command: capture.exe $($capArgs -join ' ')"
    $tmp = Join-Path $env:TEMP "ausieve-diag-$stamp"
    Invoke-Safely 'capture run' {
        $p = Start-Process -FilePath $capture -ArgumentList $capArgs -NoNewWindow -PassThru `
            -RedirectStandardOutput "$tmp.out" -RedirectStandardError "$tmp.err"
        $null = $p.Handle  # PowerShell 5.1 reports no ExitCode unless the handle is opened early
        $exited = $p.WaitForExit($Seconds * 1000)
        if (-not $exited) { Stop-Process -Id $p.Id -Force; $p.WaitForExit() }
        $out = Get-Content "$tmp.out" -Encoding UTF8 -ErrorAction SilentlyContinue
        $err = Get-Content "$tmp.err" -Encoding UTF8 -ErrorAction SilentlyContinue
        Add-Line 'stdout:'; $out | ForEach-Object { Add-Line "  $_" }
        Add-Line 'stderr:'; $err | ForEach-Object { Add-Line "  $_" }
        if ($exited) {
            Add-Line "Result: FAILED, exited by itself with code $($p.ExitCode)"
        } elseif ($out -contains 'READY') {
            Add-Line "Result: OK, ran for $Seconds s (stopped by this script)"
        } else {
            Add-Line "Result: no READY within $Seconds s"
        }
    }
    Remove-Item "$tmp.out", "$tmp.err" -ErrorAction SilentlyContinue
}

# --- RustDesk -------------------------------------------------------------------
Add-Section 'RustDesk'
Invoke-Safely 'process query' {
    $rd = @(Get-Process rustdesk -ErrorAction SilentlyContinue)
    Add-Line "rustdesk.exe processes: $($rd.Count)"
    $svc = Get-Service RustDesk -ErrorAction SilentlyContinue
    if ($svc) { Add-Line "RustDesk service: $($svc.Status)" }
}
$rdFiles = @(
    (Join-Path $env:APPDATA 'RustDesk\config\RustDesk2.toml'),
    'C:\Windows\ServiceProfiles\LocalService\AppData\Roaming\RustDesk\config\RustDesk2.toml'
)
foreach ($f in $rdFiles) {
    try {
        if (Test-Path $f -ErrorAction Stop) {
            $m = Select-String -Path $f -Pattern '^\s*audio-input\s*=' -ErrorAction Stop | Select-Object -First 1
            Add-Line "$f"
            Add-Line ('  ' + $(if ($m) { $m.Line.Trim() } else { 'audio-input not set (RustDesk uses System sound)' }))
        }
    } catch {
        Add-Line "$f"
        Add-Line "  (cannot read: $($_.Exception.Message); run as administrator to include it)"
    }
}

# --- Write ----------------------------------------------------------------------
$lines | Out-File -FilePath $OutFile -Encoding utf8
Write-Host "Saved: $OutFile"
# Default location (e.g. started from the Start menu shortcut): show the file.
if (-not $PSBoundParameters.ContainsKey('OutFile')) { explorer.exe "/select,`"$OutFile`"" }
