; Inno Setup script for the corcel Windows installer. Expects the staged
; distribution from package.ps1 at dist\corcel (relative to the repo root,
; which is two levels up from this file). Build with:
;   iscc packaging\windows\corcel.iss
; Output lands in dist\corcel-setup.exe.

#define AppName "corcel"
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

[Setup]
AppId={{7A2C63D1-5A34-4E0B-9C1D-corcelapp001}
AppName={#AppName}
AppVersion={#AppVersion}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Per-user install: no admin prompt, and friends on locked-down machines
; can still install it.
PrivilegesRequired=lowest
OutputDir=..\..\dist
OutputBaseFilename=corcel-setup
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\corcel.exe

[Files]
Source: "..\..\dist\corcel\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\corcel.exe"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\corcel.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Run]
Filename: "{app}\corcel.exe"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
