//! Picture output to an Acer / EZCast Pro projector ("EZ Display", port 2425), rebuilt from
//! a capture of the Acer software and its decompiled code. Numbers are u32 little endian.
//!
//!   1. control channel (control.rs): introduce, get role, common.request_stream -> "allow"
//!   2. UDP to 2425: "2:<t>:User:<host>:1:User\0"                -> answer command 3
//!                   "2:<t>:User:<host>:34:\0" + MIRROR_REQUEST   -> {1, 0x1001, w, h, 0, 0}
//!   3. TCP to 2425:
//!        hello      {0, n+24, 9, 0x1000, 1, 0, 0, n} + host name
//!        request    {0, 24,   6, 0x1000, 1, 0, 0, 7}             7 = H.264
//!        heartbeat  {0, 24,   4, 0, 0, 0, 0, 0}                  once a second
//!        answer     {seq, 24, 6, 1, 1, split, position, 1}       1 = allowed
//!        video      {seq, size+24} {2, 0x1000, 7, w, h, size} + one H.264 access unit

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::control::Control;
use crate::http;
use crate::hub::{Hub, log};

pub const PORT: u16 = 2425;
const CMD_VIDEO: u32 = 2;
const CMD_HEARTBEAT: u32 = 4;
const CMD_REQUEST_STREAM: u32 = 6;
const CMD_NOTIFICATION: u32 = 7;
const CMD_HELLO: u32 = 9;
const FORMAT_H264: u32 = 7;
const USER: &str = "User";

const STATE_WAITING: u32 = 0;
const STATE_ALLOWED: u32 = 1;
const STATE_DENIED: u32 = 2;

fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

pub fn message(seq: u32, command: u32, version: u32, a: u32, b: u32, c: u32, d: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = words(&[seq, 24 + payload.len() as u32, command, version, a, b, c, d]);
    out.extend_from_slice(payload);
    out
}

pub fn hello(name: &str) -> Vec<u8> {
    message(0, CMD_HELLO, 0x1000, 1, 0, 0, name.len() as u32, name.as_bytes())
}

pub fn request_stream() -> Vec<u8> {
    message(0, CMD_REQUEST_STREAM, 0x1000, 1, 0, 0, FORMAT_H264, &[])
}

pub fn heartbeat() -> Vec<u8> {
    message(0, CMD_HEARTBEAT, 0, 0, 0, 0, 0, &[])
}

pub fn video(seq: u32, unit: &[u8], width: u32, height: u32) -> Vec<u8> {
    message(seq, CMD_VIDEO, 0x1000, FORMAT_H264, width, height, unit.len() as u32, unit)
}

pub fn mirror_request() -> Vec<u8> {
    let mut out = words(&[1, 0, 48, 0]);
    out.extend(words(&[1, 0, 0, 0, 0, 0]));
    out.extend(words(&[1, 0, 0, 0, 0, 0]));
    out
}

pub fn udp_text(command: u32, host: &str) -> Vec<u8> {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as u32;
    format!("2:{t}:{USER}:{host}:{command}:").into_bytes()
}

