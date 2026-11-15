#!/bin/sh
# install-p2p.sh - Wi-Fi Direct (Miracast over P2P) on the beacon box, for laptops that cannot
# reach the receiver over the network (no WLAN, guest WLAN) and for Android Miracast.
#
# The box becomes a Wi-Fi Direct group owner named like the beacon, accepts every connection
# (push button, armed all the time), gives the laptop the fixed address 192.168.173.2 and
# reports it in http://<box>/p2p.json. The receiver (VM, "wifi_direct": true) then connects to
# the box's TCP 7236, which goes on to the laptop's RTSP port; the laptop's video (UDP to the
# box, port 50000) goes on to the receiver. The router lets the box talk to the receiver only
# as an answer: the receiver opens the UDP path itself (it sends to box:50000).
#
# Usage:   RECEIVER=10.21.0.232 sh install-p2p.sh
#          MODE=only RECEIVER=10.21.0.232 sh install-p2p.sh    (mode to start with, see below;
#                                                without MODE the last mode is kept)
#          REMOVE=1 sh install-p2p.sh          (packages stay installed, beacon back to network only)
#
# Modes (switched later with `acer-p2p-mode <mode>` or from the receiver's web panel through
# POST http://<box>/cgi-bin/p2p-mode, which only the receiver may call):
#   off       Wi-Fi Direct off, the beacon names only the receiver (network path)
#   fallback  the beacon names the receiver and the Wi-Fi Direct device: Windows tries the
#             network first and uses Wi-Fi Direct after that timed out
#   only      the beacon names only the Wi-Fi Direct device: Windows uses it at once
# Needs:   wpa-supplicant-p2p, wpa-cli and hostapd-basic-mbedtls instead of wpad-basic-mbedtls
#          (installed by this script).

set -e
NET=192.168.173
IF=p2p0

remove() {
	/etc/init.d/acer-p2p stop 2>/dev/null || true
	/etc/init.d/acer-p2p disable 2>/dev/null || true
	for s in acer_p2p_zone acer_p2p_fwd_in acer_p2p_fwd_out acer_p2p_rtsp acer_p2p_rtp acer_p2p_rtp_snat; do
		uci -q delete "firewall.$s" || true
	done
	uci commit firewall
	rm -f /etc/nftables.d/20-acer-p2p.nft /etc/init.d/acer-p2p /usr/sbin/acer-p2p /www/p2p.json \
		/usr/sbin/acer-p2p-mode /www/cgi-bin/p2p-mode /etc/acer-p2p.mode
	fw4 -q reload
	# the beacon goes back to the network path only
	if [ -f /etc/mice-beacon.conf ]; then
		. /etc/mice-beacon.conf
		P2P_ADDR= ONLY_P2P=0 sh /root/mice-beacon.sh "$NAME" "$IP" 6 >/dev/null
	fi
	sh /root/beacon-status.sh 2>/dev/null || true
}

if [ "${REMOVE:-0}" = "1" ]; then
	remove
	echo "Wi-Fi Direct removed (the packages stay installed)."
	exit 0
fi

[ -n "$RECEIVER" ] || { echo "usage: RECEIVER=<receiver-ip> sh install-p2p.sh" >&2; exit 1; }
. /etc/mice-beacon.conf    # NAME, IP of the beacon
MODE="${MODE:-$(cat /etc/acer-p2p.mode 2>/dev/null || true)}"
MODE="${MODE:-fallback}"
case "$MODE" in off|fallback|only) ;; *) echo "MODE must be off, fallback or only" >&2; exit 1 ;; esac
BOX="$(ip -4 -o addr show br-lan | awk '{print $4}' | cut -d/ -f1)"

# ---- packages ---------------------------------------------------------------------------------
if ! apk info -e wpa-supplicant-p2p >/dev/null 2>&1 || ! command -v wpa_cli >/dev/null; then
	apk update >/dev/null
	if apk info -e wpad-basic-mbedtls >/dev/null 2>&1; then
		apk add hostapd-basic-mbedtls wpa-supplicant-p2p wpa-cli '!wpad-basic-mbedtls'
	else
		apk add wpa-supplicant-p2p wpa-cli
	fi
	/etc/init.d/wpad restart || true
	wifi reload
	sleep 8
fi

