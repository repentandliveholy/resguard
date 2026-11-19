; -----------------------------------------------------------------------------
; resguard — Windows installer (Inno Setup 6.3+)
;
; Produces: resguard-setup-windows-x86_64.exe
;
; Per-user install (no admin) into %LOCALAPPDATA%\resguard\bin — the same
; directory install.ps1 uses. The built-in self-updater (`resguard --update`
; and daemon auto-update) replaces resguard.exe / rs-tui.exe / rs-dash.exe in
; place, so the install directory MUST be user-writable.
;
; Build (see installer/windows/build-installer.ps1, used by CI):
;   ISCC.exe /DAppVersion=0.2.0 /DSourceDir=C:\path\to\stage /DOutputDir=C:\path\to\dist resguard.iss
;
; Preprocessor defines (all optional; defaults allow a local compile):
;   AppVersion   version string, e.g. 0.2.0 (from Cargo.toml in CI)
;   SourceDir    folder containing resguard.exe, rs-tui.exe, rs-dash.exe, README.md
;   OutputDir    where the setup .exe is written
;   SetupIcon    optional .ico used for the wizard, shortcuts and Apps & Features
;   Compression  Inno Setup compression spec (default lzma2/max; CI uses zip/1)
; Relative paths are resolved against this script's directory.
;
; Requires Inno Setup 6.3 or newer (the "x64compatible" architecture identifier).
; -----------------------------------------------------------------------------

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\dist\stage"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif
#ifndef Compression
  #define Compression "lzma2/max"
