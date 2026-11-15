#!/bin/sh
# Relay for the projector's EZ Display ports, run on the OpenWrt box.
#
# The Acer / EZCast projector answers its streaming ports (UDP 2425, TCP 2425, TCP 63630) only
# for devices in its own subnet. The receiver VM is in another subnet, so it talks to the box
# instead; the box forwards to the projector with its own address as sender (DNAT + SNAT).
# Only the receiver's address is forwarded.
#
#   RECEIVER=10.21.0.232 PROJECTOR=10.1.34.179 sh install-relay.sh      set up / update
#   REMOVE=1 sh install-relay.sh                                         remove
set -eu

RECEIVER="${RECEIVER:-10.21.0.232}"
PROJECTOR="${PROJECTOR:-10.1.34.179}"
BOX="$(uci -q get network.lan.ipaddr | cut -d/ -f1)"

for s in acer_relay_2425 acer_relay_63630 acer_relay_snat; do
	uci -q delete "firewall.$s" || true
done

if [ "${REMOVE:-0}" != 1 ]; then
	uci batch <<EOF
set firewall.acer_relay_2425=redirect
set firewall.acer_relay_2425.name='AcerMiracast-2425'
set firewall.acer_relay_2425.src='lan'
set firewall.acer_relay_2425.src_ip='$RECEIVER'
set firewall.acer_relay_2425.src_dip='$BOX'
set firewall.acer_relay_2425.src_dport='2425'
set firewall.acer_relay_2425.dest='lan'
set firewall.acer_relay_2425.dest_ip='$PROJECTOR'
set firewall.acer_relay_2425.dest_port='2425'
set firewall.acer_relay_2425.proto='tcp udp'
set firewall.acer_relay_2425.target='DNAT'
set firewall.acer_relay_63630=redirect
set firewall.acer_relay_63630.name='AcerMiracast-63630'
set firewall.acer_relay_63630.src='lan'
set firewall.acer_relay_63630.src_ip='$RECEIVER'
set firewall.acer_relay_63630.src_dip='$BOX'
set firewall.acer_relay_63630.src_dport='63630'
set firewall.acer_relay_63630.dest='lan'
set firewall.acer_relay_63630.dest_ip='$PROJECTOR'
set firewall.acer_relay_63630.dest_port='63630'
set firewall.acer_relay_63630.proto='tcp'
set firewall.acer_relay_63630.target='DNAT'
set firewall.acer_relay_snat=nat
set firewall.acer_relay_snat.name='AcerMiracast-snat'
set firewall.acer_relay_snat.src='lan'
set firewall.acer_relay_snat.src_ip='$RECEIVER'
set firewall.acer_relay_snat.dest_ip='$PROJECTOR'
set firewall.acer_relay_snat.proto='all'
set firewall.acer_relay_snat.target='SNAT'
set firewall.acer_relay_snat.snat_ip='$BOX'
EOF
fi
uci commit firewall
fw4 -q reload
if [ "${REMOVE:-0}" = 1 ]; then
	echo "relay removed"
else
	echo "relay: $RECEIVER -> $BOX:2425,63630 -> $PROJECTOR"
	nft list ruleset | grep -E 'AcerMiracast' || true
fi
