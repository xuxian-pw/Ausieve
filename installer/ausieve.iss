; Inno Setup script for Ausieve.
; Release builds: run installer\build.ps1 (strips local paths, then calls ISCC).
; Output: target\installer\Ausieve-setup-<version>.exe

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef BinDir
  #define BinDir "..\target\release"
#endif

#define AppName "Ausieve"
#define AppExe "Ausieve.exe"

[Setup]
AppId={{6B0E7C2A-3F4D-4E8B-9A61-2D5C8F1B7E40}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=xuxian-pw
AppPublisherURL=https://github.com/xuxian-pw/Ausieve
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
; Ausieve.exe is single-instance and holds the output device; close it before replacing files.
CloseApplications=force
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional options:"; Flags: unchecked

[Files]
Source: "{#BinDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BinDir}\capture.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[InstallDelete]
; Leftovers from before the rename (audio-exclude, tray.exe); same AppId, so this is an
; upgrade. The old tray.exe is stopped in PrepareToInstall below.
Type: files; Name: "{app}\tray.exe"
; 0.1.0 also installed the README, 0.1.1 the diagnostics script (now only on GitHub).
Type: files; Name: "{app}\README.md"
Type: files; Name: "{app}\diagnose.ps1"
Type: files; Name: "{userstartup}\audio-exclude.lnk"
Type: files; Name: "{autodesktop}\audio-exclude.lnk"
Type: files; Name: "{group}\audio-exclude.lnk"
Type: files; Name: "{group}\Uninstall audio-exclude.lnk"

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "Launch Ausieve"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Stop only the Ausieve.exe from this install dir (other copies, e.g. a dev build, keep running).
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -NonInteractive -Command ""Get-Process Ausieve -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -eq '{app}\{#AppExe}' } | Stop-Process -Force"""; Flags: runhidden waituntilterminated; RunOnceId: "StopTray"
; Settings in %APPDATA%\Ausieve are kept on uninstall.

[Code]
// Upgrades from before the rename: the running tray.exe is not one of the files being
// replaced, so CloseApplications does not stop it. Stop only the copy in this install dir.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
  Old: String;
begin
  Old := ExpandConstant('{app}\tray.exe');
  if FileExists(Old) then
    Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
      '-NoProfile -NonInteractive -Command "Get-Process tray -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq ''' + Old + ''' } | Stop-Process -Force"',
      '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;
