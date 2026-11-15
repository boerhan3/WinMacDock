; WinMacDock installer (Inno Setup 6). Build with scripts\build-installer.ps1.
; Installs per user (no administrator rights) into %LOCALAPPDATA%\Programs.

#define AppName "WinMacDock"
#ifndef AppVersion
  #define AppVersion "0.2.0"
#endif
#define AppExe "winmacdock.exe"

[Setup]
AppId={{6E1F2A9C-4B7D-4C55-9C2E-8E7B3D1A5F20}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppName}
AppComments=A light macOS-style desktop for Windows 11: Dock, menu bar, Launchpad, Spotlight and live wallpaper.
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=WinMacDock-Setup-{#AppVersion}
SetupIconFile=..\assets\app.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
CloseApplications=no

[Tasks]
Name: "autostart"; Description: "Start {#AppName} when I sign in to Windows"

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autoprograms}\{#AppName} Settings"; Filename: "{app}\{#AppExe}"; Parameters: "--settings"
Name: "{autoprograms}\{#AppName} - Restore Windows Taskbar"; Filename: "{app}\{#AppExe}"; Parameters: "--restore"

[Registry]
; Same value the app's own "Start at sign-in" switch uses.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#AppExe}"; Description: "Launch {#AppName} now"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Quit the running copy, then make sure the Windows taskbar is back.
Filename: "{app}\{#AppExe}"; Parameters: "--quit"; Flags: runhidden waituntilterminated; RunOnceId: "QuitApp"
Filename: "{cmd}"; Parameters: "/c timeout /t 2 /nobreak >nul"; Flags: runhidden waituntilterminated; RunOnceId: "WaitQuit"
Filename: "{app}\{#AppExe}"; Parameters: "--restore"; Flags: runhidden waituntilterminated; RunOnceId: "RestoreTaskbar"

[UninstallDelete]
Type: dirifempty; Name: "{app}"

[Code]
// An update cannot replace the exe while it runs: ask the running copy to quit first.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Result := '';
  if FileExists(ExpandConstant('{app}\{#AppExe}')) then
  begin
    Exec(ExpandConstant('{app}\{#AppExe}'), '--quit', '', SW_HIDE, ewWaitUntilTerminated, Code);
    Sleep(2000);
  end;
end;