#endif
; Note: "SourceDir" here is a preprocessor variable, not the [Setup] SourceDir
; directive (which is left at its default: this script's directory).

; VersionInfoVersion must be purely numeric: strip any pre-release suffix
; ("0.2.0-beta.1" -> "0.2.0").
#if Pos("-", AppVersion) > 0
  #define NumericVersion Copy(AppVersion, 1, Pos("-", AppVersion) - 1)
#else
  #define NumericVersion AppVersion
#endif

#define AppName "resguard"
#define AppPublisher "resguard"
#define AppURL "https://resguard.repentandliveholy.org"
#define RepoURL "https://github.com/repentandliveholy/resguard"
; Name of the Task Scheduler job created by `resguard --install-service`.
#define ServiceTaskName "resguard"

[Setup]
; Stable identity — never change this GUID, or upgrades/uninstall break.
AppId={{B763E6FD-F737-4349-9E7C-20E2FFCD108F}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#RepoURL}/issues
AppUpdatesURL={#RepoURL}/releases
VersionInfoVersion={#NumericVersion}
VersionInfoDescription=resguard setup

; Per-user, no elevation. The directory is fixed so the self-updater, the
; PowerShell installer and this installer all agree on one location.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\resguard\bin
DisableDirPage=yes
DisableProgramGroupPage=yes
UsePreviousAppDir=no

; 64-bit only.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible

; We add/remove our directory in the user PATH (see [Code]); tell Explorer etc.
ChangesEnvironment=yes

; Let Restart Manager close anything still holding our files. We also kill
; resguard/rs-dash/rs-tui ourselves in PrepareToInstall; never relaunch the old
; binaries afterwards.
CloseApplications=yes
RestartApplications=no

OutputDir={#OutputDir}
OutputBaseFilename=resguard-setup-windows-x86_64
Compression={#Compression}
SolidCompression=yes
WizardStyle=modern
UninstallDisplayName={#AppName}
#ifdef SetupIcon
SetupIconFile={#SetupIcon}
UninstallDisplayIcon={app}\rs-dash.ico
#else
UninstallDisplayIcon={app}\rs-dash.exe
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; Checked by default: register the per-user logon task (schtasks) and start it.
Name: "service"; Description: "Run resguard in the background at logon (recommended)"; GroupDescription: "Background service:"
Name: "desktopicon"; Description: "Create a desktop shortcut for RS Dash"; GroupDescription: "Shortcuts:"; Flags: unchecked

[InstallDelete]
; Leftovers from the self-updater (renamed running exes / aborted staging files).
Type: files; Name: "{app}\*.old"
Type: files; Name: "{app}\.*.update.*"

[Files]
Source: "{#SourceDir}\resguard.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rs-tui.exe";   DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\rs-dash.exe";  DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\README.md";    DestDir: "{app}"; Flags: ignoreversion
#ifdef SetupIcon
Source: "{#SetupIcon}"; DestDir: "{app}"; DestName: "rs-dash.ico"; Flags: ignoreversion
#endif

[Icons]
; {autoprograms}/{autodesktop} resolve to the per-user locations in lowest-privilege mode.
#ifdef SetupIcon
Name: "{autoprograms}\RS Dash"; Filename: "{app}\rs-dash.exe"; WorkingDir: "{app}"; IconFilename: "{app}\rs-dash.ico"; Comment: "RS Dash - resguard dashboard"
Name: "{autodesktop}\RS Dash";  Filename: "{app}\rs-dash.exe"; WorkingDir: "{app}"; IconFilename: "{app}\rs-dash.ico"; Comment: "RS Dash - resguard dashboard"; Tasks: desktopicon
#else
Name: "{autoprograms}\RS Dash"; Filename: "{app}\rs-dash.exe"; WorkingDir: "{app}"; Comment: "RS Dash - resguard dashboard"
Name: "{autodesktop}\RS Dash";  Filename: "{app}\rs-dash.exe"; WorkingDir: "{app}"; Comment: "RS Dash - resguard dashboard"; Tasks: desktopicon
#endif

[Run]
; 1) Undo anything a previous (force-stopped) governor left applied. Safe no-op
;    when the ledger is empty. Uses the binary we just installed.
Filename: "{app}\resguard.exe"; Parameters: "--restore"; Flags: runhidden waituntilterminated; StatusMsg: "Restoring process priorities..."
; 2) Background service: (re)register the logon task with the new binary and
;    start it — or remove it if the user unchecked the task.
Filename: "{app}\resguard.exe"; Parameters: "--install-service"; Flags: runhidden waituntilterminated; Tasks: service; StatusMsg: "Registering resguard background task..."
Filename: "{app}\resguard.exe"; Parameters: "--uninstall-service"; Flags: runhidden waituntilterminated; Tasks: not service; StatusMsg: "Removing resguard background task..."
; 3) Optional post-install checkbox.
Filename: "{app}\rs-dash.exe"; Description: "Launch RS Dash"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Order matters: stop + remove the logon task, stop any remaining governor or
; dashboard, THEN undo every recorded change (--restore must not race a live
; governor). Errors are ignored (e.g. no task registered).
Filename: "{app}\resguard.exe";    Parameters: "--uninstall-service"; Flags: runhidden waituntilterminated; RunOnceId: "UninstallService"
Filename: "{sys}\taskkill.exe";    Parameters: "/F /IM rs-dash.exe";  Flags: runhidden waituntilterminated; RunOnceId: "KillRsDash"
Filename: "{sys}\taskkill.exe";    Parameters: "/F /IM rs-tui.exe";   Flags: runhidden waituntilterminated; RunOnceId: "KillRsTui"
Filename: "{sys}\taskkill.exe";    Parameters: "/F /IM resguard.exe"; Flags: runhidden waituntilterminated; RunOnceId: "KillResguard"
Filename: "{app}\resguard.exe";    Parameters: "--restore";           Flags: runhidden waituntilterminated; RunOnceId: "RestorePriorities"

[UninstallDelete]
; Self-updater leftovers that were never part of the installed file set.
Type: files; Name: "{app}\*.old"
Type: files; Name: "{app}\.*.update.*"
; Remove the bin folder (and %LOCALAPPDATA%\resguard) only if nothing else
; lives there — config, ledger and logs are left untouched.
Type: dirifempty; Name: "{app}"
Type: dirifempty; Name: "{localappdata}\resguard"

[Code]
const
  EnvKey = 'Environment';

{ --- Process handling ------------------------------------------------------ }

procedure RunHidden(const FileName, Params: String);
var
  ResultCode: Integer;
begin
  { Failures are expected (task not registered, process not running): ignore. }
  Exec(FileName, Params, '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

procedure StopResguard();
begin
  { End the logon task first so it does not respawn anything, then make sure no
    resguard binary keeps a lock on the files we are about to replace. A
    force-stopped governor leaves its ledger behind; [Run] --restore and the
    next daemon start undo those changes. }
  RunHidden(ExpandConstant('{sys}\schtasks.exe'), '/End /TN {#ServiceTaskName}');
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM rs-dash.exe');
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM rs-tui.exe');
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM resguard.exe');
  { Give Windows a moment to release file handles. }
  Sleep(500);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  { Runs before Setup checks for files in use. }
  StopResguard();
  Result := '';
end;

{ --- User PATH (HKCU\Environment) ------------------------------------------ }

function NormalizeDir(const Dir: String): String;
begin
  Result := Uppercase(RemoveBackslashUnlessRoot(Trim(Dir)));
end;

{ Returns Paths with every entry equal to Dir removed (case-insensitive,
  trailing backslash ignored). Empty entries are dropped as well. }
function RemovePathEntry(const Paths, Dir: String): String;
var
  Rest, Entry, Target: String;
  P: Integer;
begin
  Result := '';
  Target := NormalizeDir(Dir);
  Rest := Paths;
  while Rest <> '' do
  begin
    P := Pos(';', Rest);
    if P > 0 then
    begin
      Entry := Copy(Rest, 1, P - 1);
      Delete(Rest, 1, P);
    end
    else
    begin
      Entry := Rest;
      Rest := '';
    end;
    if (Trim(Entry) <> '') and (NormalizeDir(Entry) <> Target) then
    begin
      if Result <> '' then
        Result := Result + ';';
      Result := Result + Entry;
    end;
  end;
end;

function PathHasEntry(const Paths, Dir: String): Boolean;
var
  Rest, Entry, Target: String;
  P: Integer;
begin
  Result := False;
  Target := NormalizeDir(Dir);
  Rest := Paths;
  while (Rest <> '') and (not Result) do
  begin
    P := Pos(';', Rest);
    if P > 0 then
    begin
      Entry := Copy(Rest, 1, P - 1);
      Delete(Rest, 1, P);
    end
    else
    begin
      Entry := Rest;
      Rest := '';
    end;
    Result := NormalizeDir(Entry) = Target;
  end;
end;

procedure AddToUserPath(const Dir: String);
var
  Paths: String;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', Paths) then
    Paths := '';
  if PathHasEntry(Paths, Dir) then
    exit;
  if (Paths <> '') and (Copy(Paths, Length(Paths), 1) <> ';') then
    Paths := Paths + ';';
  Paths := Paths + Dir;
  if not RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', Paths) then
    Log('Failed to add ' + Dir + ' to the user PATH');
end;

procedure RemoveFromUserPath(const Dir: String);
var
  Paths, NewPaths: String;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', Paths) then
    exit;
  if not PathHasEntry(Paths, Dir) then
    exit;
  NewPaths := RemovePathEntry(Paths, Dir);
  if NewPaths = '' then
    RegDeleteValue(HKEY_CURRENT_USER, EnvKey, 'Path')
  else
    RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', NewPaths);
end;

{ --- Setup / uninstall hooks ----------------------------------------------- }

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    AddToUserPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  { Only our own entry is removed; the rest of the user PATH is preserved. }
  if CurUninstallStep = usPostUninstall then
    RemoveFromUserPath(ExpandConstant('{app}'));
end;
