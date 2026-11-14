#!/bin/sh
# install-wake.sh - let the receiver switch the projector on through this box.
#
# Switched off, the projector is gone from the network and only reacts to a Wake-on-LAN packet,
# which has to come from its own subnet. This installs `etherwake` and a web hook:
#     POST http://<box>/cgi-bin/wake-projector   -> sends the packet, answers "ok"
# The hook can only wake the one MAC address given here.
#
# Usage: sh install-wake.sh <projector-mac>      e.g. 00:60:e9:30:22:e6
set -e
MAC="$1"
echo "$MAC" | grep -Eq '^([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}$' || { echo "usage: sh install-wake.sh <mac>" >&2; exit 1; }

if ! command -v etherwake >/dev/null; then
	apk update >/dev/null
	apk add etherwake
fi

echo "$MAC" > /etc/wake-projector.mac
cat > /www/cgi-bin/wake-projector <<'EOF'
#!/bin/sh
printf 'Content-Type: text/plain\r\n\r\n'
[ "$REQUEST_METHOD" = "POST" ] || { echo "POST only"; exit 0; }
MAC="$(cat /etc/wake-projector.mac 2>/dev/null)"
[ -n "$MAC" ] || { echo "no mac configured"; exit 0; }
if etherwake -i br-lan "$MAC" 2>/dev/null; then echo ok; else echo failed; fi
EOF
chmod 755 /www/cgi-bin/wake-projector
echo "installed for $MAC"