/// Picture size from the answer to the mirror request.
pub fn parse_size(datagram: &[u8]) -> Option<(u32, u32)> {
    let pos = datagram.windows(4).position(|w| w == b":34\0")? + 4;
    let w = |i: usize| datagram.get(pos + 4 * i..pos + 4 * i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    let (tag, width, height) = (w(0)?, w(2)?, w(3)?);
    (tag == 1 && (160..=4096).contains(&width) && (120..=2400).contains(&height)).then_some((width, height))
}

#[derive(Clone)]
pub struct Settings {
    /// projector address for its web interface
    pub host: String,
    /// address for the streaming ports: the projector itself, or the box that relays to it
    /// (the projector answers them only inside its own subnet)
    pub stream_host: String,
    pub width: u32,
    pub height: u32,
    pub name: String,
    /// switch the projector to its LAN/WiFi input (Acer web interface) when someone projects
    pub select_input: bool,
    /// picture shown while nobody projects (one H.264 key frame), filled in at start-up
    pub standby: Arc<Mutex<Option<Vec<u8>>>>,
}

/// Renders the standby picture with ffmpeg as one H.264 key frame (CBP, with SPS/PPS).
pub fn render_standby(width: u32, height: u32, room: &str, font: &str) -> io::Result<Vec<u8>> {
    let esc = |t: &str| t.replace('\\', "\\\\").replace(':', "\\:").replace('\'', "\u{2019}").replace(',', "\\,");
    let text = |t: &str, size: u32, y: &str, color: &str| {
        format!("drawtext=fontfile={font}:text='{}':fontsize={size}:fontcolor={color}:x=(w-text_w)/2:y={y}", esc(t))
    };
    let filter = [
        text("Win + K drücken", height / 8, "h*0.36", "white"),
        text(&format!("und „{room}“ wählen"), height / 16, "h*0.36+th*2.6", "0xf2a93b"),
        text("Bild vom Laptop drahtlos auf den Beamer", height / 30, "h*0.82", "0x98a1b0"),
    ]
    .join(",");
    let out = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-f", "lavfi", "-i"])
        .arg(format!("color=c=0x0e1014:s={width}x{height}:r=1"))
        .args(["-frames:v", "1", "-vf", &filter, "-pix_fmt", "yuv420p", "-c:v", "libx264", "-profile:v", "baseline",
               "-level", "3.1", "-bf", "0", "-x264-params", "repeat-headers=1", "-f", "h264", "pipe:1"])
        .stderr(std::process::Stdio::inherit())
        .output()?;
    let unit = crate::ts::AccessUnits::default()
        .build(&out.stdout)
        .filter(|u| u.key)
        .ok_or_else(|| io::Error::other("ffmpeg produced no key frame"))?;
    Ok(unit.data)
}

enum Msg {
    Video(Vec<u8>, bool),
    End,
}

/// Handle used by the session; all network work happens on the output's own thread so a slow
/// projector never stalls the receiver.
#[derive(Clone)]
pub struct Output {
    tx: SyncSender<Msg>,
    overflow: Arc<AtomicBool>,
}

impl Output {
    pub fn start(settings: Settings, hub: Arc<Hub>) -> Output {
        let (tx, rx) = mpsc::sync_channel(90);
        let overflow = Arc::new(AtomicBool::new(false));
        let worker = Worker::new(settings, hub, overflow.clone());
        thread::spawn(move || worker.run(rx));
        Output { tx, overflow }
    }

    pub fn send_video(&self, unit: Vec<u8>, key: bool) {
        if let Err(TrySendError::Full(_)) = self.tx.try_send(Msg::Video(unit, key)) {
            self.overflow.store(true, Ordering::Relaxed); // restart at the next key frame
        }
    }

    pub fn session_ended(&self) {
        let _ = self.tx.send(Msg::End);
    }
}

struct Connection {
    control: Option<Control>,
    writer: Arc<Mutex<TcpStream>>,
    alive: Arc<AtomicBool>,
    state: Arc<AtomicU32>,
    seq: u32,
    sent: u64,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Standby,
    Live,
}

struct Worker {
    s: Settings,
    hub: Arc<Hub>,
    overflow: Arc<AtomicBool>,
    conn: Option<Connection>,
    need_key: bool,
    failures: u32,
    retry_at: Instant,
    mode: Mode,
    shown: bool,
    in_session: bool,
    last_video: Instant,
    next_standby: Instant,
}

impl Worker {
    fn new(s: Settings, hub: Arc<Hub>, overflow: Arc<AtomicBool>) -> Worker {
        let now = Instant::now();
        Worker {
            s, hub, overflow, conn: None, need_key: true, failures: 0, retry_at: now, mode: Mode::Standby,
            shown: false, in_session: false, last_video: now, next_standby: now,
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Msg::Video(unit, key)) => {
                    self.in_session = true;
                    self.last_video = Instant::now();
                    self.send(unit, key, Mode::Live);
                }
                Ok(Msg::End) => {
                    if let Some(c) = &self.conn {
                        log(format!("projector: session ended after {} frames on this connection", c.sent));
                    }
                    self.in_session = false;
                    self.failures = 0;
                    self.retry_at = Instant::now();
                    self.hub.set_output("");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            // standby picture while nobody projects (or the source has paused for a while)
            if (!self.in_session || self.last_video.elapsed() > Duration::from_secs(3)) && Instant::now() >= self.next_standby {
                self.next_standby = Instant::now() + Duration::from_secs(1);
                let frame = self.s.standby.lock().unwrap().clone();
                if let Some(frame) = frame {
                    self.send(frame, true, Mode::Standby);
                }
            }
        }
    }

    fn fail(&mut self, why: &str) {
        self.conn = None;
        self.failures += 1;
        // Every attempt makes the projector show "processing signal": back off, 15 s .. 2 min.
        let wait = (15u64 << (self.failures - 1).min(3)).min(120);
        self.retry_at = Instant::now() + Duration::from_secs(wait);
        log(format!("projector: {why}; next attempt in {wait} s"));
        self.hub.set_output(format!("Fehler: {why}, neuer Versuch in {wait} s"));
    }

    fn send(&mut self, unit: Vec<u8>, key: bool, mode: Mode) {
        if self.conn.is_none() {
            if Instant::now() < self.retry_at {
                return;
            }
            if mode == Mode::Standby && !self.standby_allowed() {
                self.retry_at = Instant::now() + Duration::from_secs(30);
                return;
            }
            self.hub.set_output("verbindet …");
            match self.connect(mode == Mode::Live && self.s.select_input) {
                Ok(c) => self.conn = Some(c),
                Err(e) => {
                    self.fail(&e.to_string());
                    return;
                }
            }
        }
        let c = self.conn.as_mut().unwrap();
        let control_lost = c.control.as_ref().is_some_and(|ctl| !ctl.poll());
        if !c.alive.load(Ordering::Relaxed) || control_lost {
            let what = if control_lost { "control connection closed" } else { "projector closed the connection" };
            if c.sent == 0 {
                self.fail(what);
            } else {
                log(format!("projector: {what} after {} frames, reconnecting", c.sent));
                self.conn = None;
                self.retry_at = Instant::now() + Duration::from_secs(2);
            }
            return;
        }
        match c.state.load(Ordering::Relaxed) {
            STATE_ALLOWED => {}
            STATE_DENIED => {
                self.fail("stream request denied");
                return;
            }
            _ => return,
        }
        if mode != self.mode {
            // switching between standby picture and the laptop: start at a key frame
            self.mode = mode;
            self.need_key = true;
            self.shown = false;
            if mode == Mode::Live {
                self.hub.request_idr();
            }
        }
        if self.overflow.swap(false, Ordering::Relaxed) {
            self.need_key = true;
            self.hub.request_idr();
        }
        if self.need_key {
            if !key {
                return;
            }
            self.need_key = false;
        }
        let data = video(c.seq, &unit, self.s.width, self.s.height);
        let written = c.writer.lock().unwrap().write_all(&data);
        if let Err(e) = written {
            log(format!("projector: send failed: {e}"));
            self.conn = None;
            self.retry_at = Instant::now() + Duration::from_secs(2);
            return;
        }
        c.seq = c.seq.wrapping_add(1);
        c.sent += 1;
        if !self.shown {
            self.shown = true;
            self.failures = 0;
            let what = if mode == Mode::Live { "Bild vom Laptop" } else { "Wartebild" };
            log(format!("projector: showing the {}", if mode == Mode::Live { "laptop picture" } else { "standby picture" }));
            self.hub.set_output(what);
        }
        if mode == Mode::Live && (c.sent % 1800 == 0) {
            log(format!("projector: {} frames sent on this connection", c.sent));
        }
    }

    /// Standby only on a projector that is on and already on its LAN/WiFi input: someone
    /// presenting over HDMI must not be switched away.
    fn standby_allowed(&self) -> bool {
        if !self.s.select_input {
            return true;
        }
        match http::acer_post(&self.s.host, "/form/home_cgi", "QueryControl") {
            Ok(state) => {
                let on = !matches!(state.get("sysstatus").map(String::as_str), Some("0") | Some("3") | None);
                on && state.get("displaysrc").map(String::as_str) == Some("33")
            }
            Err(_) => false,
        }
    }

    fn connect(&mut self, switch_input: bool) -> io::Result<Connection> {
        let host = self.s.host.clone();
        let stream_host = self.s.stream_host.clone();
        self.shown = false;
        if switch_input {
            match http::acer_post(&host, "/form/home_cgi", "QueryControl") {
                Ok(state) if state.get("displaysrc").map(String::as_str) == Some("33") => {}
                Ok(_) => {
                    log("projector: switching to the LAN/WiFi input");
                    let _ = http::acer_post(&host, "/form/control_cgi", "src=33");
                    thread::sleep(Duration::from_secs(2));
                }
                Err(e) => log(format!("projector: web interface not reachable: {e}")),
            }
        }

        // 1. control channel
        let mut control = match Control::open(&stream_host, &self.s.name) {
            Ok(c) => Some(c),
            Err(e) => {
                log(format!("projector: control channel failed: {e}"));
                None
            }
        };
        if let Some(c) = control.as_mut() {
            if !c.request_stream()? {
                return Err(io::Error::other("projector did not allow streaming"));
            }
        }

        // 2. UDP greeting and mirror request
        self.udp_handshake();
        if let Some(c) = control.as_mut() {
            c.enable_control();
        }

        // 3. picture connection
        let address: SocketAddr = format!("{stream_host}:{PORT}")
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "projector_stream_host must be an IP address"))?;
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(4))?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut first = hello(&self.s.name);
        first.extend(request_stream());
        (&stream).write_all(&first)?;
        let alive = Arc::new(AtomicBool::new(true));
        let state = Arc::new(AtomicU32::new(STATE_WAITING));
        let writer = Arc::new(Mutex::new(stream.try_clone()?));
        {
            let (alive, state, hub) = (alive.clone(), state.clone(), self.hub.clone());
            thread::spawn(move || read_loop(stream, alive, state, hub));
        }
        {
            let (alive, writer) = (alive.clone(), writer.clone());
            thread::spawn(move || {
                let beat = heartbeat();
                while alive.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(1));
                    if writer.lock().unwrap().write_all(&beat).is_err() {
                        alive.store(false, Ordering::Relaxed);
                    }
                }
            });
        }
        log(format!("projector: connected to {stream_host}:{PORT}, stream requested"));
        self.need_key = true;
        self.hub.request_idr();
        Ok(Connection { control, writer, alive, state, seq: 0, sent: 0 })
    }

    fn udp_handshake(&self) {
        let Ok(udp) = UdpSocket::bind("0.0.0.0:0") else { return };
        let _ = udp.set_read_timeout(Some(Duration::from_millis(300)));
        let target = (self.s.stream_host.as_str(), PORT);
        let ask = |datagram: &[u8], wanted: &[u8]| -> Option<Vec<u8>> {
            for _ in 0..2 {
                udp.send_to(datagram, target).ok()?;
                let deadline = Instant::now() + Duration::from_millis(1500);
                let mut buf = [0u8; 2048];
                while Instant::now() < deadline {
                    if let Ok((n, _)) = udp.recv_from(&mut buf) {
                        if buf[..n].windows(wanted.len()).any(|w| w == wanted) {
                            return Some(buf[..n].to_vec());
                        }
                    }
                }
            }
            None
        };
        let mut greeting = udp_text(1, &self.s.name);
        greeting.extend_from_slice(USER.as_bytes());
        greeting.push(0);
        match ask(&greeting, b":3:") {
            Some(a) => log(format!("projector: found {}", String::from_utf8_lossy(&a).trim_end_matches('\0'))),
            None => log("projector: no answer to the UDP greeting"),
        }
        let mut mirror = udp_text(34, &self.s.name);
        mirror.push(0);
        mirror.extend(mirror_request());
        match ask(&mirror, b":34\0").map(|a| parse_size(&a)) {
            Some(Some((w, h))) => {
                log(format!("projector: mirror request answered, picture size {w}x{h}"));
                if (w, h) != (self.s.width, self.s.height) {
                    log(format!("projector: it asks for {w}x{h} but the video is {}x{}", self.s.width, self.s.height));
                }
            }
            Some(None) => log("projector: mirror request answered without a size"),
            None => log("projector: no answer to the mirror request"),
        }
    }
}

