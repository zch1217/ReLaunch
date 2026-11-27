Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot '..')
Push-Location $repoRoot
try {
    Write-Host '==> Building release binary...'
    cargo build --release
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release failed with exit code $LASTEXITCODE"
    }

    $distDir = Join-Path $repoRoot 'dist'
    if (-not (Test-Path $distDir)) {
        New-Item -ItemType Directory -Path $distDir | Out-Null
    }

    $releaseExe = Join-Path $repoRoot 'target\release\relaunch.exe'
    $portableExe = Join-Path $distDir 'ReLaunch.exe'
    Copy-Item -LiteralPath $releaseExe -Destination $portableExe -Force

    $isccCandidates = @(
        'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
        'C:\Program Files\Inno Setup 6\ISCC.exe'
    )
    $iscc = $isccCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $iscc) {
        $cmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
        if ($cmd) {
            $iscc = $cmd.Source
        }
    }
    if (-not $iscc) {
        throw 'Inno Setup 6 (ISCC.exe) not found.'
    }

    Write-Host "==> Compiling installer via $iscc..."
    & $iscc (Join-Path $repoRoot 'installer\relaunch.iss')
    if ($LASTEXITCODE -ne 0) {
        throw "ISCC.exe failed with exit code $LASTEXITCODE"
    }

    Write-Host '==> Packaging complete:'
    Get-ChildItem -LiteralPath $distDir -File | ForEach-Object {
        $mb = [math]::Round($_.Length / 1MB, 2)
        Write-Host ("    {0} ({1} MB / {2} bytes)" -f $_.FullName, $mb, $_.Length)
    }
} finally {
    Pop-Location
}
