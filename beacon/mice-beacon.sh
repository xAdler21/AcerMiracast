#!/bin/sh
# mice-beacon.sh - make an OpenWrt box advertise a Miracast-over-Infrastructure (MS-MICE) sink.
#
# The box only sends beacons / probe responses. The actual receiver is a server on the LAN whose
# IP address is carried inside the beacon. Nobody can join the network (random WPA2 key).
#
# Usage:   sh mice-beacon.sh <name> <receiver-ip> [channel]
#   name         shown in Win+K. ASCII letters, digits, '-' and '_' only, max 22 chars, no '.'
#   receiver-ip  IPv4 address of the machine listening on TCP 7250
#   channel      1, 6 or 11 (default 6) - the Wi-Fi Direct social channels
#
# Environment:
#   DRY_RUN=1    only print what would be configured
#   MICE_CAP=05  MS-MICE capability byte (05 = MICE supported, protocol version 1)
#   SSID=...     override the SSID (default DIRECT-MC-<name>)
#   RADIO=radio0 wifi-device to use
#   REMOVE=1     remove the beacon again (no other arguments needed)
#   P2P_ADDR=..  P2P device address to announce (the Wi-Fi Direct interface of install-p2p.sh)
#   ONLY_P2P=1   leave out the receiver address (MS-MICE): Windows connects over Wi-Fi Direct at
#                once instead of trying the network first (needs P2P_ADDR)

set -e

RADIO="${RADIO:-radio0}"
SECTION="mice"

if [ "${REMOVE:-0}" = "1" ]; then
	uci -q delete "wireless.$SECTION" || true
	uci commit wireless
	wifi reload
	rm -f /etc/mice-beacon.conf
	echo "Beacon removed."
	exit 0
fi

NAME="$1"
IP="$2"
CHANNEL="${3:-6}"
MICE_CAP="${MICE_CAP:-05}"

die() { echo "ERROR: $*" >&2; exit 1; }

[ -n "$NAME" ] && [ -n "$IP" ] || die "usage: sh mice-beacon.sh <name> <receiver-ip> [channel]"

case "$NAME" in
	*[!A-Za-z0-9_-]*) die "name may only contain A-Z a-z 0-9 - _ (no spaces, umlauts or '.')" ;;
esac
[ "${#NAME}" -le 22 ] || die "name is longer than 22 characters"

echo "$IP" | grep -Eq '^([0-9]{1,3}\.){3}[0-9]{1,3}$' || die "receiver-ip is not an IPv4 address"
case "$CHANNEL" in 1|6|11) ;; *) die "channel must be 1, 6 or 11" ;; esac
echo "$MICE_CAP" | grep -Eq '^[0-9a-fA-F]{2}$' || die "MICE_CAP must be two hex digits"
ONLY_P2P="${ONLY_P2P:-0}"
[ "$ONLY_P2P" = "0" ] || [ -n "$P2P_ADDR" ] || die "ONLY_P2P=1 needs P2P_ADDR (Wi-Fi Direct from install-p2p.sh)"

SSID="${SSID:-DIRECT-MC-$NAME}"
[ "${#SSID}" -le 32 ] || die "SSID is longer than 32 characters"

