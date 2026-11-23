#Requires -Version 7.4
param(
    [Parameter(Mandatory)]
    [ValidateSet('x64', 'arm64')]
    [string]$Architecture,
    [switch]$Offline,
    [string]$InnoCompiler
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$platform = @{
    x64 = @{ Target = 'x86_64-pc-windows-msvc'; Installer = 'x64os' }
    arm64 = @{ Target = 'aarch64-pc-windows-msvc'; Installer = 'arm64' }
}[$Architecture]
$target = $platform.Target
if ($InnoCompiler) {
    $compiler = (Get-Command $InnoCompiler -CommandType Application).Source
}

# Identify the committed source and use its Cargo version in archive names.
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$revision = git -C $repo rev-parse HEAD
if (git -C $repo status --porcelain) {
    throw 'Commit the changes before packaging.'
}
$manifest = Join-Path $repo 'Cargo.toml'
$projectMetadata = cargo metadata --frozen --no-deps --format-version 1 --manifest-path $manifest |
    ConvertFrom-Json
$version = ($projectMetadata.packages | Where-Object name -eq 'window-gestures').version
$releaseName = "window-gestures-$version"
$label = "$releaseName-$Architecture"
$targetDir = Join-Path $repo 'target'
$releaseDir = Join-Path $targetDir "packages/$($revision.Substring(0, 12))"
$sourceArchive = "$releaseName-source.zip"
$sourceZip = Join-Path $releaseDir $sourceArchive
$output = Join-Path $releaseDir $Architecture
if (Test-Path -LiteralPath $output) {
    throw "The packages already exist at $output."
}

# Keep staged artifacts on the same drive so they can be published atomically.
$stageName = "pack-$([System.IO.Path]::GetRandomFileName())"
$stage = [System.IO.Directory]::CreateDirectory((Join-Path $targetDir $stageName)).FullName
try {
    # Archive the checkout and vendor dependencies once for both architectures.
    [System.IO.Directory]::CreateDirectory($releaseDir) | Out-Null
    $source = $null
    if (![System.IO.File]::Exists($sourceZip)) {
        $trackedArchive = Join-Path $stage 'tracked.zip'
        $vendoredSource = Join-Path $stage 'vendored'
        git -C $repo archive --format=zip "--output=$trackedArchive" $revision
        [System.IO.Compression.ZipFile]::ExtractToDirectory($trackedArchive, $vendoredSource)

        Push-Location -LiteralPath $vendoredSource
        try {
            $vendorArgs = @('vendor', '--locked', '--versioned-dirs')
            if ($Offline) { $vendorArgs += '--offline' }
            $sourceConfig = cargo @vendorArgs vendor
            Add-Content -LiteralPath '.cargo/config.toml' -Encoding utf8 -Value $sourceConfig
        } finally {
            Pop-Location
        }

        $stagedSourceZip = Join-Path $stage $sourceArchive
        [System.IO.Compression.ZipFile]::CreateFromDirectory($vendoredSource, $stagedSourceZip)
        try {
            [System.IO.File]::Move($stagedSourceZip, $sourceZip)
            $source = $vendoredSource
        } catch [System.IO.IOException] {
            # Another run can publish the same commit while we vendor it.
            if (![System.IO.File]::Exists($sourceZip)) { throw }
        }
    }

    if (!$source) {
        $source = Join-Path $stage 'source'
        [System.IO.Compression.ZipFile]::ExtractToDirectory($sourceZip, $source)
    }

    # Build the selected target offline and resolve the crates used by that target.
    Push-Location -LiteralPath $source
    try {
        $metadata = cargo metadata --frozen --filter-platform $target --format-version 1 |
            ConvertFrom-Json
        $buildDir = Join-Path $stage 'build'
        cargo build --frozen --release --target $target --target-dir $buildDir
    } finally {
        Pop-Location
    }

    # Assemble the runtime files and license notices for the portable ZIP and installer.
    $bundle = (New-Item -ItemType Directory -Path (Join-Path $stage 'bundle')).FullName
    Copy-Item -LiteralPath @(
        (Join-Path $buildDir "$target/release/window-gestures.exe"),
        (Join-Path $source 'LICENSE'),
        (Join-Path $source 'README.md'),
        (Join-Path $source 'BUILDING.md')
    ) -Destination $bundle
    $assets = (New-Item -ItemType Directory -Path (Join-Path $bundle 'assets')).FullName
    Copy-Item -LiteralPath @(
        (Join-Path $source 'assets/window-gestures.png'),
        (Join-Path $source 'assets/window-gestures-demo.gif')
    ) -Destination $assets
    $licenses = (New-Item -ItemType Directory -Path (Join-Path $bundle 'licenses')).FullName
    foreach ($dependency in $metadata.packages | Where-Object name -ne 'window-gestures') {
        $notices = @(Get-ChildItem -LiteralPath (Split-Path $dependency.manifest_path) -File |
            Where-Object Name -Match '^(LICENSE|LICENCE|COPYING|NOTICE|UNLICENSE|COPYRIGHT)')
        if (!$dependency.license -or $notices.Count -eq 0) {
            throw "Review the license files for $($dependency.name) $($dependency.version)."
        }
        $destination = New-Item -ItemType Directory -Path (
            Join-Path $licenses "$($dependency.name)-$($dependency.version)"
        )
        Copy-Item -LiteralPath $notices.FullName -Destination $destination.FullName
    }
    $rustDocs = Join-Path (rustc --print sysroot) 'share/doc/rust'
    $rustNotices = New-Item -ItemType Directory -Path (Join-Path $licenses 'rust')
    Copy-Item -Recurse -LiteralPath @(
        (Join-Path $rustDocs 'COPYRIGHT-library.html'),
        (Join-Path $rustDocs 'licenses')
    ) -Destination $rustNotices.FullName

    # Record the build inputs and create the requested download files.
    @("Revision $revision", "Target $target", (rustc --version), "Source archive $sourceArchive") |
        Set-Content -LiteralPath (Join-Path $bundle 'BUILD.txt') -Encoding utf8
    $artifacts = [System.IO.Directory]::CreateDirectory((Join-Path $stage 'artifacts')).FullName
    $binaryZip = Join-Path $artifacts "$label-portable.zip"
    [System.IO.Compression.ZipFile]::CreateFromDirectory($bundle, $binaryZip)

    if ($InnoCompiler) {
        $installerName = "window-gestures-$($Architecture.ToLowerInvariant())-setup"
        & $compiler "-O$artifacts" "-F$installerName" "-DBundleDir=$bundle" "-DAppVersion=$version" `
            "-DTargetArchitecture=$($platform.Installer)" `
            (Join-Path $source 'scripts/window-gestures.iss')
    }

    # Publish the complete architecture directory and print its checksums.
    [System.IO.Directory]::Move($artifacts, $output)
    Get-Item -LiteralPath $sourceZip | Get-FileHash -Algorithm SHA256
    Get-ChildItem -LiteralPath $output -File | Get-FileHash -Algorithm SHA256
} finally {
    $resolvedStage = (Resolve-Path -LiteralPath $stage).Path
    if ($resolvedStage -ieq (Join-Path $targetDir $stageName)) {
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force
    }
}
