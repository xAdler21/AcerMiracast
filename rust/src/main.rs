//! AcerMiracast: Win+K (MS-MICE, Miracast over infrastructure) receiver that forwards the
//! picture to an Acer / EZCast Pro projector, plus a web panel for projector and beacon box.
//!
//!   acermiracast [--config /etc/acermiracast/config.json] [--bind 0.0.0.0] [--port 7250]
//!                [--rtp-port 50000] [--http-port 8445] [--no-preview]
//!
//! Flow of one session:
//!   1. Windows connects to TCP 7250 and sends SOURCE_READY (friendly name, RTSP port).
//!   2. We connect back to the source's RTSP port and negotiate Wi-Fi Display.
//!   3. The source sends RTP/MPEG-TS (H.264) to our UDP port.
//!   4. The H.264 goes to the projector (ezdisplay.rs), a JPEG preview to the panel.

mod admin;
mod audio;
mod control;
mod ezdisplay;
mod http;
mod hub;
mod media;
mod panel;
mod session;
mod share;
mod ts;
mod ws;

use std::io::Read;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use hub::{Hub, SessionInfo, log};
use media::VideoSize;
use session::{Options, Session};

#[derive(Clone, Default)]
pub struct Config {
    pub room_name: String,
    pub projector_host: String,
    pub projector_stream_host: String,
    pub projector_model: String,
    pub projector_mac: String,
    pub standby: bool,
    pub standby_font: String,
    pub wake_url: String,
    pub beacon_host: String,
    pub wifi_direct: bool,
    pub picture_output: String,
    pub video_mode: String,
    pub video_width: u32,
    pub video_height: u32,
    pub video_fps: u32,
    pub select_input: bool,
    /// where the sound of a session goes (audio.rs); "none" = dropped
    pub audio_target: String,
}

impl Config {
    fn load(path: &str) -> Config {
        let v: Value = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log(format!("config {path} is not valid JSON ({e}), using defaults"));
                Value::Null
            }),
            Err(_) => {
                log(format!("no config file {path}, running without projector and beacon"));
                Value::Null
            }
        };
        let s = |k: &str, d: &str| v.get(k).and_then(Value::as_str).unwrap_or(d).to_string();
        let n = |k: &str, d: u32| v.get(k).and_then(Value::as_u64).map(|x| x as u32).unwrap_or(d);
        Config {
            room_name: s("room_name", "Plintus"),
            projector_host: s("projector_host", ""),
            projector_stream_host: s("projector_stream_host", ""),
            projector_model: s("projector_model", ""),
            projector_mac: s("projector_mac", ""),
            standby: v.get("standby").and_then(Value::as_bool).unwrap_or(true),
            standby_font: s("standby_font", "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"),
            wake_url: s("wake_url", ""),
            beacon_host: s("beacon_host", ""),
            wifi_direct: v.get("wifi_direct").and_then(Value::as_bool).unwrap_or(false),
            picture_output: s("picture_output", "ezdisplay"),
            video_mode: s("video_mode", "passthrough"),
            video_width: n("video_width", 1280),
            video_height: n("video_height", 720),
            video_fps: n("video_fps", 30),
            select_input: s("projector_control", "acer_web") == "acer_web",
            audio_target: s("audio_target", "none"),
        }
    }
}

struct Args {
    config: String,
    bind: String,
    port: u16,
    rtp_port: u16,
    http_port: u16,
    preview: bool,
}

fn args() -> Args {
    let mut a = Args {
        config: "/etc/acermiracast/config.json".into(),
        bind: "0.0.0.0".into(),
        port: 7250,
        rtp_port: 50000,
        http_port: 8445,
        preview: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--config" => a.config = value(),
            "--bind" => a.bind = value(),
            "--port" => a.port = value().parse().expect("--port"),
            "--rtp-port" => a.rtp_port = value().parse().expect("--rtp-port"),
            "--http-port" => a.http_port = value().parse().expect("--http-port"),
            "--no-preview" => a.preview = false,
            "-h" | "--help" => {
                println!("acermiracast [--config FILE] [--bind ADDR] [--port 7250] [--rtp-port 50000] [--http-port 8445] [--no-preview]");
                std::process::exit(0);
            }
            other => panic!("unknown option {other}"),
        }
    }
    a
}

// ---- MICE control channel (TCP 7250) ----------------------------------------------------------

fn set_keepalive(conn: &TcpStream) {
    let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(30)).with_interval(Duration::from_secs(10));
    let _ = socket2::SockRef::from(conn).set_tcp_keepalive(&ka);
}

const MICE_SOURCE_READY: u8 = 0x01;
const MICE_STOP_PROJECTION: u8 = 0x02;

