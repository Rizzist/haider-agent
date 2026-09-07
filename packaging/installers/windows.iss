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

{ Inno Setup 6 runs 32-bit Setup/Uninstall even in 64-bit install mode.
  Integer is its registry root type; Cardinal is a DWORD and can carry the
  NULL data pointer (0). Kind and Size pass DWORDs by reference.
  RegQueryStringValue accepts both string kinds without reporting which one. }
function RegGetValue(RootKey: Integer; SubKey, Name: String; Flags: Cardinal;
  var Kind: Cardinal; Data: Cardinal; var Size: Cardinal): Integer;
  external 'RegGetValueW@advapi32.dll stdcall';

function PathKind: Cardinal;
var
  Size, Kind: Cardinal;
  Status: Integer;
begin
  Size := 0;
  Kind := 0;
  Result := 2;
  { RRF_RT_ANY or RRF_NOEXPAND; query type/size only, with pvData = NULL. }
  Status := RegGetValue(HKCU, 'Environment', 'Path', $1000FFFF, Kind, 0, Size);
  if Status = 2 then exit; { ERROR_FILE_NOT_FOUND: new PATH is REG_EXPAND_SZ. }
  if Status <> 0 then RaiseException('Could not query the user PATH type');
  if (Kind <> 1) and (Kind <> 2) then
    RaiseException('The user PATH must be REG_SZ or REG_EXPAND_SZ');
  Result := Kind;
end;

procedure WritePath(Value: String; Kind: Cardinal);
var Written: Boolean;
begin
  if Kind = 1 then begin
    { RegWriteStringValue preserves an existing REG_EXPAND_SZ type. Remove
      that value first only when restoring an explicitly saved REG_SZ. }
    if RegValueExists(HKCU, 'Environment', 'Path') then
      if PathKind = 2 then
        if not RegDeleteValue(HKCU, 'Environment', 'Path') then
          RaiseException('Could not restore the user PATH type');
    Written := RegWriteStringValue(HKCU, 'Environment', 'Path', Value);
  end
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
  Kind: Cardinal;
begin
  if Step <> ssPostInstall then exit;
  if not RegQueryStringValue(HKCU, OwnerKey, 'AddedPath', Original) then begin
    RegQueryStringValue(HKCU, 'Environment', 'Path', Value);
    if not PathContains(Value, ExpandConstant('{app}')) then begin
      Kind := PathKind;
      RegWriteStringValue(HKCU, OwnerKey, 'OriginalPath', Value);
      RegWriteDWordValue(HKCU, OwnerKey, 'OriginalKind', Kind);
      if RegValueExists(HKCU, 'Environment', 'Path') then
        RegWriteDWordValue(HKCU, OwnerKey, 'PathExisted', 1)
      else RegWriteDWordValue(HKCU, OwnerKey, 'PathExisted', 0);
      if Value <> '' then Value := Value + ';';
      Value := Value + ExpandConstant('{app}');
      WritePath(Value, Kind);
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
