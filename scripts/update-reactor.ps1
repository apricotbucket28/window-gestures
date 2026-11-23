#Requires -Version 7.4
[CmdletBinding(DefaultParameterSetName = 'Update')]
param(
    [Parameter(Mandatory, ParameterSetName = 'Update')]
    [string]$Revision,
    [Parameter(Mandatory, ParameterSetName = 'Check')]
    [switch]$Check,
    [string]$Upstream
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
$repo = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$snapshot = Join-Path $repo 'third_party/windows-reactor'
$extension = Join-Path $repo 'third_party/reactor-popup'
$revisionFile = Join-Path $extension 'windows-rs-revision.txt'
$patchFile = Join-Path $extension 'upstream.patch'

if ($Check) {
    $Revision = (Get-Content -LiteralPath $revisionFile -Raw).Trim()
} elseif (git -C $repo status --porcelain -- third_party/windows-reactor) {
    throw 'The vendored Reactor snapshot has local changes. Preserve them before updating.'
}

$target = [System.IO.Directory]::CreateDirectory((Join-Path $repo 'target')).FullName
$stage = [System.IO.Directory]::CreateDirectory(
    (Join-Path $target "reactor-update-$([Guid]::NewGuid().ToString('N'))")
)
if ($stage.Parent.FullName -ne $target) {
    throw "Unexpected Reactor staging directory: $($stage.FullName)"
}
try {
    if (!$Upstream) {
        $Upstream = Join-Path $stage.FullName 'upstream'
        git init --quiet $Upstream
        git -C $Upstream remote add origin https://github.com/microsoft/windows-rs
        git -C $Upstream fetch --quiet --depth=1 origin $Revision
        $Revision = 'FETCH_HEAD'
    }
    $commit = git -C $Upstream rev-parse --verify --end-of-options "$Revision^{commit}"

    $archive = Join-Path $stage.FullName 'reactor.zip'
    $extracted = Join-Path $stage.FullName 'extracted'
    git -C $Upstream archive --format=zip "--output=$archive" $commit crates/libs/reactor
    [System.IO.Compression.ZipFile]::ExtractToDirectory($archive, $extracted)
    $source = Join-Path $extracted 'crates/libs/reactor'

    # The source manifest inherits the windows-rs workspace. Use that same revision
    # for its dependencies when resolving the crate from Window Gestures.
    $patch = (Get-Content -LiteralPath $patchFile -Raw).Replace('@WINDOWS_RS_REVISION@', $commit)
    $preparedPatch = Join-Path $stage.FullName 'reactor.patch'
    [System.IO.File]::WriteAllText($preparedPatch, $patch)
    $relativeSource = [System.IO.Path]::GetRelativePath($repo, $source).Replace('\', '/')
    git -C $repo apply --check "--directory=$relativeSource" $preparedPatch
    git -C $repo apply "--directory=$relativeSource" $preparedPatch

    if ($Check) {
        git -c core.autocrlf=false -c core.safecrlf=false diff --no-index --ignore-cr-at-eol --quiet -- $snapshot $source
        Write-Output "Reactor snapshot matches $commit and upstream.patch."
    } else {
        # The verified archive and the current snapshot are on the same volume.
        $previous = Join-Path $stage.FullName 'previous'
        if ((Resolve-Path -LiteralPath $snapshot).Path -ne $snapshot) {
            throw "Unexpected Reactor snapshot directory: $snapshot"
        }
        [System.IO.Directory]::Move($snapshot, $previous)
        try {
            [System.IO.Directory]::Move($source, $snapshot)
        } catch {
            [System.IO.Directory]::Move($previous, $snapshot)
            throw
        }
        Set-Content -LiteralPath $revisionFile -Encoding utf8 -Value $commit
        Write-Output "Updated Reactor to $commit. Run cargo check --all-targets to refresh Cargo.lock."
    }
} finally {
    Remove-Item -LiteralPath $stage.FullName -Recurse -Force
}
