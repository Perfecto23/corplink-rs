Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$runTests = $false
foreach ($arg in $args) {
    switch ($arg) {
        "--test" { $runTests = $true }
        "-h" { Write-Host "usage: libwg/build.ps1 [--test]"; exit 0 }
        "--help" { Write-Host "usage: libwg/build.ps1 [--test]"; exit 0 }
        default { throw "usage: libwg/build.ps1 [--test]" }
    }
}

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
$scriptDir = (Resolve-Path $scriptDir).Path
$repoRoot = (Resolve-Path (Join-Path $scriptDir "..")).Path
$patchDir = Join-Path $scriptDir "patches"
$sourceFromEnv = [Environment]::GetEnvironmentVariable("CORPLINK_WG_SOURCE")
$sourceSpec = if ([string]::IsNullOrWhiteSpace($sourceFromEnv)) {
    Join-Path $scriptDir "wireguard-go"
} else {
    $sourceFromEnv
}

foreach ($required in @("git", "tar", "go", "make")) {
    if (-not (Get-Command $required -ErrorAction SilentlyContinue)) {
        throw "corplink libwg build: required command missing: $required"
    }
}

$sourceIsDefault = [string]::IsNullOrWhiteSpace($sourceFromEnv)
$tempRoot = Join-Path ([IO.Path]::GetTempPath()) ("corplink-wg-build-" + [Guid]::NewGuid().ToString("N"))
$installStage = Join-Path $scriptDir (".corplink-wg-install-" + [Guid]::NewGuid().ToString("N"))
$archiveBackedUp = $false
$headerBackedUp = $false
$archiveInstalled = $false
$headerInstalled = $false
$oldArchive = Join-Path $installStage "old-libwg.a"
$oldHeader = Join-Path $installStage "old-libwg.h"
$utf8NoBom = New-Object System.Text.UTF8Encoding -ArgumentList $false

function Restore-OldArtifacts {
    if ($script:archiveBackedUp -or $script:archiveInstalled) {
        if (Test-Path (Join-Path $script:scriptDir "libwg.a")) {
            Remove-Item -Force (Join-Path $script:scriptDir "libwg.a")
        }
        if (Test-Path $script:oldArchive) {
            Move-Item -Force $script:oldArchive (Join-Path $script:scriptDir "libwg.a")
        }
    }
    if ($script:headerBackedUp -or $script:headerInstalled) {
        if (Test-Path (Join-Path $script:scriptDir "libwg.h")) {
            Remove-Item -Force (Join-Path $script:scriptDir "libwg.h")
        }
        if (Test-Path $script:oldHeader) {
            Move-Item -Force $script:oldHeader (Join-Path $script:scriptDir "libwg.h")
        }
    }
}

