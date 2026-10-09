[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$oldEncodedFlags = $env:CARGO_ENCODED_RUSTFLAGS
Push-Location $projectRoot
try {
    # Cargo's encoded flags preserve paths containing spaces. Remap both path
    # separators so Rust diagnostics embedded in the executable expose no local profile.
    $flags = @()
    if ($oldEncodedFlags) {
        $flags += $oldEncodedFlags.Split([char]31)
    } elseif ($env:RUSTFLAGS) {
        $flags += $env:RUSTFLAGS.Split(' ', [StringSplitOptions]::RemoveEmptyEntries)
    }
    foreach ($prefix in @($env:USERPROFILE, $env:CARGO_HOME, $projectRoot) | Where-Object { $_ } | Select-Object -Unique) {
        $replacement = if ($prefix -eq $projectRoot) { 'C:/build/GameReady' } else { 'C:/build/user' }
        $flags += "--remap-path-prefix=$prefix=$replacement"
        $flags += "--remap-path-prefix=$($prefix.Replace('\', '/'))=$replacement"
    }
    $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]31
    cargo build --release --locked --bin gameready --features custom-protocol
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }

    $version = (Get-Content -LiteralPath 'tauri.conf.json' -Raw | ConvertFrom-Json).version
    $assetBase = "GameReady-$version-windows-x64"
    $releaseDir = Join-Path $projectRoot 'release'
    [void](New-Item -ItemType Directory -Path $releaseDir -Force)
    $builtExe = Join-Path $projectRoot 'target/release/gameready.exe'
    $bytes = [IO.File]::ReadAllBytes($builtExe)
    foreach ($encoding in @([Text.Encoding]::UTF8, [Text.Encoding]::Unicode)) {
        $contents = $encoding.GetString($bytes)
        foreach ($prefix in @($env:USERPROFILE, $env:CARGO_HOME, $projectRoot) | Where-Object { $_ }) {
            foreach ($variant in @($prefix, $prefix.Replace('\', '/'))) {
                if ($contents.IndexOf($variant, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                    throw 'Release executable still contains a local build path; publishing stopped'
                }
            }
        }
    }
    Copy-Item -LiteralPath $builtExe -Destination (Join-Path $projectRoot 'gameready.exe') -Force
    $exeAsset = Join-Path $releaseDir "$assetBase.exe"
    Copy-Item -LiteralPath $builtExe -Destination $exeAsset -Force

    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zipAsset = Join-Path $releaseDir "$assetBase.zip"
    $zipTemp = Join-Path $releaseDir "$assetBase.$([guid]::NewGuid()).tmp.zip"
    try {
        $archive = [IO.Compression.ZipFile]::Open($zipTemp, [IO.Compression.ZipArchiveMode]::Create)
        try {
            # Explicit allowlist: never package Data, credentials, logs, or screenshots.
            [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $builtExe, 'gameready.exe')
            [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, (Join-Path $projectRoot 'README.md'), 'README.md')
        } finally { $archive.Dispose() }
        Move-Item -LiteralPath $zipTemp -Destination $zipAsset -Force
    } finally {
        if (Test-Path -LiteralPath $zipTemp) { Remove-Item -LiteralPath $zipTemp }
    }
    Get-FileHash -Algorithm SHA256 -LiteralPath $exeAsset, $zipAsset |
        Select-Object @{Name='Asset';Expression={Split-Path $_.Path -Leaf}}, Hash
} finally {
    $env:CARGO_ENCODED_RUSTFLAGS = $oldEncodedFlags
    Pop-Location
}