# ---- address of the P2P interface: the radio MAC as a locally administered address --------------
PHYMAC="$(cat /sys/class/ieee80211/phy0/macaddress)"
FIRST=$(( 0x$(echo "$PHYMAC" | cut -d: -f1) | 2 ))
P2PMAC="$(printf '%02x' "$FIRST"):$(echo "$PHYMAC" | cut -d: -f2-)"

# ---- the daemon ---------------------------------------------------------------------------------
cat > /usr/sbin/acer-p2p <<EOF
#!/bin/sh
# Wi-Fi Direct group owner for AcerMiracast (installed by install-p2p.sh)
IF=$IF; NET=$NET; MAC=$P2PMAC; NAME="$NAME"
RUN=/var/run/acer-p2p
EOF
cat >> /usr/sbin/acer-p2p <<'EOF'
mkdir -p "$RUN"
cli() { wpa_cli -p "$RUN" -i "$IF" "$@" >/dev/null 2>&1; }
status() { [ "$(cat /tmp/p2p.json 2>/dev/null)" = "$1" ] || echo "$1" > /tmp/p2p.json; }

start_radio() {
	iw dev "$IF" info >/dev/null 2>&1 || iw phy phy0 interface add "$IF" type managed
	ip link set "$IF" down
	ip link set "$IF" address "$MAC"
	ip link set "$IF" up
	cat > "$RUN/wpa.conf" <<CONF
ctrl_interface=$RUN
device_name=$NAME
device_type=7-0050F204-3
manufacturer=AcerMiracast
model_name=$NAME
config_methods=virtual_push_button physical_push_button
p2p_go_intent=15
p2p_listen_reg_class=81
p2p_listen_channel=6
p2p_oper_reg_class=81
p2p_oper_channel=6
p2p_no_group_iface=1
p2p_ssid_postfix=-$NAME
country=DE
CONF
	wpa_supplicant -B -i "$IF" -c "$RUN/wpa.conf" -D nl80211 -P "$RUN/wpa.pid"
	sleep 2
	# Wi-Fi Display element (the p2p package has no CONFIG_WIFI_DISPLAY, so it is added as a
	# vendor element): device information = primary sink, available, RTSP port 7236, 200 Mbit/s.
	# Frames: probe responses (1, 2), GO beacon (3), provision discovery response (5),
	# GO negotiation (6, 7, 8), invitation (9, 10), association response (12).
	for frame in 1 2 3 5 6 7 8 9 10 12; do
		cli vendor_elem_add "$frame" dd0d506f9a0a00000600111c4400c8
	done
	cli p2p_group_add freq=2437
	sleep 3
	ip addr add "$NET.1/24" dev "$IF" 2>/dev/null
	start_dhcp
}

start_dhcp() {
	[ -f "$RUN/dnsmasq.pid" ] && kill "$(cat "$RUN/dnsmasq.pid")" 2>/dev/null
	rm -f "$RUN/leases"
	# one address only (the forwarding points at it), no gateway or DNS: the laptop keeps its
	# own internet connection
	dnsmasq --conf-file=/dev/null --port=0 --interface="$IF" --bind-interfaces \
		--dhcp-range="$NET.2,$NET.2,255.255.255.0,5m" --dhcp-option=3 --dhcp-option=6 \
		--dhcp-authoritative --dhcp-leasefile="$RUN/leases" --pid-file="$RUN/dnsmasq.pid"
}

stop_all() {
	cli p2p_group_remove "$IF"
	[ -f "$RUN/wpa.pid" ] && kill "$(cat "$RUN/wpa.pid")" 2>/dev/null
	[ -f "$RUN/dnsmasq.pid" ] && kill "$(cat "$RUN/dnsmasq.pid")" 2>/dev/null
	status '{"peer":""}'
	exit 0
}
trap stop_all INT TERM

