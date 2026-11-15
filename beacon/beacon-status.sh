#!/bin/sh
# beacon-status.sh - write the beacon state to /tmp/beacon.json, served as http://<box>/beacon.json.
# The receiver's web panel reads that file.
#
#   sh beacon-status.sh            write the file once (cron runs this every minute)
#   sh beacon-status.sh --install  set up the cron job and the link in the web root

SELF="/root/beacon-status.sh"

if [ "$1" = "--install" ]; then
	ln -sf /tmp/beacon.json /www/beacon.json
	touch /etc/crontabs/root
	grep -q "$SELF" /etc/crontabs/root || echo "* * * * * sh $SELF" >> /etc/crontabs/root
	/etc/init.d/cron enable
	/etc/init.d/cron restart
	sh "$SELF"
	echo "installed: $(cat /tmp/beacon.json)"
	exit 0
fi

NAME=""; IP=""
[ -f /etc/mice-beacon.conf ] && . /etc/mice-beacon.conf
SSID="$(uci -q get wireless.mice.ssid)"
UP=false
if [ -n "$SSID" ] && iw dev 2>/dev/null | grep -q "ssid $SSID\$"; then UP=true; fi
CHANNEL="$(iw dev 2>/dev/null | awk '/channel/ {print $2; exit}')"
UPTIME="$(cut -d. -f1 /proc/uptime)"
# Wi-Fi Direct (install-p2p.sh): off | fallback | only, "none" if not installed
P2P=none
[ -x /usr/sbin/acer-p2p-mode ] && P2P="$(/usr/sbin/acer-p2p-mode)"
P2P_BUSY=false; [ -f /tmp/acer-p2p-mode.busy ] && P2P_BUSY=true

printf '{"name":"%s","receiver":"%s","ssid":"%s","channel":"%s","up":%s,"uptime":%s,"p2p":"%s","p2p_busy":%s}\n' \
	"$NAME" "$IP" "$SSID" "$CHANNEL" "$UP" "${UPTIME:-0}" "$P2P" "$P2P_BUSY" > /tmp/beacon.json.$$
mv /tmp/beacon.json.$$ /tmp/beacon.json
