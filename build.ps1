[CmdletBinding()]
param([switch]$Clean)

$ErrorActionPreference = 'Stop'
$projectRoot = $PSScriptRoot
$oldEncodedFlags = $env:CARGO_ENCODED_RUSTFLAGS

function Stop-GameReadyProcesses {
    $snapshot = @(Get-CimInstance Win32_Process)
    $treeIds = [Collections.Generic.HashSet[uint32]]::new()
    foreach ($root in @($snapshot | Where-Object { $_.Name -ieq 'gameready.exe' })) { [void]$treeIds.Add($root.ProcessId) }
    do {
        $added = $false
        foreach ($entry in $snapshot) {
            if ($treeIds.Contains($entry.ParentProcessId) -and $treeIds.Add($entry.ProcessId)) { $added = $true }
        }
    } while ($added)
    $tracked = @()
    try {
    foreach ($processId in $treeIds) {
        $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
        if ($process) {
            # Keep the original process handle, so PID reuse cannot hide a surviving child.
            [void]$process.Handle
            $tracked += $process
        }
    }
    foreach ($running in @(Get-Process -Name gameready -ErrorAction SilentlyContinue)) {
        try {
            if (-not $running.HasExited) {
                # 包含 WebView 子进程，不能只关闭主窗口（可能只是隐藏到托盘）。
                $stopper = Start-Process -FilePath (Join-Path $env:SystemRoot 'System32/taskkill.exe') `
                    -ArgumentList @('/PID', $running.Id, '/T', '/F') -WindowStyle Hidden -Wait -PassThru
                try {
                    if ($stopper.ExitCode -ne 0 -and -not $running.HasExited) {
                        throw 'Cannot stop GameReady; executable replacement stopped'
                    }
                } finally { $stopper.Dispose() }
            }
            if (-not $running.WaitForExit(10000)) { throw 'GameReady did not exit; executable replacement stopped' }
        } finally { $running.Dispose() }
    }
    if (Get-Process -Name gameready -ErrorAction SilentlyContinue) {
        throw 'GameReady is still running; executable replacement stopped'
    }
    foreach ($process in $tracked) {
        if (-not $process.WaitForExit(10000)) { throw 'A GameReady child is still running; executable replacement stopped' }
    }
    } finally { foreach ($process in $tracked) { $process.Dispose() } }
}

Push-Location $projectRoot
try {
    # Ship the reviewed official MSI inside the EXE, with its redistribution license.
    $componentPackage = Join-Path $projectRoot 'assets/third-party/fakerinput/FakerInput_Setup_0.1.1_x64.msi'
    $componentHash = '4C0AEFB7340051A91D606776243298B5CD1143EF5508BBAE6800C474F9ED0840'
    if ((Get-FileHash -LiteralPath $componentPackage -Algorithm SHA256).Hash -ne $componentHash) {
        throw 'Bundled FakerInput package does not match the reviewed official release'
    }
    $componentSignature = Get-AuthenticodeSignature -LiteralPath $componentPackage
    if ($componentSignature.Status -ne 'Valid' -or $componentSignature.SignerCertificate.Subject -notmatch '^CN=Ryodigi Solutions LLC,') {
        throw 'Bundled FakerInput signature validation failed'
    }
    $componentLicense = Get-Content -LiteralPath (Join-Path $projectRoot 'assets/third-party/fakerinput/LICENSE.txt') -Raw
    if ($componentLicense -notmatch 'Copyright \(c\) 2021 Travis Nickles') { throw 'Bundled FakerInput license is missing' }
    $version = (Get-Content -LiteralPath 'tauri.conf.json' -Encoding UTF8 -Raw | ConvertFrom-Json).version
    $metadata = cargo metadata --no-deps --locked --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read package metadata' }
    $packageVersion = ($metadata.packages | Where-Object { $_.name -eq 'gameready' }).version
    if ($version -ne $packageVersion) { throw 'Cargo and Tauri versions must match' }

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
    # 每次打包固定交付到桌面 gameready.exe；Data 保留在原位置。
    $desktopDir = [Environment]::GetFolderPath('DesktopDirectory', [Environment+SpecialFolderOption]::DoNotVerify)
    if (-not $desktopDir) {
        $desktopDir = (Get-ItemProperty -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders' -Name Desktop -ErrorAction Stop).Desktop
        $desktopDir = [Environment]::ExpandEnvironmentVariables($desktopDir)
    }
    if (-not $desktopDir -or -not (Test-Path -LiteralPath $desktopDir -PathType Container)) {
        throw 'Cannot locate the desktop folder'
    }
    $desktopExe = Join-Path $desktopDir 'gameready.exe'
    $exeAsset = Join-Path $releaseDir "$assetBase.exe"
    Copy-Item -LiteralPath $builtExe -Destination $exeAsset -Force

    # Two names for the same EXE: stable direct download and legacy update checks.
    $stableAsset = Join-Path $releaseDir 'gameready.exe'
    Copy-Item -LiteralPath $builtExe -Destination $stableAsset -Force
    foreach ($asset in @($exeAsset, $stableAsset)) {
        $sha256 = [Security.Cryptography.SHA256]::Create()
        $stream = [IO.File]::OpenRead($asset)
        try {
            [pscustomobject]@{
                Asset = [IO.Path]::GetFileName($asset)
                Hash = [BitConverter]::ToString($sha256.ComputeHash($stream)).Replace('-', '')
            }
        } finally { $stream.Dispose(); $sha256.Dispose() }
    }
    # 校验完成后，在替换前停止并等待退出，缩短关闭程序到更新完成的间隔。
    Stop-GameReadyProcesses
    Copy-Item -LiteralPath $builtExe -Destination (Join-Path $projectRoot 'gameready.exe') -Force
    Copy-Item -LiteralPath $builtExe -Destination $desktopExe -Force
    if ((Get-FileHash -LiteralPath $desktopExe).Hash -ne (Get-FileHash -LiteralPath $builtExe).Hash) {
        throw 'Desktop executable verification failed'
    }
    Write-Output 'Desktop gameready.exe updated and verified'
    if ($Clean) {
        cargo clean
        if ($LASTEXITCODE -ne 0) { throw 'Build cache cleanup failed' }
    }
} finally {
    $env:CARGO_ENCODED_RUSTFLAGS = $oldEncodedFlags
    Pop-Location
}
