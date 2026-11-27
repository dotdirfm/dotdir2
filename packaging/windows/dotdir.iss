[Setup]
AppId=dev.dotdir.native
AppName=DotDir Native
AppVersion={#AppVersion}
DefaultDirName={autopf}\DotDir Native
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
WizardStyle=modern
DisableProgramGroupPage=yes
SetupIconFile=dotdir.ico
UninstallDisplayIcon={app}\dotdir.ico
OutputDir=..\..\dist\installers
OutputBaseFilename=DotDir-Windows-x64-Setup
SolidCompression=yes

[Files]
Source: "..\..\dist\windows\dotdir.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "dotdir.ico"; DestDir: "{app}"

[Icons]
Name: "{autoprograms}\DotDir Native"; Filename: "{app}\dotdir.exe"; WorkingDir: "{app}"; IconFilename: "{app}\dotdir.ico"

[Run]
Filename: "{app}\dotdir.exe"; Description: "Launch DotDir Native"; Flags: nowait postinstall skipifsilent
