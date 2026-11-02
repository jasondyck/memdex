; Inno Setup script for memdex (per-user install, no admin rights).
;   iscc /DAppVersion=0.5.2 /DArch=x64 /DSrcDir=path\to\unzipped /DOutDir=dist packaging\windows\memdex.iss
; Arch: x64 or arm64. SrcDir must contain memdex.exe, README.md, LICENSE-MIT, LICENSE-APACHE.

#ifndef AppVersion
  #error Pass /DAppVersion=X.Y.Z
#endif
#ifndef Arch
  #define Arch "x64"
#endif
#ifndef SrcDir
  #define SrcDir "."
#endif
#ifndef OutDir
  #define OutDir "dist"
#endif

[Setup]
AppId={{60583FD2-CBB2-4BBA-94AD-7095727D2987}
AppName=memdex
AppVersion={#AppVersion}
AppVerName=memdex {#AppVersion}
AppPublisher=memdex contributors
AppPublisherURL=https://github.com/jasondyck/memdex
AppSupportURL=https://github.com/jasondyck/memdex/issues
DefaultDirName={localappdata}\Programs\memdex
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
DisableProgramGroupPage=yes
DisableDirPage=yes
ChangesEnvironment=yes
UninstallDisplayIcon={app}\memdex.exe
UninstallDisplayName=memdex {#AppVersion}
OutputDir={#OutDir}
OutputBaseFilename=memdex-{#AppVersion}-{#Arch}-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif

[Files]
Source: "{#SrcDir}\memdex.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SrcDir}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SrcDir}\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SrcDir}\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion

[Registry]
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
  ValueData: "{olddata};{app}"; Check: NeedsAddPath(ExpandConstant('{app}'))

[Messages]
FinishedLabel=memdex is installed and on your PATH (open a new terminal).%n%nRegister it with Claude Code:%n%n  claude mcp add --scope user memdex -- memdex serve

[Code]
function NeedsAddPath(Dir: string): Boolean;
var
  Paths: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') = 0;
end;

procedure RemovePath(Dir: string);
var
  Paths: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths) then
    exit;
  Paths := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Paths));
  if P = 0 then
    exit;
  Delete(Paths, P, Length(Dir) + 1);
  Paths := Copy(Paths, 2, Length(Paths) - 2);
  RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', Paths);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemovePath(ExpandConstant('{app}'));
end;
