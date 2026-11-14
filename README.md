# AcerMiracast

Project a Windows screen with **Win+K** onto a projector that has no Miracast of its own, and
control that projector from a web panel.

| Part | Where | What it does |
|---|---|---|
| `beacon/` | OpenWrt box (FRITZ!Box 4020) | Sends the Wi-Fi beacon that makes the room show up in Win+K, and publishes its state as `http://<box>/beacon.json`. |
| `receiver/miracast_sink.py` | VM | Accepts the connection (TCP 7250), negotiates the stream (RTSP), decodes it with ffmpeg to JPEG frames. |
| `receiver/panel.py`, `panel.html` | VM | Web panel on port 8445: projector status and control (PJLink), beacon and session status, live picture. |
| projector picture output | VM | Sends the frames to the projector. **Not built yet** - the projector protocol is not verified. |

## Beacon

```powershell
.\beacon\deploy.ps1 -ReceiverIp 10.21.0.232 -Name Plintus -DryRun   # show what would be set
.\beacon\deploy.ps1 -ReceiverIp 10.21.0.232 -Name Plintus           # switch it on
```

Uses the SSH key `~/.ssh/claude_fritzbox_beacon`. The setting survives a reboot.
Switch off on the box with `REMOVE=1 sh /root/mice-beacon.sh`.

## Receiver and panel on the VM

```powershell
.\deploy\vm\push.ps1
```

Packs the receiver and runs `deploy/vm/install.sh` on the VM in one SSH session. It installs to
`/opt/acermiracast`, runs as user `acermiracast` (`acermiracast.service`), installs ffmpeg only
if that changes no existing package, and opens the ports in ufw.

Settings are in `/etc/acermiracast/config.json` (kept on update); restart the service after a change:

| Key | Meaning |
|---|---|
| `room_name` | name shown in the panel hint |
| `projector_host` | IP of the projector (PJLink, TCP 4352); empty = not set up |
| `projector_password` | PJLink password, if the projector asks for one |
| `beacon_host` | IP of the beacon box |

Ports: TCP 8445 (panel), TCP 7250 and UDP 50000 (from the Windows device); the receiver connects
out to the Windows device on TCP 7236. The Windows device need not be in the same subnet, but
these connections must be allowed.

Panel for the Plintus app: enter `http://10.21.0.232:8445/` in the hub under Einstellungen -> Beamer.

## Tests

```sh
python3 receiver/mice_listen.py --selftest
python3 receiver/miracast_sink.py --no-decode --http-port 0 --bind 127.0.0.1 &
python3 receiver/fake_source.py --sink 127.0.0.1 [--ts video.ts]
```

`fake_source.py` plays the Windows side of the whole protocol. Not yet tested against real Windows.