# ---- hex helpers -------------------------------------------------------------------------------
hexstr() { printf '%s' "$1" | hexdump -v -e '/1 "%02x"'; }   # ASCII string -> hex
bytes()  { echo $(( ${#1} / 2 )); }                           # number of bytes in a hex string
be16()   { printf '%04x' "$1"; }                              # 16 bit big endian
le16()   { printf '%02x%02x' $(( $1 & 255 )) $(( ($1 >> 8) & 255 )); }
# one 802.11 vendor specific element: dd <len> <oui+type> <body>
element() {
	_n=$(bytes "$1$2")
	[ "$_n" -le 255 ] || die "information element too long ($_n bytes)"
	printf 'dd%02x%s%s' "$_n" "$1" "$2"
}
wsc_attr() { printf '%s%s%s' "$1" "$(be16 "$(bytes "$2")")" "$2"; }   # WSC TLV: id(2) len(2 BE)
p2p_attr() { printf '%s%s%s' "$1" "$(le16 "$(bytes "$2")")" "$2"; }   # P2P TLV: id(1) len(2 LE)

# ---- radio MAC (used as P2P device address) ----------------------------------------------------
MAC=""
for p in /sys/class/ieee80211/*; do
	[ -e "$p/macaddress" ] || continue
	MAC="$(cat "$p/macaddress")"
	break
done
if [ -z "$MAC" ]; then
	if [ "${DRY_RUN:-0}" = "1" ]; then
		MAC="02:00:00:00:00:01"
	else
		die "could not read the radio MAC address"
	fi
fi
# With Wi-Fi Direct on the box (install-p2p.sh) the beacon names the real P2P device, so
# Windows falls back to it when it cannot reach the receiver over the network.
[ -n "$P2P_ADDR" ] && MAC="$P2P_ADDR"
MAC_HEX="$(echo "$MAC" | tr -d ':' | tr 'A-F' 'a-f')"

NAME_HEX="$(hexstr "$NAME")"
IP_HEX="$(hexstr "$IP")"

# Primary device type: category 7 (Displays), WFA OUI 00 50 f2 04, sub category 3 (Projector)
DEV_TYPE="00070050f2040003"

# ---- 1. Wi-Fi Direct (P2P) element, OUI 50 6f 9a type 09 ---------------------------------------
#   02 P2P Capability   : device capability 25, group capability 01 (group owner)
#   03 P2P Device ID    : device address
#   0d P2P Device Info  : address, config methods, primary device type, 0 secondary, device name
P2P_CAP="$(p2p_attr 02 "${P2P_DEV_CAP:-25}${P2P_GRP_CAP:-01}")"
P2P_ID="$(p2p_attr 03 "$MAC_HEX")"
P2P_INFO="$(p2p_attr 0d "${MAC_HEX}0080${DEV_TYPE}00$(wsc_attr 1011 "$NAME_HEX")")"
IE_P2P="$(element 506f9a09 "$P2P_CAP$P2P_ID$P2P_INFO")"

# ---- 2. Wi-Fi Display (Miracast) element, OUI 50 6f 9a type 0a ---------------------------------
#   subelement 00 (WFD Device Information), length 6:
#     0011  primary sink, available for session
#     1c44  RTSP control port 7236
#     00c8  max throughput 200 Mbit/s
IE_WFD="$(element 506f9a0a "0000060011${WFD_PORT_HEX:-1c44}00c8")"

# ---- 3. WSC (WPS) element, OUI 00 50 f2 type 04, with the MS-MICE vendor extension --------------
#   104a Version 0x10, 1044 WSC state 2 (configured), 1054 primary device type, 1011 device name
#   1049 Vendor Extension, OUI 00 01 37 (Microsoft):
#        2001 Capability, 2002 Host Name (ASCII), 2005 IP Address (ASCII dotted decimal)
MICE="000137$(wsc_attr 2001 "$MICE_CAP")$(wsc_attr 2002 "$NAME_HEX")$(wsc_attr 2005 "$IP_HEX")"
WSC="$(wsc_attr 104a 10)$(wsc_attr 1044 02)$(wsc_attr 1054 "$DEV_TYPE")$(wsc_attr 1011 "$NAME_HEX")"
# without the Microsoft extension Windows sees a plain Miracast sink and uses Wi-Fi Direct
[ "$ONLY_P2P" = "0" ] && WSC="$WSC$(wsc_attr 1049 "$MICE")"
IE_WSC="$(element 0050f204 "$WSC")"

VENDOR_ELEMENTS="$IE_P2P$IE_WFD$IE_WSC"

# random key nobody knows: the network exists only to carry the beacon
KEY="$(head -c 24 /dev/urandom | hexdump -v -e '/1 "%02x"')"

echo "Name        : $NAME"
echo "Receiver IP : $IP"
echo "SSID        : $SSID"
echo "Channel     : $CHANNEL"
echo "Radio MAC   : $MAC"
echo "Only P2P    : $ONLY_P2P"
echo "P2P element : $IE_P2P"
echo "WFD element : $IE_WFD"
echo "WSC element : $IE_WSC"
echo "vendor_elements ($(bytes "$VENDOR_ELEMENTS") bytes):"
echo "$VENDOR_ELEMENTS"

if [ "${DRY_RUN:-0}" = "1" ]; then
	echo "DRY_RUN=1 - nothing changed."
	exit 0
fi

# ---- apply --------------------------------------------------------------------------------------
uci -q delete "wireless.$SECTION" || true
uci set "wireless.$SECTION=wifi-iface"
uci set "wireless.$SECTION.device=$RADIO"
uci set "wireless.$SECTION.mode=ap"
uci set "wireless.$SECTION.ssid=$SSID"
uci set "wireless.$SECTION.encryption=psk2+ccmp"
uci set "wireless.$SECTION.key=$KEY"
uci set "wireless.$SECTION.isolate=1"
uci set "wireless.$SECTION.wps_pushbutton=0"
uci set "wireless.$SECTION.vendor_elements=$VENDOR_ELEMENTS"

uci set "wireless.$RADIO.channel=$CHANNEL"
uci set "wireless.$RADIO.htmode=HT20"
uci set "wireless.$RADIO.disabled=0"

# keep the open default network off
if uci -q get wireless.default_radio0 >/dev/null; then
	uci set wireless.default_radio0.disabled=1
fi

uci commit wireless
wifi reload

# remembered for beacon-status.sh
printf 'NAME="%s"\nIP="%s"\nP2P_ADDR="%s"\nONLY_P2P="%s"\n' "$NAME" "$IP" "$P2P_ADDR" "$ONLY_P2P" > /etc/mice-beacon.conf

echo "Applied. Waiting for the radio ..."
sleep 10
echo "---- iw dev"
iw dev
echo "---- hostapd log"
logread | grep -i hostapd | tail -n 12
