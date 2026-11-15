# Builds the release exe and the installer: dist\WinMacDock-Setup-<version>.exe
# (Not using ErrorActionPreference=Stop: cargo reports progress on stderr.)
$root = Split-Path $PSScriptRoot -Parent
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

# Keep personal folder names (C:\Users\<name>\...) out of the release exe.
$env:RUSTFLAGS = "--remap-path-prefix=$env:USERPROFILE\.cargo\registry\src=crates --remap-path-prefix=$root=winmacdock"

# Optional code signing: set SIGN_PFX (and SIGN_PFX_PASSWORD) to a code-signing
# certificate. Signed files are far less likely to be flagged by antivirus.
function Sign-File($path) {
    if (-not $env:SIGN_PFX) { return }
    $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue | Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $signtool) { Write-Warning 'signtool.exe not found (install the Windows SDK); not signing'; return }
    $pwArgs = @(); if ($env:SIGN_PFX_PASSWORD) { $pwArgs = @('/p', $env:SIGN_PFX_PASSWORD) }
    & $signtool.FullName sign /f $env:SIGN_PFX @pwArgs /fd SHA256 /tr http://timestamp.digicert.com /td SHA256 /d WinMacDock $path
    if ($LASTEXITCODE -ne 0) { throw "signing $path failed" }
}

Push-Location $root
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    Sign-File 'target\release\winmacdock.exe'

    $version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
    $iscc = @(
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $iscc) { throw 'Inno Setup 6 not found. Install it with: winget install JRSoftware.InnoSetup' }

    & $iscc "/DAppVersion=$version" installer\winmacdock.iss
    if ($LASTEXITCODE -ne 0) { throw 'ISCC failed' }
    Sign-File "dist\WinMacDock-Setup-$version.exe"
    Get-Item "dist\WinMacDock-Setup-$version.exe"
}
finally {
    Pop-Location
}