struct Mice {
    hub: Arc<Hub>,
    options: Arc<Options>,
    output: Option<ezdisplay::Output>,
    current: Mutex<Option<(IpAddr, Arc<AtomicBool>)>>,
}

impl Mice {
    fn handle(self: Arc<Self>, mut conn: TcpStream) {
        let Ok(peer) = conn.peer_addr() else { return };
        log(format!("MICE: connection from {peer}"));
        // No read timeout: Windows keeps this connection open and silent for the whole
        // projection; closing it ends the projection (that was the drop after ~70 s).
        // Dead peers are found by TCP keep-alive.
        let _ = conn.set_read_timeout(None);
        set_keepalive(&conn);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = match conn.read(&mut chunk) {
                Ok(0) => {
                    log(format!("MICE: {} closed the connection", peer.ip()));
                    return;
                }
                Ok(n) => n,
                Err(e) => {
                    log(format!("MICE: connection from {} lost: {e}", peer.ip()));
                    return;
                }
            };
            buf.extend_from_slice(&chunk[..n]);
            while buf.len() >= 4 {
                let size = u16::from_be_bytes([buf[0], buf[1]]) as usize;
                if size < 4 {
                    log(format!("MICE: bad size field from {}", peer.ip()));
                    return;
                }
                if buf.len() < size {
                    break;
                }
                let msg: Vec<u8> = buf.drain(..size).collect();
                self.clone().on_message(peer.ip(), &msg);
            }
        }
    }

    fn on_message(self: Arc<Self>, ip: IpAddr, msg: &[u8]) {
        let command = msg[3];
        let mut name = String::new();
        let mut port = 7236u16;
        let mut pos = 4;
        while pos + 3 <= msg.len() {
            let t = msg[pos];
            let len = u16::from_be_bytes([msg[pos + 1], msg[pos + 2]]) as usize;
            let value = &msg[pos + 3..(pos + 3 + len).min(msg.len())];
            match t {
                0x00 => {
                    let units: Vec<u16> = value.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    name = String::from_utf16_lossy(&units);
                }
                0x02 if value.len() == 2 => port = u16::from_be_bytes([value[0], value[1]]),
                _ => {}
            }
            pos += 3 + len;
        }
        match command {
            MICE_SOURCE_READY => {
                log(format!("MICE {ip}: SOURCE_READY from '{name}', RTSP port {port}"));
                self.start_session(ip, port, name, ip.to_string(), None);
            }
            MICE_STOP_PROJECTION => {
                log(format!("MICE {ip}: STOP_PROJECTION from '{name}'"));
                if let Some((owner, stop)) = self.current.lock().unwrap().as_ref() {
                    if *owner == ip {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
            other => log(format!("MICE {ip}: command 0x{other:02x} ignored")),
        }
    }

    /// Connects to the source's RTSP port and runs the session; false if one is running.
    fn start_session(self: Arc<Self>, ip: IpAddr, port: u16, name: String, shown_ip: String, punch: Option<SocketAddr>) -> bool {
        let info = SessionInfo {
            source: name.clone(),
            ip: shown_ip,
            playing: false,
            started: Instant::now(),
            frames: 0,
            video: String::new(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        if let Err(busy) = self.hub.begin_session(info, stop.clone()) {
            log(format!("busy with '{busy}', ignoring '{name}'"));
            return false;
        }
        *self.current.lock().unwrap() = Some((ip, stop.clone()));
        thread::spawn(move || {
            log(format!("session start: '{name}' from {ip}"));
            let session = Session { source_ip: ip, rtsp_port: port, name, stop, punch };
            session.run(&self.options, self.hub.clone(), self.output.clone());
            *self.current.lock().unwrap() = None;
            self.hub.end_session();
        });
        true
    }
}

// ---- Wi-Fi Direct through the beacon box ------------------------------------------------------
//
// Without a network path to the VM, Windows connects to the box over Wi-Fi Direct. The box
// (beacon/install-p2p.sh) is group owner, gives the laptop a fixed address and reports it in
// http://<box>/p2p.json. The box forwards its TCP 7236 to the laptop's RTSP port and the
// laptop's RTP (to the box, UDP 50000) to us. The router passes the box's packets to the VM
// only as answers, so we connect out to the box and keep the UDP path open ourselves.

fn wifi_direct(mice: Arc<Mice>, box_host: String, rtp_port: u16) {
    let Ok(box_ip) = box_host.parse::<IpAddr>() else {
        log(format!("wifi_direct: {box_host} is not an IP address"));
        return;
    };
    log(format!("Wi-Fi Direct: watching {box_host} for connected laptops"));
    let mut last: Option<(String, Instant)> = None;
    loop {
        thread::sleep(Duration::from_secs(2));
        if mice.hub.session().is_some() {
            continue;
        }
        let status = http::request("GET", &format!("http://{box_host}/p2p.json"), b"", "", Duration::from_secs(3))
            .ok()
            .filter(|(code, _)| *code == 200)
            .and_then(|(_, body)| serde_json::from_slice::<Value>(&body).ok());
        let peer = status.as_ref().and_then(|v| v["peer"].as_str()).unwrap_or("").to_string();
        if peer.is_empty() {
            last = None;
            continue;
        }
        // one attempt per connected laptop, repeated every 15 s while it stays connected
        if last.as_ref().is_some_and(|(p, t)| *p == peer && t.elapsed() < Duration::from_secs(15)) {
            continue;
        }
        last = Some((peer.clone(), Instant::now()));
        let name = status.as_ref().and_then(|v| v["name"].as_str()).filter(|n| !n.is_empty()).unwrap_or("Wi-Fi Direct").to_string();
        log(format!("Wi-Fi Direct: '{name}' ({peer}) is connected to the box"));
        mice.clone().start_session(box_ip, 7236, name, format!("{peer} (Wi-Fi Direct)"), Some(SocketAddr::new(box_ip, rtp_port)));
    }
}

fn main() {
    let a = args();
    let config = Config::load(&a.config);
    let hub = Hub::new(&config.audio_target);

    let use_projector = config.picture_output == "ezdisplay" && !config.projector_host.is_empty();
    let standby = Arc::new(Mutex::new(None));
    let output = use_projector.then(|| {
        let stream_host = if config.projector_stream_host.is_empty() {
            config.projector_host.clone()
        } else {
            config.projector_stream_host.clone()
        };
        if stream_host != config.projector_host {
            log(format!("projector streaming ports go through {stream_host}"));
        }
        ezdisplay::Output::start(
            ezdisplay::Settings {
                host: config.projector_host.clone(),
                stream_host,
                width: config.video_width,
                height: config.video_height,
                name: config.room_name.clone(),
                select_input: config.select_input,
                standby: standby.clone(),
            },
            hub.clone(),
        )
    });
    if use_projector && config.standby {
        let (w, h, room, font) = (config.video_width, config.video_height, config.room_name.clone(), config.standby_font.clone());
        thread::spawn(move || match ezdisplay::render_standby(w, h, &room, &font) {
            Ok(frame) => {
                log(format!("standby picture ready ({} bytes)", frame.len()));
                *standby.lock().unwrap() = Some(frame);
            }
            Err(e) => log(format!("no standby picture: {e}")),
        });
    }
    let passthrough = config.video_mode != "transcode";
    if use_projector {
        log(format!(
            "video goes to the projector {} as H.264 {}x{} ({})",
            config.projector_host,
            config.video_width,
            config.video_height,
            if passthrough { "passed through from the source" } else { "re-encoded by ffmpeg" }
        ));
    }
    let options = Arc::new(Options {
        rtp_port: a.rtp_port,
        idle_timeout: Duration::from_secs(70),
        passthrough: passthrough && use_projector,
        video: VideoSize { width: config.video_width, height: config.video_height, fps: config.video_fps },
        preview: a.preview.then_some((5, 960)),
    });

    if a.http_port != 0 {
        let monitor = panel::Monitor::start(config.clone(), hub.clone(), output.clone());
        let share = share::ShareOptions {
            video: VideoSize { width: config.video_width, height: config.video_height, fps: config.video_fps },
            preview: options.preview,
        };
        if let Err(e) = panel::start(&a.bind, a.http_port, monitor, hub.clone(), share) {
            log(format!("web panel could not listen on port {}: {e}", a.http_port));
            std::process::exit(1);
        }
        log(format!("web panel on http://{}:{}/", a.bind, a.http_port));
    }

    let mice = Arc::new(Mice { hub, options, output, current: Mutex::new(None) });
    if config.wifi_direct && !config.beacon_host.is_empty() {
        let (mice, host, port) = (mice.clone(), config.beacon_host.clone(), a.rtp_port);
        thread::spawn(move || wifi_direct(mice, host, port));
    }
    let listener = TcpListener::bind((a.bind.as_str(), a.port)).unwrap_or_else(|e| {
        log(format!("cannot listen on port {}: {e}", a.port));
        std::process::exit(1);
    });
    log(format!("MICE sink listening on {}:{}, RTP on UDP {}", a.bind, a.port, a.rtp_port));
    for conn in listener.incoming().flatten() {
        let mice = mice.clone();
        thread::spawn(move || mice.handle(conn));
    }
}
