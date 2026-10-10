; Windows installer (Inno Setup 6). Built by the release workflow:
;   iscc /DAppVersion=0.2.7 /DArch=x64 /DExe=target\release\stecak.exe installer\stecak.iss
; Installs per user (no admin prompt) into %LOCALAPPDATA%\Programs\Stecak, like VS Code's
; user installer, with a Start menu entry and optionally `stecak` on PATH.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
; x64 or arm64
#ifndef Arch
  #define Arch "x64"
#endif
#ifndef Exe
  #define Exe "target\release\stecak.exe"
#endif
; scripts/fetch-conpty.ps1 puts conpty.dll and OpenConsole.exe next to the exe.
#define ExeDir ExtractFilePath(Exe)

[Setup]
AppId={{5B7C3E52-8A0D-4C1B-9F3E-57ECAC0A1D2E}
AppName=Stećak
AppVersion={#AppVersion}
AppVerName=Stećak {#AppVersion}
AppPublisher=alminisl
AppPublisherURL=https://github.com/alminisl/stecak
AppSupportURL=https://github.com/alminisl/stecak/issues
AppUpdatesURL=https://github.com/alminisl/stecak/releases
DefaultDirName={autopf}\Stecak
DefaultGroupName=Stećak
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif
SourceDir=..
OutputDir=dist
OutputBaseFilename=stecak-v{#AppVersion}-windows-{#Arch}-setup
SetupIconFile=assets\stecak.ico
UninstallDisplayIcon={app}\stecak.exe
UninstallDisplayName=Stećak
LicenseFile=LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
; Upgrades: close a running Stećak first (it holds stecak.exe open).
CloseApplications=yes

[Tasks]
Name: "addtopath"; Description: "Add stecak to PATH (run it from any terminal)"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#Exe}"; DestDir: "{app}"; DestName: "stecak.exe"; Flags: ignoreversion
Source: "{#ExeDir}conpty.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#ExeDir}OpenConsole.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "config.example.yaml"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Stećak"; Filename: "{app}\stecak.exe"
Name: "{autodesktop}\Stećak"; Filename: "{app}\stecak.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\stecak.exe"; Description: "{cm:LaunchProgram,Stećak}"; Flags: nowait postinstall skipifsilent

[Code]
const
  EnvKey = 'Environment';

{ Add the install folder to the user's PATH (HKCU), once. }
procedure EnvAddPath(Dir: string);
var
  Paths: string;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
    Paths := '';
  if Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0 then
    exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then
    Paths := Paths + ';';
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Paths + Dir);
end;

{ Remove it again on uninstall, with its separator. }
procedure EnvRemovePath(Dir: string);
var
  Paths: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
    exit;
  { In ';' + Paths + ';' the match starts at P (the ';' before Dir), so Dir starts at Paths[P]. }
  P := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';');
  if P = 0 then
    exit;
  if P > 1 then
    Delete(Paths, P - 1, Length(Dir) + 1)
  else
    Delete(Paths, P, Length(Dir) + 1);
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Paths);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    EnvAddPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    EnvRemovePath(ExpandConstant('{app}'));
end;
