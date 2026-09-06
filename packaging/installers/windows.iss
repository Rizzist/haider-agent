#include "generated.iss"

[Setup]
AppId=HaiderHarness
AppName=Haider
AppVersion={#ReleaseVersion}
AppPublisher=Haider
DefaultDirName={localappdata}\Programs\Haider
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
ChangesEnvironment=yes
Uninstallable=yes
CreateUninstallRegKey=yes
UninstallDisplayName=Haider
OutputDir={#OutputPath}
OutputBaseFilename=haider-v{#ReleaseVersion}-{#ReleaseTarget}-setup
Compression=lzma2
SolidCompression=yes
CloseApplications=no
RestartApplications=no
#ifdef SignEnabled
SignTool=haider
SignedUninstaller=yes
#endif

[Files]
#include "members.iss"

[Code]
const
  OwnerKey = 'Software\HaiderInstaller';

function RegGetValue(RootKey: HKEY; SubKey, Name: String; Flags: Cardinal;
  var Kind: Cardinal; Data: Cardinal; var Size: Cardinal): Integer;
  external 'RegGetValueW@advapi32.dll stdcall';

function PathKind: Cardinal;
var Size: Cardinal;
begin
  Size := 0;
  Result := 2;
  RegGetValue(HKCU, 'Environment', 'Path', $1000FFFF, Result, 0, Size);
end;

procedure WritePath(Value: String; Kind: Cardinal);
var Written: Boolean;
begin
  if Kind = 1 then Written := RegWriteStringValue(HKCU, 'Environment', 'Path', Value)
  else Written := RegWriteExpandStringValue(HKCU, 'Environment', 'Path', Value);
  if not Written then RaiseException('Could not update the user PATH');
end;

function PathContains(Value, Entry: String): Boolean;
begin
  Result := Pos(';' + Lowercase(Entry) + ';', ';' + Lowercase(Value) + ';') > 0;
end;

procedure CurStepChanged(Step: TSetupStep);
var
  Value, Original: String;
begin
  if Step <> ssPostInstall then exit;
  if not RegQueryStringValue(HKCU, OwnerKey, 'AddedPath', Original) then begin
    RegQueryStringValue(HKCU, 'Environment', 'Path', Value);
    if not PathContains(Value, ExpandConstant('{app}')) then begin
      RegWriteStringValue(HKCU, OwnerKey, 'OriginalPath', Value);
      RegWriteDWordValue(HKCU, OwnerKey, 'OriginalKind', PathKind);
      if RegValueExists(HKCU, 'Environment', 'Path') then
        RegWriteDWordValue(HKCU, OwnerKey, 'PathExisted', 1)
      else RegWriteDWordValue(HKCU, OwnerKey, 'PathExisted', 0);
      Original := Value;
      if Value <> '' then Value := Value + ';';
      Value := Value + ExpandConstant('{app}');
      WritePath(Value, PathKind);
      RegWriteStringValue(HKCU, OwnerKey, 'AddedPath', ExpandConstant('{app}'));
      RegWriteStringValue(HKCU, OwnerKey, 'WrittenPath', Value);
    end;
  end;
end;

procedure CurUninstallStepChanged(Step: TUninstallStep);
var
  Value, Added, Written, Original: String;
  Separator: Integer;
  Existed, Kind: Cardinal;
begin
  if Step = usPostUninstall then begin
    { Silent/CI uninstall always preserves state; interactive removal asks. }
    if not UninstallSilent then
      if DirExists(GetEnv('USERPROFILE') + '\.haider') then
        if MsgBox('Delete your Haider state, sessions and settings in ' +
          GetEnv('USERPROFILE') + '\.haider' + '? This cannot be undone.',
          mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
          DelTree(GetEnv('USERPROFILE') + '\.haider', True, True, True);
    exit;
  end;
  { ChangesEnvironment broadcasts during native uninstall, before usPostUninstall. }
  if Step <> usUninstall then exit;
  if RegQueryStringValue(HKCU, OwnerKey, 'AddedPath', Added) then begin
    RegQueryStringValue(HKCU, 'Environment', 'Path', Value);
    RegQueryStringValue(HKCU, OwnerKey, 'WrittenPath', Written);
    RegQueryStringValue(HKCU, OwnerKey, 'OriginalPath', Original);
    RegQueryDWordValue(HKCU, OwnerKey, 'PathExisted', Existed);
    RegQueryDWordValue(HKCU, OwnerKey, 'OriginalKind', Kind);
    if Value = Written then begin
      if Existed = 0 then RegDeleteValue(HKCU, 'Environment', 'Path')
      else WritePath(Original, Kind);
    end else begin
      { Remove only the owned token; preserve all other separators and entries. }
      Separator := Pos(';' + Lowercase(Added) + ';', ';' + Lowercase(Value) + ';');
      if Separator = 1 then begin
        Delete(Value, 1, Length(Added));
        if Copy(Value, 1, 1) = ';' then Delete(Value, 1, 1);
      end else if Separator > 1 then
        Delete(Value, Separator - 1, Length(Added) + 1);
      WritePath(Value, PathKind);
    end;
  end;
  RegDeleteKeyIncludingSubkeys(HKCU, OwnerKey);
end;
