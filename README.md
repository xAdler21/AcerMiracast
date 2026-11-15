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
| `wake_url` | Wake-on-LAN CGI on the box (projector off = not on the network) |
| `video_mode` | `passthrough` (default) or `transcode` |
| `video_width`, `video_height`, `video_fps` | picture the projector takes (1280x720x30 on the Acer P5) |
| `beacon_host` | IP of the beacon box |

Ports: TCP 8445 (panel), TCP 7250 and UDP 50000 (from the Windows device); the receiver connects
out to the Windows device on TCP 7236 and to the projector on TCP 63630, 2425 and UDP 2425.

Panel for the Plintus app: `http://10.21.0.232:8445/` in the hub under Einstellungen -> Beamer.

## Updates through the Plintus hub

```powershell
.\deploy\build-release.ps1 -Bump   # raise VERSION, build release/acermiracast.tar.gz + release/latest.json
```

Commit and push `VERSION` and `release/`; the hub shows the update under Updates -> Beamer.
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