ln -sf /tmp/p2p.json /www/p2p.json
status '{"peer":""}'
start_radio
tick=0
while true; do
	# keep the push button armed (a WPS PBC window lasts 120 s)
	[ $(( tick % 60 )) -eq 0 ] && cli wps_pbc
	if ! iw dev "$IF" info 2>/dev/null | grep -q 'type P2P-GO'; then
		logger -t acer-p2p "group owner gone, restarting"
		[ -f "$RUN/wpa.pid" ] && kill "$(cat "$RUN/wpa.pid")" 2>/dev/null
		sleep 2
		start_radio
	fi
	STATIONS=$(iw dev "$IF" station dump 2>/dev/null | grep -c '^Station')
	LEASE="$(head -n 1 "$RUN/leases" 2>/dev/null)"
	if [ "$STATIONS" -gt 0 ] && [ -n "$LEASE" ]; then
		set -- $LEASE        # expiry mac ip hostname client-id
		HOST="$4"; [ "$HOST" = "*" ] && HOST=""
		status "{\"peer\":\"$3\",\"name\":\"$HOST\",\"mac\":\"$2\"}"
	else
		status '{"peer":""}'
		# the laptop left: free the single address for the next one
		[ "$STATIONS" -eq 0 ] && [ -n "$LEASE" ] && start_dhcp
	fi
	tick=$(( tick + 2 ))
	sleep 2
done
EOF
chmod 755 /usr/sbin/acer-p2p

cat > /etc/init.d/acer-p2p <<'EOF'
#!/bin/sh /etc/rc.common
START=99
USE_PROCD=1
start_service() {
	procd_open_instance
	procd_set_param command /usr/sbin/acer-p2p
	procd_set_param respawn 3600 5 0
	procd_close_instance
}
EOF
chmod 755 /etc/init.d/acer-p2p

# ---- firewall -----------------------------------------------------------------------------------
uci -q batch <<EOF
delete firewall.acer_p2p_zone
set firewall.acer_p2p_zone=zone
set firewall.acer_p2p_zone.name=p2p
set firewall.acer_p2p_zone.device=$IF
set firewall.acer_p2p_zone.input=ACCEPT
set firewall.acer_p2p_zone.output=ACCEPT
set firewall.acer_p2p_zone.forward=REJECT
set firewall.acer_p2p_zone.masq=1
delete firewall.acer_p2p_fwd_in
set firewall.acer_p2p_fwd_in=forwarding
set firewall.acer_p2p_fwd_in.src=lan
set firewall.acer_p2p_fwd_in.dest=p2p
delete firewall.acer_p2p_fwd_out
set firewall.acer_p2p_fwd_out=forwarding
set firewall.acer_p2p_fwd_out.src=p2p
set firewall.acer_p2p_fwd_out.dest=lan
delete firewall.acer_p2p_rtsp
set firewall.acer_p2p_rtsp=redirect
set firewall.acer_p2p_rtsp.name=acer-p2p-rtsp
set firewall.acer_p2p_rtsp.target=DNAT
set firewall.acer_p2p_rtsp.src=lan
set firewall.acer_p2p_rtsp.src_ip=$RECEIVER
set firewall.acer_p2p_rtsp.src_dport=7236
set firewall.acer_p2p_rtsp.proto=tcp
set firewall.acer_p2p_rtsp.dest=p2p
set firewall.acer_p2p_rtsp.dest_ip=$NET.2
set firewall.acer_p2p_rtsp.dest_port=7236
delete firewall.acer_p2p_rtp
set firewall.acer_p2p_rtp=redirect
set firewall.acer_p2p_rtp.name=acer-p2p-rtp
set firewall.acer_p2p_rtp.target=DNAT
set firewall.acer_p2p_rtp.src=p2p
set firewall.acer_p2p_rtp.src_dport=50000
set firewall.acer_p2p_rtp.proto=udp
set firewall.acer_p2p_rtp.dest=lan
set firewall.acer_p2p_rtp.dest_ip=$RECEIVER
set firewall.acer_p2p_rtp.dest_port=50000
delete firewall.acer_p2p_rtp_snat
set firewall.acer_p2p_rtp_snat=nat
set firewall.acer_p2p_rtp_snat.name=acer-p2p-rtp-snat
set firewall.acer_p2p_rtp_snat.target=SNAT
set firewall.acer_p2p_rtp_snat.src=lan
set firewall.acer_p2p_rtp_snat.proto=udp
set firewall.acer_p2p_rtp_snat.dest_ip=$RECEIVER
set firewall.acer_p2p_rtp_snat.dest_port=50000
set firewall.acer_p2p_rtp_snat.snat_ip=$BOX
set firewall.acer_p2p_rtp_snat.snat_port=50000
commit firewall
EOF
# the receiver's keep-open packets must not create a connection on the box: that would take
# the address box:50000 the video is sent from
cat > /etc/nftables.d/20-acer-p2p.nft <<EOF
chain acer_p2p_keepopen {
	type filter hook prerouting priority raw; policy accept;
	ip saddr $RECEIVER udp dport 50000 iifname "br-lan" drop
}
EOF
fw4 -q reload