fn read_loop(mut stream: TcpStream, alive: Arc<AtomicBool>, state: Arc<AtomicU32>, hub: Arc<Hub>) {
    let mut head = [0u8; 8];
    while alive.load(Ordering::Relaxed) {
        if let Err(e) = stream.read_exact(&mut head) {
            if alive.swap(false, Ordering::Relaxed) {
                log(format!("projector: connection closed ({e})"));
            }
            return;
        }
        let length = u32::from_le_bytes(head[4..8].try_into().unwrap()) as usize;
        if length > 1 << 20 {
            log("projector: unexpected data, closing");
            alive.store(false, Ordering::Relaxed);
            return;
        }
        let mut body = vec![0u8; length];
        if stream.read_exact(&mut body).is_err() {
            alive.store(false, Ordering::Relaxed);
            return;
        }
        let w: Vec<u32> = body.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        let shown: Vec<String> = w.iter().take(6).map(|v| format!("{v:x}")).collect();
        log(format!("projector: message {}", shown.join(" ")));
        match w.first() {
            Some(&CMD_REQUEST_STREAM) if w.len() >= 6 => {
                let ok = w[5] == 1;
                state.store(if ok { STATE_ALLOWED } else { STATE_DENIED }, Ordering::Relaxed);
                log(format!("projector: stream request {}", if ok { "accepted" } else { "DENIED" }));
                if ok {
                    hub.request_idr();
                }
            }
            Some(&CMD_NOTIFICATION) if w.len() >= 3 && (w[2] == 2 || w[2] == 4) => {
                log("projector: asks to stop the stream");
                state.store(STATE_DENIED, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn messages_match_the_capture() {
        assert_eq!(hello("YOGA"), hex("000000001c000000090000000010000001000000000000000000000004000000594f4741"));
        assert_eq!(request_stream(), hex("0000000018000000060000000010000001000000000000000000000007000000"));
        assert_eq!(heartbeat(), hex("0000000018000000040000000000000000000000000000000000000000000000"));
        // 64 bytes after "...:34:\0" in the captured datagram (91 bytes in total)
        let captured = hex(concat!(
            "01000000000000003000000000000000",
            "010000000000000000000000000000000000000000000000",
            "010000000000000000000000000000000000000000000000"
        ));
        assert_eq!(mirror_request(), captured);
        assert_eq!(udp_text(34, "YOGA").len() + 1 + captured.len(), "2:1794687910:User:YOGA:34:".len() + 1 + 64);
        let unit = [0u8, 0, 0, 1, 0x65];
        assert_eq!(&video(3, &unit, 1280, 720)[..32], &words(&[3, 29, 2, 0x1000, 7, 1280, 720, 5])[..]);
        assert_eq!(
            parse_size(&hex("313a39313738383a726f6f743a4163657250726f6a6563746f725f33453032303830303a333400010000000110000000050000d00200000000000000000000")),
            Some((1280, 720))
        );
    }
}
