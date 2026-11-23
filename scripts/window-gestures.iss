#define ProductId "49A86264-8D40-436A-997C-1403162F988F"
#define ProductName "Window Gestures"
#define AppExecutable "window-gestures.exe"
#define RuntimeDownload "https://aka.ms/windowsappsdk/2.7/2.7.0/windowsappruntimeinstall-"
#if TargetArchitecture == "arm64"
  #define RuntimeArchitecture "arm64"
  #define RuntimeArchitectureFlag 16
  #define RuntimeHash "A29130ADB909C4CA340A30BD184CD96E216797EDDF8FAA90E16DA0BE8F74BF00"
#elif TargetArchitecture == "x64os"
  #define RuntimeArchitecture "x64"
  #define RuntimeArchitectureFlag 4
  #define RuntimeHash "ED1AC8AA9497473389A288017C7FC352C382BA8DF6C08F36DDB5815BFF283B2A"
#else
  #error Unsupported setup architecture
#endif

[Setup]
AppId={{{#ProductId}}
AppName={#ProductName}
AppVersion={#AppVersion}
AppPublisher=apricotbucket28
DefaultDirName={localappdata}\Programs\{#ProductName}
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
MinVersion=10.0.22000
ArchitecturesAllowed={#TargetArchitecture}
ArchitecturesInstallIn64BitMode={#TargetArchitecture}
CloseApplications=yes
RestartApplications=no
SetupIconFile=..\assets\window-gestures.ico
UninstallDisplayName={#ProductName}
UninstallDisplayIcon={app}\{#AppExecutable}

[Tasks]
Name: "startup"; Description: "Start {#ProductName} when I sign in"; Check: IsFirstInstall

[Files]
Source: "{#BundleDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{autoprograms}\{#ProductName}"; Filename: "{app}\{#AppExecutable}"
Name: "{userstartup}\{#ProductName}"; Filename: "{app}\{#AppExecutable}"; Tasks: startup

[Run]
Filename: "{app}\{#AppExecutable}"; Parameters: "{code:LaunchParameters}"; Description: "Launch {#ProductName}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: files; Name: "{userstartup}\{#ProductName}.lnk"

[Code]
var
  RuntimeDownloadPage: TDownloadWizardPage;
  FreshInstall: Boolean;

function TryCreatePackageDependency(User: UINT_PTR; FamilyName: String;
  MinVersion: Int64; Architectures, LifetimeKind: Cardinal; LifetimeArtifact: UINT_PTR;
  Options: Cardinal; var DependencyId: UINT_PTR): Integer;
  external 'TryCreatePackageDependency@api-ms-win-appmodel-runtime-l1-1-5.dll stdcall setuponly';

function GetProcessHeap: UINT_PTR;
  external 'GetProcessHeap@kernel32.dll stdcall setuponly';

function HeapFree(Heap: UINT_PTR; Flags: Cardinal; Address: UINT_PTR): BOOL;
  external 'HeapFree@kernel32.dll stdcall setuponly';

procedure InitializeWizard;
begin
  RuntimeDownloadPage := CreateDownloadPage('Windows App Runtime',
    'Downloading the runtime required by UI.', nil);
  RuntimeDownloadPage.ShowBaseNameInsteadOfUrl := True;
end;

function HasWindowsAppRuntime: Boolean;
var
  DependencyId: UINT_PTR;
  Status: Integer;
begin
  { Match Reactor's minimum 2.6.4.0 and accept the target architecture or Neutral (1).
    Windows releases the dependency with Setup's process (lifetime kind 0). }
  Status := TryCreatePackageDependency(0, 'Microsoft.WindowsAppRuntime.2_8wekyb3d8bbwe',
    StrToInt64('$0002000600040000'), {#RuntimeArchitectureFlag} or 1, 0, 0, 0, DependencyId);
  Result := Status = 0;
  if Result then
    HeapFree(GetProcessHeap, 0, DependencyId)
  else
    Log(Format('Windows App Runtime check returned 0x%.8x.', [Status]));
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ExitCode: Integer;
begin
  Result := '';
  if HasWindowsAppRuntime then
  begin
    Log('A compatible Windows App Runtime is already installed.');
    Exit;
  end;

  try
    RuntimeDownloadPage.Clear;
    RuntimeDownloadPage.Add('{#RuntimeDownload}{#RuntimeArchitecture}.exe',
      'WindowsAppRuntimeInstall.exe', '{#RuntimeHash}');
    RuntimeDownloadPage.Show;
    try
      RuntimeDownloadPage.Download;
    finally
      RuntimeDownloadPage.Hide;
    end;
  except
    Result := 'Could not download the Windows App Runtime. Check your connection or install the runtime from Microsoft, then run Setup again.'
      + #13#10 + GetExceptionMessage;
    Log(Result);
    Exit;
  end;

  Log('Installing Windows App Runtime with the Microsoft redistributable.');
  if not Exec(ExpandConstant('{tmp}\WindowsAppRuntimeInstall.exe'), '--quiet',
    '', SW_HIDE, ewWaitUntilTerminated, ExitCode) then
    Result := 'Could not start the Windows App Runtime installer. ' + SysErrorMessage(ExitCode)
  else if ExitCode <> 0 then
    Result := Format('Windows App Runtime installation failed (0x%.8x).', [ExitCode]);

  if Result <> '' then
    Log(Result)
  else
    Log('Windows App Runtime installer completed successfully.');
end;

function IsFirstInstall: Boolean;
begin
  Result := FreshInstall;
end;

function InitializeSetup: Boolean;
begin
  { Capture this before Setup writes the uninstall registration. }
  FreshInstall := not RegKeyExists(HKCU,
    'Software\Microsoft\Windows\CurrentVersion\Uninstall\{{#ProductId}}_is1');
  Result := True;
end;

function LaunchParameters(Param: String): String;
begin
  if FreshInstall then
    Result := '--show-welcome'
  else
    Result := '';
end;
