; Inno Setup script for the trax installer.
; Build: iscc /DAppVersion=0.1.0 installer\trax.iss   (after `cargo build --release`)
; Installs per user (no admin rights) to %LOCALAPPDATA%\Programs\trax. Your library and
; settings live in %APPDATA%\trax and are kept when trax is uninstalled.

#ifndef SourceExe
  #define SourceExe "..\target\release\trax.exe"
#endif

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6C1F3E0B-8E52-4C1D-9B7A-3F2A9D7C5E41}
AppName=trax
AppVersion={#AppVersion}
AppVerName=trax {#AppVersion}
AppPublisher=NUCL3ARN30N
AppCopyright=Copyright (C) NUCL3ARN30N
AppPublisherURL=https://github.com/NUCL3ARN30N/trax
AppSupportURL=https://github.com/NUCL3ARN30N/trax/issues
AppUpdatesURL=https://github.com/NUCL3ARN30N/trax/releases
DefaultDirName={localappdata}\Programs\trax
DefaultGroupName=trax
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
OutputDir=..\target\installer
OutputBaseFilename=trax-setup-{#AppVersion}
SetupIconFile=..\assets\logo\trax.ico
; GPL-3.0, shown during setup (not an agreement the user must sign: it grants rights).
InfoBeforeFile=..\LICENSE
UninstallDisplayIcon={app}\trax.exe
UninstallDisplayName=trax
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Updates run this installer while trax has just exited; close it if it is still running.
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "trax.exe"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\THIRD_PARTY_LICENSES.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\trax"; Filename: "{app}\trax.exe"
Name: "{autodesktop}\trax"; Filename: "{app}\trax.exe"; Tasks: desktopicon

[Run]
; Normal install: offer to start trax. In-app update (silent, /update=1): start the new version.
Filename: "{app}\trax.exe"; Description: "{cm:LaunchProgram,trax}"; Flags: nowait postinstall skipifsilent
Filename: "{app}\trax.exe"; Flags: nowait; Check: IsUpdate

[UninstallRun]
; Remove trax from "Open with" and the default apps (registered from Settings).
Filename: "{app}\trax.exe"; Parameters: "--unregister"; RunOnceId: "UnregisterPlayer"; Flags: runhidden waituntilterminated

[Code]
function IsUpdate: Boolean;
begin
  Result := ExpandConstant('{param:update|0}') = '1';
end;