# ---- switching the mode ---------------------------------------------------------------------------
cat > /usr/sbin/acer-p2p-mode <<EOF
#!/bin/sh
# acer-p2p-mode [off|fallback|only] - switch Wi-Fi Direct (installed by install-p2p.sh);
# without an argument it prints the current mode
MAC=$P2PMAC
EOF
cat >> /usr/sbin/acer-p2p-mode <<'EOF'
MODE_FILE=/etc/acer-p2p.mode
BUSY=/tmp/acer-p2p-mode.busy
if [ -z "$1" ]; then
	cat "$MODE_FILE" 2>/dev/null || echo fallback
	exit 0
fi
case "$1" in off|fallback|only) ;; *) echo "usage: acer-p2p-mode [off|fallback|only]" >&2; exit 1 ;; esac
. /etc/mice-beacon.conf
touch "$BUSY"
echo "$1" > "$MODE_FILE"
sh /root/beacon-status.sh 2>/dev/null
if [ "$1" = off ]; then
	/etc/init.d/acer-p2p stop
	/etc/init.d/acer-p2p disable
	ADDR=""; ONLY=0
else
	/etc/init.d/acer-p2p enable
	/etc/init.d/acer-p2p start
	ADDR="$MAC"; ONLY=0
	[ "$1" = only ] && ONLY=1
fi
# the beacon: whom Windows is told about (mice-beacon.sh ends with a log read that can hang)
T=""; command -v timeout >/dev/null && T="timeout 90"
P2P_ADDR="$ADDR" ONLY_P2P="$ONLY" $T sh /root/mice-beacon.sh "$NAME" "$IP" 6 >/dev/null 2>&1
rm -f "$BUSY"
sh /root/beacon-status.sh 2>/dev/null
logger -t acer-p2p "mode $1"
EOF
chmod 755 /usr/sbin/acer-p2p-mode

# web hook for the receiver's panel: GET reads the mode, POST (body off|fallback|only) switches
# it in the background (takes about 20 s); only the receiver's address may switch
cat > /www/cgi-bin/p2p-mode <<EOF
#!/bin/sh
RECEIVER=$RECEIVER
EOF
cat >> /www/cgi-bin/p2p-mode <<'EOF'
printf 'Content-Type: application/json\r\n\r\n'
BUSY=false; [ -f /tmp/acer-p2p-mode.busy ] && BUSY=true
if [ "$REQUEST_METHOD" = "POST" ]; then
	[ "$REMOTE_ADDR" = "$RECEIVER" ] || { echo '{"ok":false,"error":"only the receiver may switch"}'; exit 0; }
	[ "$BUSY" = false ] || { echo '{"ok":false,"error":"busy"}'; exit 0; }
	[ "${CONTENT_LENGTH:-0}" -le 16 ] 2>/dev/null || { echo '{"ok":false,"error":"bad request"}'; exit 0; }
	WANT="$(head -c "${CONTENT_LENGTH:-0}" | tr -dc 'a-z')"
	case "$WANT" in
		off|fallback|only) ;;
		*) echo '{"ok":false,"error":"mode must be off, fallback or only"}'; exit 0 ;;
	esac
	touch /tmp/acer-p2p-mode.busy
	( /usr/sbin/acer-p2p-mode "$WANT" </dev/null >/dev/null 2>&1 & )
	echo "{\"ok\":true,\"mode\":\"$WANT\",\"busy\":true}"
	exit 0
fi
echo "{\"ok\":true,\"mode\":\"$(/usr/sbin/acer-p2p-mode)\",\"busy\":$BUSY}"
EOF
chmod 755 /www/cgi-bin/p2p-mode

# the daemon script changed: restart it, then apply the mode (service and beacon)
[ "$MODE" = off ] || /etc/init.d/acer-p2p restart
/usr/sbin/acer-p2p-mode "$MODE"

sleep 4
echo "---- P2P interface"; iw dev "$IF" info | grep -E 'addr|type|ssid|channel'
echo "---- status"; cat /tmp/p2p.json
echo "---- mode"; /usr/sbin/acer-p2p-mode
