# Build the release the Plintus hub installs: release/acermiracast.tar.gz and release/latest.json.
# Usage: .\deploy\build-release.ps1            (version from VERSION)
#        .\deploy\build-release.ps1 -Bump      (raise the last number in VERSION first)
# Commit and push release/ afterwards; the hub finds the new version under Updates.
param([switch]$Bump)
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..')
$versionFile = Join-Path $root 'VERSION'
$version = ([IO.File]::ReadAllText($versionFile)).Trim()
if ($Bump) {
    if ($version -notmatch '^(.*?)(\d+)$') { throw "VERSION '$version' ends without a number" }
    $version = $Matches[1] + ([int]$Matches[2] + 1)
    [IO.File]::WriteAllText($versionFile, "$version`n", (New-Object Text.UTF8Encoding $false))
}
# Same rules as the hub: letters, digits and . - + ~ _ only
if ($version -notmatch '^[A-Za-z0-9][A-Za-z0-9.+~_-]{0,63}$') { throw "invalid version '$version'" }

# Program (static Rust binary, the panel page is built in), installer and defaults. The hub's
# root helper installs the program files and adds new keys of config.json; install.sh is for the
# first setup and for changes of the unit file (the switch from Python to Rust is one).
$files = 'deploy\vm\install.sh', 'deploy\vm\acermiracast.service', 'deploy\vm\config.json', 'VERSION'
$top = "acermiracast-$version"
$work = Join-Path $env:TEMP "acermiracast-release-$PID"
$release = Join-Path $root 'release'
New-Item -ItemType Directory -Force (Join-Path $work $top), $release | Out-Null
try {
    $env:CARGO_TARGET_DIR = 'C:\RE\AcerMiracast\cargo-target'
    Push-Location (Join-Path $root 'rust')
    # through cmd: PowerShell 5.1 with ErrorActionPreference Stop treats cargo's stderr as an error
    try { cmd /c 'cargo build --release --target x86_64-unknown-linux-musl 2>&1' } finally { Pop-Location }
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    Copy-Item "$env:CARGO_TARGET_DIR\x86_64-unknown-linux-musl\release\acermiracast" (Join-Path $work "$top\acermiracast")
    foreach ($f in $files) {
        # LF line endings, so the shell script and the unit work on Linux
        $text = [IO.File]::ReadAllText((Join-Path $root $f)).Replace("`r`n", "`n")
        [IO.File]::WriteAllText((Join-Path $work "$top\$(Split-Path $f -Leaf)"), $text, (New-Object Text.UTF8Encoding $false))
    }
    $package = Join-Path $release 'acermiracast.tar.gz'
    tar -czf $package -C $work $top
    if ($LASTEXITCODE -ne 0) { throw 'tar failed' }
    $item = Get-Item $package
    $manifest = [ordered]@{
        name    = 'acermiracast'
        version = $version
        file    = $item.Name
        folder  = $top
        sha256  = (Get-FileHash $package -Algorithm SHA256).Hash.ToLowerInvariant()
        size    = $item.Length
        built   = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
        install = 'sudo bash install.sh'
    }
    $json = ($manifest | ConvertTo-Json) + "`n"
    [IO.File]::WriteAllText((Join-Path $release 'latest.json'), $json, (New-Object Text.UTF8Encoding $false))
    Write-Host "release\acermiracast.tar.gz ($version, $($item.Length) bytes)"
    Write-Host 'Now commit and push release\ (and VERSION); the hub shows the update under Updates.'
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
