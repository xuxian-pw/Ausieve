; Inno Setup script for audio-exclude.
; Release builds: run installer\build.ps1 (strips local paths, then calls ISCC).
; Output: target\installer\audio-exclude-setup-<version>.exe

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif

#define AppName "audio-exclude"
#define AppExe "tray.exe"

[Setup]
AppId={{6B0E7C2A-3F4D-4E8B-9A61-2D5C8F1B7E40}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=xuxian-pw
AppPublisherURL=https://github.com/xuxian-pw/Ausk
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Per-user install by default (no admin); the user can still pick all-users.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Process Loopback API needs Windows 10 2004 (build 19041).
MinVersion=10.0.19041
LicenseFile=..\LICENSE
SetupIconFile=..\assets\app.ico
UninstallDisplayIcon={app}\{#AppExe}
OutputDir=..\target\installer
OutputBaseFilename={#AppName}-setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
; tray.exe is single-instance and holds the output device; close it before replacing files.
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "autostart"; Description: "Start audio-exclude when I sign in"; GroupDescription: "Additional options:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional options:"; Flags: unchecked

[Files]
Source: "{#BinDir}\tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\capture.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon
Name: "{userstartup}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: autostart

[Run]
Filename: "{app}\{#AppExe}"; Description: "Launch audio-exclude"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Stop only the tray.exe from this install dir (other copies, e.g. a dev build, keep running).
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -NonInteractive -Command ""Get-Process tray -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -eq '{app}\{#AppExe}' } | Stop-Process -Force"""; Flags: runhidden waituntilterminated; RunOnceId: "StopTray"
; Settings in %APPDATA%\audio-exclude are kept on uninstall.
