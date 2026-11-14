#!/bin/bash
# Install or update the Miracast receiver with its web panel on the VM (Ubuntu).
# Run as root from the unpacked package: sudo bash install.sh [allowed-subnet ...]
# Follows the layout of the other services on the Plintus VM:
#   /opt/acermiracast            program (root-owned, read-only for the service)
#   /etc/acermiracast/config.json settings (kept on update)
#   user acermiracast            unprivileged service account
#   acermiracast.service         systemd unit
set -euo pipefail
cd "$(dirname "$0")"

SUBNETS=("$@")
[ ${#SUBNETS[@]} -gt 0 ] || SUBNETS=(10.21.0.0/24 192.168.2.0/24)

if ! command -v ffmpeg >/dev/null; then
	echo "== ffmpeg fehlt, pruefe die Installation"
	apt-get update -qq
	PLAN="$(apt-get install -s --no-install-recommends ffmpeg)"
	# Only brand-new packages are acceptable: no removals, no upgrades of what is installed.
	if echo "$PLAN" | grep -Eq '^(Remv |Inst [^ ]+ \[)'; then
		echo "ABBRUCH: ffmpeg wuerde vorhandene Pakete aendern oder entfernen:" >&2
		echo "$PLAN" | grep -E '^(Remv |Inst [^ ]+ \[)' >&2
		exit 1
	fi
	echo "$PLAN" | grep -E '^[0-9]+ upgraded' || true
	DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends ffmpeg
fi
ffmpeg -version | head -n 1

id acermiracast >/dev/null 2>&1 || useradd --system --home-dir /var/lib/acermiracast \
	--no-create-home --shell /usr/sbin/nologin acermiracast

install -d -m 755 /opt/acermiracast /etc/acermiracast
install -m 644 miracast_sink.py mice_listen.py panel.py panel.html /opt/acermiracast/
if [ ! -f /etc/acermiracast/config.json ]; then
	install -m 644 config.json /etc/acermiracast/config.json
	echo "== neue Konfiguration /etc/acermiracast/config.json"
fi

install -m 644 acermiracast.service /etc/systemd/system/acermiracast.service
systemctl daemon-reload
systemctl enable -q acermiracast.service
systemctl restart acermiracast.service

if ufw status | grep -q '^Status: active'; then
	for net in "${SUBNETS[@]}"; do
		ufw allow from "$net" to any port 8445 proto tcp comment 'acermiracast panel' >/dev/null
		ufw allow from "$net" to any port 7250 proto tcp comment 'acermiracast' >/dev/null
		ufw allow from "$net" to any port 50000 proto udp comment 'acermiracast video' >/dev/null
	done
fi

sleep 2
echo "== Dienst: $(systemctl is-active acermiracast.service)"
ss -tulnH | awk '{print $1, $5}' | grep -E ':(8445|7250)$' || true
journalctl -u acermiracast.service -n 6 --no-pager -o cat
