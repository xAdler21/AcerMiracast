# Pack receiver + installer and run install.sh on the VM in ONE ssh session.
# Usage: .\deploy\vm\push.ps1 [-Vm 10.21.0.232]
#        -ProjectorHost <ip> also writes that address into /etc/acermiracast/config.json
param([string]$Vm = '10.21.0.232', [string]$User = 'claude', [string]$ProjectorHost = '')
if ($ProjectorHost -and $ProjectorHost -notmatch '^[0-9.]+$') { throw 'ProjectorHost must be an IPv4 address' }
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$key = Join-Path $env:USERPROFILE '.ssh\claude_plintus_vm'
$work = Join-Path $env:TEMP "acermiracast-push-$PID"
New-Item -ItemType Directory -Force "$work\pkg" | Out-Null
try {
    # static Linux binary (musl, linked by rust-lld; see rust\.cargo\config.toml). The target
    # directory lives on C: because build scripts may not run from the V: drive.
    $env:CARGO_TARGET_DIR = 'C:\RE\AcerMiracast\cargo-target'
    Push-Location (Join-Path $root 'rust')
    # through cmd: PowerShell 5.1 with ErrorActionPreference Stop treats cargo's stderr as an error
    try { cmd /c 'cargo build --release --target x86_64-unknown-linux-musl 2>&1' } finally { Pop-Location }
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    Copy-Item "$env:CARGO_TARGET_DIR\x86_64-unknown-linux-musl\release\acermiracast" "$work\pkg\acermiracast"
    $files = 'deploy\vm\install.sh', 'deploy\vm\acermiracast.service', 'deploy\vm\config.json', 'VERSION'
    foreach ($f in $files) {
        # normalise to LF so the shell script and unit file work on Linux
        $text = [IO.File]::ReadAllText((Join-Path $root $f)).Replace("`r`n", "`n")
        [IO.File]::WriteAllText((Join-Path "$work\pkg" (Split-Path $f -Leaf)), $text, (New-Object Text.UTF8Encoding $false))
    }
    tar -czf "$work\pkg.tgz" -C "$work\pkg" .
    if ($LASTEXITCODE -ne 0) { throw 'tar failed' }
    $b64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes("$work\pkg.tgz"), 'InsertLineBreaks').Replace("`r`n", "`n")
    $script = "set -e`nD=`$(mktemp -d)`ntrap 'rm -rf `$D' EXIT`nbase64 -d > `$D/pkg.tgz <<'PAYLOAD_END'`n$b64`nPAYLOAD_END`ntar -xzf `$D/pkg.tgz -C `$D`nbash `$D/install.sh`n"
    if ($ProjectorHost) {
        $script += "python3 -c `"import json; p='/etc/acermiracast/config.json'; c=json.load(open(p)); c['projector_host']='$ProjectorHost'; json.dump(c, open(p,'w'), indent=2)`"`n"
        $script += "systemctl restart acermiracast.service`nsleep 8`n"
    }
    $script += "echo '== status'; curl -s -m 5 http://127.0.0.1:8445/api/status; echo`n"
    [IO.File]::WriteAllText("$work\remote.sh", $script, (New-Object Text.UTF8Encoding $false))
    cmd /c "ssh -i `"$key`" -o BatchMode=yes -o ConnectTimeout=10 $User@$Vm sudo bash -s < `"$work\remote.sh`""
    if ($LASTEXITCODE -ne 0) { throw "install failed ($LASTEXITCODE)" }
} finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
