# Builds a portable .zip, and an NSIS installer when makensis is available.
# Windows half of M8.
#
# Code signing is deliberately NOT here. It needs an Authenticode certificate this project
# does not have, and a script that silently emits an unsigned installer while looking like
# it signed one is worse than a script that says it cannot. SmartScreen will warn on this
# installer on every machine but the one that built it.
$ErrorActionPreference = 'Stop'

$Root    = Split-Path -Parent $PSScriptRoot
$Bin     = Join-Path $Root 'target\release\medatat-ui.exe'
$Dist    = Join-Path $Root 'dist'
$Version = (Select-String -Path (Join-Path $Root 'crates\medatat-ui\Cargo.toml') `
             -Pattern '^version' | Select-Object -First 1).Line.Split('"')[1]

if (-not (Test-Path $Bin)) {
  Write-Error "$Bin not built. Run: cargo build --release -p medatat-ui"
}

New-Item -ItemType Directory -Force -Path $Dist | Out-Null
$Stage = Join-Path $Dist 'medatat-portable'
Remove-Item -Recurse -Force $Stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $Stage | Out-Null
Copy-Item $Bin (Join-Path $Stage 'medatat.exe')

$Zip = Join-Path $Dist "medatat-$Version-x86_64-windows.zip"
Remove-Item -Force $Zip -ErrorAction SilentlyContinue
Compress-Archive -Path (Join-Path $Stage '*') -DestinationPath $Zip
Write-Host "built $Zip ($([math]::Round((Get-Item $Zip).Length / 1MB, 1)) MB)"

# The MSVC runtime is the dependency most likely to be missing on a clean machine, and
# the failure is a dialog about a missing DLL rather than anything actionable. Report
# what the binary actually imports so the answer is in the build log, not in a bug report.
$dumpbin = Get-Command dumpbin -ErrorAction SilentlyContinue
if ($dumpbin) {
  $imports = & dumpbin /dependents $Bin |
             Select-String -Pattern '^\s+\S+\.dll' |
             ForEach-Object { $_.Line.Trim() }
  Write-Host "imports: $($imports -join ', ')"
} else {
  Write-Host "imports: dumpbin not on PATH, DLL dependencies not reported"
}

# ------------------------------------------------------------------------ installer
$makensis = Get-Command makensis -ErrorAction SilentlyContinue
if (-not $makensis) {
  Write-Host 'installer: skipped (makensis not found; install NSIS to build one)'
  exit 0
}

$Nsi = Join-Path $Dist 'medatat.nsi'
@"
Unicode true
Name "medatat"
OutFile "$Dist\medatat-$Version-setup.exe"
InstallDir "`$LOCALAPPDATA\medatat"
; Per-user, so the installer needs no elevation. A data-entry tool has no reason to
; write outside the user's profile, and asking for admin trains people to grant it.
RequestExecutionLevel user
ShowInstDetails show

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Section "Install"
  SetOutPath "`$INSTDIR"
  File "$Stage\medatat.exe"
  CreateShortcut "`$SMPROGRAMS\medatat.lnk" "`$INSTDIR\medatat.exe"
  WriteUninstaller "`$INSTDIR\uninstall.exe"
  ; One line each: NSIS has no line-continuation character, and a trailing backslash
  ; is a syntax error rather than a wrap.
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\medatat" "DisplayName" "medatat"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\medatat" "DisplayVersion" "$Version"
  WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\medatat" "UninstallString" "`$INSTDIR\uninstall.exe"
SectionEnd

Section "Uninstall"
  ; The local store lives in the user's data directory and is NOT removed here. It can
  ; hold unsynced edits, and an uninstaller that deletes a week of unsynced clinical work
  ; without asking is indistinguishable from data loss.
  Delete "`$INSTDIR\medatat.exe"
  Delete "`$INSTDIR\uninstall.exe"
  Delete "`$SMPROGRAMS\medatat.lnk"
  RMDir "`$INSTDIR"
  DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\medatat"
SectionEnd
"@ | Set-Content -Encoding UTF8 $Nsi

& makensis $Nsi | Out-Null
if ($LASTEXITCODE -ne 0) { Write-Error "makensis failed with exit $LASTEXITCODE" }

$Setup = Join-Path $Dist "medatat-$Version-setup.exe"
Write-Host "built $Setup ($([math]::Round((Get-Item $Setup).Length / 1MB, 1)) MB)"
Write-Host 'signed: NO - needs an Authenticode certificate.'
Write-Host '  SmartScreen will warn on any machine but the one that built it.'
Write-Host "  To sign: signtool sign /fd SHA256 /tr <timestamp-url> /td SHA256 /a $Setup"
