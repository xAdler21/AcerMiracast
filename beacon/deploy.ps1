# Copy mice-beacon.sh to the OpenWrt box and switch the beacon on.
# Usage: .\beacon\deploy.ps1 -ReceiverIp 10.1.34.201 [-Name Konferenzraum] [-Channel 6] [-DryRun]
# Needs the SSH key ~/.ssh/claude_fritzbox_beacon to be installed on the box (see README.md).
param(
    [Parameter(Mandatory = $true)][string]$ReceiverIp,
    [string]$Name = 'Konferenzraum',
    [int]$Channel = 6,
    [string]$Box = '10.1.34.178',
    [switch]$DryRun
)
$ErrorActionPreference = 'Stop'
$key = Join-Path $env:USERPROFILE '.ssh\claude_fritzbox_beacon'
$script = Join-Path $PSScriptRoot 'mice-beacon.sh'
$ssh = @('-i', $key, '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8')

scp -O @ssh $script (Join-Path $PSScriptRoot 'beacon-status.sh') "root@${Box}:/root/"
if ($LASTEXITCODE -ne 0) { throw "scp failed ($LASTEXITCODE)" }

$prefix = ''
if ($DryRun) { $prefix = 'DRY_RUN=1 ' }
ssh @ssh "root@$Box" "${prefix}sh /root/mice-beacon.sh $Name $ReceiverIp $Channel"
if ($LASTEXITCODE -ne 0) { throw "mice-beacon.sh failed ($LASTEXITCODE)" }
if (-not $DryRun) {
    ssh @ssh "root@$Box" "sh /root/beacon-status.sh --install"
    if ($LASTEXITCODE -ne 0) { throw "beacon-status.sh failed ($LASTEXITCODE)" }
}