try {
    New-Item -ItemType Directory -Force $tempRoot | Out-Null
    if (-not (Test-Path $sourceSpec -PathType Container)) {
        if ($sourceIsDefault) {
            & git -C $repoRoot submodule update --init --recursive -- libwg/wireguard-go
            if ($LASTEXITCODE -ne 0) { throw "failed to initialize the fixed wireguard-go submodule" }
        } else {
            throw "CORPLINK_WG_SOURCE must point at an existing local Git checkout: $sourceSpec"
        }
    }

    $expectedSource = (Resolve-Path $sourceSpec).Path
    $sourceRoot = (& git -C $sourceSpec rev-parse --show-toplevel 2>$null | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($sourceRoot) -or
        (Resolve-Path $sourceRoot).Path -ne $expectedSource) {
        if ($sourceIsDefault) {
            & git -C $repoRoot submodule update --init --recursive -- libwg/wireguard-go
            if ($LASTEXITCODE -ne 0) { throw "failed to initialize the fixed wireguard-go submodule" }
            $sourceRoot = (& git -C $sourceSpec rev-parse --show-toplevel).Trim()
        }
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($sourceRoot) -or
            (Resolve-Path $sourceRoot).Path -ne $expectedSource) {
            throw "source is not an initialized Git checkout rooted at $sourceSpec"
        }
    }
    & git -C $sourceRoot diff --quiet
    if ($LASTEXITCODE -ne 0) { throw "source checkout has tracked changes: $sourceRoot" }
    & git -C $sourceRoot diff --cached --quiet
    if ($LASTEXITCODE -ne 0) { throw "source checkout has staged changes: $sourceRoot" }

    $sourceRevision = (& git -C $sourceRoot rev-parse HEAD).Trim()
    $sourceVersion = (& git -C $sourceRoot describe --tags --always $sourceRevision 2>$null).Trim()
    if ([string]::IsNullOrWhiteSpace($sourceVersion)) {
        $sourceVersion = "source-" + $sourceRevision.Substring(0, 12)
    }
    $sourceVersion = $sourceVersion.Replace('\', '\\').Replace('"', '\"')

    $patchedRoot = Join-Path $tempRoot "patched-source"
    New-Item -ItemType Directory -Force $patchedRoot | Out-Null
    $archivePath = Join-Path $tempRoot "source.tar"
    & git -C $sourceRoot archive --format=tar --output=$archivePath $sourceRevision
    if ($LASTEXITCODE -ne 0) { throw "failed to archive source revision $sourceRevision" }
    & tar -xf $archivePath -C $patchedRoot
    if ($LASTEXITCODE -ne 0) { throw "failed to extract source archive" }

    $versionText = "package main`n`nconst Version = `"$sourceVersion`"`n"
    [IO.File]::WriteAllText((Join-Path $patchedRoot "version.go"), $versionText, $utf8NoBom)
    $patchedLibwg = Join-Path $patchedRoot "libwg"
    New-Item -ItemType Directory -Force $patchedLibwg | Out-Null
    [IO.File]::WriteAllText((Join-Path $patchedLibwg "version.go"), $versionText, $utf8NoBom)

    $patches = @(Get-ChildItem -LiteralPath $patchDir -Filter "*.patch" -File | Sort-Object Name)
    foreach ($patch in $patches) {
        & git -C $patchedRoot apply --check $patch.FullName
        if ($LASTEXITCODE -ne 0) { throw "patch check failed: $($patch.Name)" }
        & git -C $patchedRoot apply $patch.FullName
        if ($LASTEXITCODE -ne 0) { throw "patch apply failed: $($patch.Name)" }
    }

    Push-Location $patchedRoot
    try {
        if ($runTests) {
            & go test ./libwg ./corplink ./conn ./tun/netstack
            if ($LASTEXITCODE -ne 0) { throw "patched wireguard-go tests failed" }
            if ($env:OS -ne "Windows_NT") {
                if (-not (Get-Command python3 -ErrorAction SilentlyContinue)) {
                    throw "python3 is required for the FFI probe on supported Unix hosts"
                }
                $probeLibrary = Join-Path $tempRoot "libwg-probe.so"
                & go build -trimpath -buildmode=c-shared -o $probeLibrary ./libwg
                if ($LASTEXITCODE -ne 0) { throw "patched FFI probe build failed" }
                & python3 (Join-Path $repoRoot "tests/libwg_ffi_probe.py") $probeLibrary
                if ($LASTEXITCODE -ne 0) { throw "patched FFI probe failed" }
            } else {
                Write-Host "corplink libwg build: skipping Unix-only FFI probe on Windows"
            }
        }
        & make -B -o generate-version libwg
        if ($LASTEXITCODE -ne 0) { throw "patched wireguard-go build failed" }
    } finally {
        Pop-Location
    }
    $builtArchive = Join-Path $patchedRoot "libwg.a"
    $builtHeader = Join-Path $patchedRoot "libwg.h"
    if (-not (Test-Path $builtArchive) -or -not (Test-Path $builtHeader)) {
        throw "patched build did not produce libwg.a and libwg.h"
    }

    New-Item -ItemType Directory -Force $installStage | Out-Null
    Copy-Item -Force $builtArchive (Join-Path $installStage "libwg.a")
    Copy-Item -Force $builtHeader (Join-Path $installStage "libwg.h")
    $targetArchive = Join-Path $scriptDir "libwg.a"
    $targetHeader = Join-Path $scriptDir "libwg.h"
    if (Test-Path $targetArchive) { Move-Item -Force $targetArchive $oldArchive; $archiveBackedUp = $true }
    if (Test-Path $targetHeader) { Move-Item -Force $targetHeader $oldHeader; $headerBackedUp = $true }
    Copy-Item -Force (Join-Path $installStage "libwg.a") $targetArchive
    $archiveInstalled = $true
    Copy-Item -Force (Join-Path $installStage "libwg.h") $targetHeader
    $headerInstalled = $true
    Remove-Item -Force $oldArchive,$oldHeader -ErrorAction SilentlyContinue
    Write-Host "corplink libwg build: installed patched source $sourceRevision"
} catch {
    try { Restore-OldArtifacts } catch { Write-Error "failed to restore previous libwg artifacts: $_" }
    throw
} finally {
    if (Test-Path $tempRoot) { Remove-Item -Recurse -Force $tempRoot }
    if (Test-Path $installStage) { Remove-Item -Recurse -Force $installStage }
}
