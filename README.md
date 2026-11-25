# AcerMiracast

Project a Windows screen with **Win+K** onto an Acer / EZCast Pro projector that has no Miracast
of its own, and control that projector from a web panel.

| Part | Where | What it does |
|---|---|---|
| `beacon/` | OpenWrt box (FRITZ!Box 4020) | Sends the Wi-Fi beacon that makes the room show up in Win+K, publishes its state as `http://<box>/beacon.json`, wakes the projector (Wake-on-LAN CGI), relays the projector's streaming ports (`install-relay.sh`). |
| `rust/` | VM | One static binary `acermiracast`: Win+K receiver (MICE TCP 7250, RTSP, RTP), picture output to the projector, web panel on port 8445. |
| `receiver/` | - | Former Python version, kept for reference; no longer deployed. |

## How the picture gets to the projector

1. Win+K: Windows connects to TCP 7250 (MS-MICE), the receiver negotiates Wi-Fi Display (RTSP)
   and asks for **H.264 Constrained Baseline 1280x720p30** - exactly what the projector plays.
2. The H.264 is taken out of the RTP/MPEG-TS stream and forwarded unchanged (no re-encoding).
   If a source insists on another format, ffmpeg re-encodes it (`video_mode` `transcode` forces that).
3. To the projector (protocol rebuilt from a capture and the Acer eDisplay Pro software):
   - control channel TCP 63630: JSON-RPC, AES-128-CBC; `common.request_stream` must be answered
     with `"allow"` (`rust/src/control.rs`)
   - UDP 2425: greeting and mirror request; TCP 2425: hello, stream request, heartbeat, one
     H.264 access unit per message (`rust/src/ezdisplay.rs`)
4. A low-rate JPEG preview for the panel comes from ffmpeg (optional).
5. While nobody projects, the projector shows a standby picture ("Win + K drücken") - only if it
   is on and already on its LAN/WiFi input, so HDMI use is never interrupted.

The projector answers its streaming ports only inside its own subnet. The box (same subnet)
relays them for the VM: `RECEIVER=10.21.0.232 PROJECTOR=10.1.34.179 sh install-relay.sh` on the
box (DNAT+SNAT, only the VM's address), and `projector_stream_host` = box address in the config.

### Wi-Fi Direct (laptop without a network path to the VM)

Without WLAN, or from an isolated guest WLAN, Windows cannot reach the VM and falls back to
Wi-Fi Direct with the device named in the beacon. `beacon/install-p2p.sh` (on the box,
`RECEIVER=10.21.0.232 sh install-p2p.sh`) makes the box that device:

- swaps `wpad-basic-mbedtls` for `hostapd-basic-mbedtls` + `wpa-supplicant-p2p` + `wpa-cli`
- runs a Wi-Fi Direct group owner `p2p0` next to the beacon on channel 6 (service `acer-p2p`),
  push button always armed, Wi-Fi Display element added as vendor element (the package has no
  `CONFIG_WIFI_DISPLAY`); the beacon announces this device's address (`P2P_ADDR`)
- gives the laptop `192.168.173.2` (no gateway, no DNS) and reports it in `http://<box>/p2p.json`
- forwards box TCP 7236 (from the VM only) to the laptop's RTSP port, and the laptop's RTP
  (UDP 50000 to the box) to the VM

The router passes the box's packets to the VM only as answers, so the VM connects out: it polls
`p2p.json`, connects to box:7236 and keeps the UDP path open by sending to box:50000
(`"wifi_direct": true`). `REMOVE=1 sh install-p2p.sh` switches it off again.

Three modes, switched in the panel (card "Funk-Sender", with an explanation of each) or on the box
with `acer-p2p-mode <mode>`; the box keeps the mode across reboots and reinstalls:

| Mode | Beacon tells Windows | Effect |
|---|---|---|
| `only` | only the Wi-Fi Direct device | every laptop uses Wi-Fi Direct at once, whatever WLAN it is in |
| `fallback` | the VM address and the Wi-Fi Direct device | Windows tries the network first and uses Wi-Fi Direct after that timed out - slow in isolated WLANs |
| `off` | only the VM address | Wi-Fi Direct stopped; only laptops whose network reaches the VM can project |

The VM stays the receiver in every mode; only the way the laptop's video reaches it changes. The
panel switches through `http://<box>/cgi-bin/p2p-mode`, which takes commands only from the
`RECEIVER` address; switching is refused while somebody projects (the beacon restarts, about 20 s).

Windows keeps the MICE connection (TCP 7250) open and silent for the whole projection; closing
it ends the projection, so it has no read timeout.

## Build and deploy

Needs Rust with the target `x86_64-unknown-linux-musl` (`rustup target add x86_64-unknown-linux-musl`);
linking uses the bundled `rust-lld`, no C toolchain. Build output goes to `C:\RE\AcerMiracast\cargo-target`.

```powershell
.\deploy\vm\push.ps1        # build, copy and install on the VM in one SSH session
```

`install.sh` installs to `/opt/acermiracast`, runs as user `acermiracast` (`acermiracast.service`),
installs ffmpeg only if that changes no existing package, and opens the ports in ufw.

Settings in `/etc/acermiracast/config.json` (kept on update, new keys are added):

| Key | Meaning |
|---|---|
| `room_name` | name sent to the projector and shown in the panel |
| `projector_host` | IP of the projector (web interface) |
| `projector_stream_host` | address for the streaming ports: the relaying box (empty = projector_host) |
| `standby` | standby picture while nobody projects (default true; needs ffmpeg) |
| `projector_control` | `acer_web`: switch to the LAN/WiFi input and show status via the projector's web page |
| `projector_mac` | MAC of the projector: the VM sends the Wake-on-LAN packet itself (reaches the projector only while the router still knows its MAC; directed broadcasts are not routed) |
| `wake_url` | Wake-on-LAN CGI on the box (projector off = not on the network); always used as well, it reaches the projector from inside its subnet |
| `video_mode` | `passthrough` (default) or `transcode` |
| `video_width`, `video_height`, `video_fps` | picture the projector takes (1280x720x30 on the Acer P5) |
| `beacon_host` | IP of the beacon box |
| `wifi_direct` | take Win+K connections that come over Wi-Fi Direct through the box (needs `install-p2p.sh` on the box) |
| `audio_target` | where the sound of a session goes (`rust/src/audio.rs`); for now only `none` (dropped, level shown in the panel) |

Ports: TCP 8445 (panel), TCP 7250 and UDP 50000 (from the Windows device); the receiver connects
out to the Windows device on TCP 7236 and to the projector on TCP 63630, 2425 and UDP 2425.

The panel has two pages on the same port, without a login of its own (the HTTP interface stays
open for Home Assistant). The button "Verwaltung" at the top of `/` leads to `/admin`, "Zurück"
leads back; both keep `?name=` and ask first while a screen share of this page runs (leaving the
page ends it):

| Page | Content |
|---|---|
| `/` | power, hide, freeze, inputs (shown also while the projector is off), screen sharing, who presents (preview, sound level, "Übertragung beenden"), volume of the transmission, disconnecting presenters, requests of other presenters |
| `/admin` | everything else, nothing twice: picture settings, remote-control keys, split screen and moderator, AirView (screenshot of what the projector shows), administration of the Acer web interface and the EZCast module, sound target, beacon box and Wi-Fi Direct mode |

The admin page shows its sections also while the projector is off, with a note: switched off it
is not on the network, so nothing of it can be read or changed then.

Besides the Acer pages (LAN, login, e-mail alerts, Crestron) and the EZCast functions with a page
of their own (Wi-Fi, hotspot password, firmware, reboot, reset), the admin page offers the EZCast
commands that only the firmware's unused "Device Management" and admin scripts know: hidden networks and WPA2-Enterprise (PEAP), IP
address in the Wi-Fi, hiding the hotspot name, maximum connections, hotspot channel 2.4/5 GHz,
castcode, Miracode, Internet access through the hotspot, hiding the hotspot password on the
picture, router-only mode, rights of the moderator, AirView, HDMI-CEC, SNMP on/off, password of the
EZCast pages and the firmware update server. No shipped page calls them, so the panel reads each
one first (`ez_read`, when the admin page opens) and leaves out what the firmware does not answer;
after every change it reads the value back. Not offered because the firmware lacks the CGI:
device name, resolution, language, default mode, EQ, autoplay, USB/AirDisk, RS232 control,
EAP-TLS certificates.

### Screen sharing from the panel

"Bildschirm teilen" sends the screen of the PC the panel runs on, without Win+K
(`rust/src/share.rs`, `rust/src/ws.rs`):

1. The browser captures the screen (`getDisplayMedia`), scales it to 1280x720 (black bars keep
   the aspect ratio) and encodes H.264 Constrained Baseline itself (WebCodecs `avc1.42E01F`,
   Annex B, about 3.5 Mbit/s, a key frame every 2 s and whenever the projector asks for one).
2. Each access unit goes as a binary WebSocket message to `GET /api/share` on port 8445
   (`0x01 key pts_us | H.264`); sound as `0x02 channels pts_us rate | PCM s16le`.
3. The VM passes the video to the projector unchanged, like the Win+K passthrough; it only lowers
   `level_idc` in the SPS to 3.1 (hardware encoders write 4.1).

The share occupies the receiver like a Win+K session. If somebody presents, the panel asks first
and ends that session (anybody may take over). The WebSocket only accepts pages of the panel
itself (Origin = Host).

Browsers offer screen capture and WebCodecs only on secure pages (https or localhost). The panel
stays on http, so the browser must treat `http://10.21.0.232:8445` as secure: the Plintus app
(WebView2) starts with `--unsafely-treat-insecure-origin-as-secure=http://10.21.0.232:8445`; in
Chrome or Edge the same is set under `chrome://flags` -> "Insecure origins treated as secure".

### Sound

The sound of both sources ends in one sink (`rust/src/audio.rs`): Win+K sends LPCM 48 kHz stereo
in the MPEG-TS (stream type 0x83, read in `rust/src/ts.rs`), the screen sharing sends PCM from
the chosen source (none, system sound of the shared screen, or an input device; a specific
Windows output device needs a virtual audio cable as input). The sink shows the level in the
panel (`GET /api/audio`) and hands the samples with their timestamps to the target in
`audio_target`. The only target so far is `none`; new targets implement `AudioTarget`.

The panel covers the projector's own web pages and the Acer eDisplay Pro functions that work
with this firmware:

- picture settings of the Acer control page (mode, brightness, contrast, gamma, colour
  temperature, keystone, aspect, zoom, volume, projection, start screen, hide, freeze, eco, resync, reset)
- remote-control keys and volume of the transmission (through the open projector connection),
  presenters on the projector (split screen, moderator, disconnect), requests of other presenters
- administration (`rust/src/admin.rs`): LAN, Wi-Fi client and hotspot of the EZCast module,
  login of the Acer web pages, e-mail alerts, Crestron, EZCast firmware update, reboot, factory reset

Passwords are never shown: the projector hands them out to anyone, the panel only sets new ones.

Panel for the Plintus app: `http://10.21.0.232:8445/` in the hub under Einstellungen -> Beamer.

### Home Assistant

Complete guide with sensors, switches, input selection, a wall tablet dashboard and automations:
[docs/home-assistant.md](docs/home-assistant.md). In short: `GET /api/state` gives `{"power": "on|off|warming|cooling", "input": "hdmi1", "hide": false,
"freeze": false, "presenter": "YOGA"}`; `POST /api/projector` (JSON) takes the commands:

| action | value |
|---|---|
| `power` | `on`, `off`, `toggle` |
| `input` | `hdmi1`, `hdmi2`, `wireless`, `vga1`, `vga2`, `video`, `media`, `usb` |
| `hide`, `freeze` | `on`, `off`, `toggle` (the projector's button only toggles; on/off reads its state first) |

The JSON content type is required on purpose: a web page in a browser cannot send it to the
panel without a preflight, which the panel never answers.

## Updates through the Plintus hub

Raise `VERSION` and push to `main`: GitHub Actions (`.github/workflows/release.yml`) runs the
tests, builds the static binary and publishes release `v<version>` with `acermiracast.tar.gz` and
`latest.json`, always also at `https://github.com/xAdler21/AcerMiracast/releases/latest/download/<name>`.
A push without a new `VERSION` builds nothing. The hub shows the update under Updates -> Beamer.

```powershell
.\deploy\build-release.ps1 -Bump   # optional local test package in C:\RE\AcerMiracast\dist
```

The unit file is not changed by hub updates: the switch from the Python version to the binary
needs `push.ps1` once.

## Tests

```powershell
cd rust
cargo test                                    # protocol bytes checked against the capture
cargo run --release --example fake_projector  # stand-in projector on 127.0.0.1
cargo run --release -- --config test.json --bind 127.0.0.1 --no-preview   # projector_host 127.0.0.1
py -I testing\fake_source.py                  # stand-in Windows source with synthetic H.264
```
