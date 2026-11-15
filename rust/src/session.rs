//! One Win+K session: RTSP (Wi-Fi Display sink side, M1..M7, M16 keep-alive, M13 IDR
//! request) to the source, RTP/MPEG-TS from the source into the media pipeline.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::hub::{Hub, log};
use crate::media::{Pipeline, VideoSize};
use crate::ezdisplay::Output;

pub struct Options {
    pub rtp_port: u16,
    pub idle_timeout: Duration,
    /// ask the source for exactly what the projector takes and forward it unchanged
    pub passthrough: bool,
    pub video: VideoSize,
    pub preview: Option<(u32, u32)>,
}

/// CEA resolution bit for 1280x720p30 and its index (Wi-Fi Display spec, table 5-10).
const CEA_720P30: u32 = 0x20;

fn video_formats(o: &Options) -> String {
    if o.passthrough {
        // native 720p30 (CEA index 5), CBP level 3.1, only 1280x720p30: H.264 the projector can
        // play as it is
        "28 00 01 01 00000020 00000000 00000000 00 0000 0000 00 none none".into()
    } else {
        // CHP|CBP level 4.2, all CEA / VESA / handheld modes
        "00 00 03 10 0001ffff 1fffffff 00001fff 00 0000 0000 10 none none".into()
    }
}

/// True if the source's choice (M4) is plain CBP 1280x720p30.
fn is_projector_format(chosen: &str) -> bool {
    let f: Vec<&str> = chosen.split_whitespace().collect();
    let hex = |i: usize| f.get(i).and_then(|v| u32::from_str_radix(v, 16).ok());
    f.len() >= 7 && hex(2) == Some(1) && hex(4) == Some(CEA_720P30) && hex(5) == Some(0) && hex(6) == Some(0)
}

struct Message {
    first: String,
    headers: HashMap<String, String>,
    body: String,
}

impl Message {
    fn is_response(&self) -> bool {
        self.first.starts_with("RTSP/")
    }
    fn status(&self) -> u32 {
        self.first.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
    }
    fn method(&self) -> &str {
        self.first.split(' ').next().unwrap_or("")
    }
    fn params(&self) -> Vec<(String, String)> {
        self.body
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| match l.split_once(':') {
                Some((k, v)) => (k.trim().to_string(), v.trim().to_string()),
                None => (l.to_string(), String::new()),
            })
            .collect()
    }
}

struct Rtsp {
    stream: TcpStream,
    buf: Vec<u8>,
    cseq: u32,
    pending: HashMap<u32, String>,
}

impl Rtsp {
    /// Next message; Ok(None) on timeout. Partial data stays buffered.
    fn read(&mut self) -> io::Result<Option<Message>> {
        loop {
            if let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buf[..end]).into_owned();
                let mut lines = head.split("\r\n");
                let first = lines.next().unwrap_or("").to_string();
                let headers: HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                if self.buf.len() >= end + 4 + length {
                    let body = String::from_utf8_lossy(&self.buf[end + 4..end + 4 + length]).into_owned();
                    self.buf.drain(..end + 4 + length);
                    return Ok(Some(Message { first, headers, body }));
                }
            }
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "source closed the connection")),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    fn send(&mut self, first: &str, headers: &[(&str, String)], body: &str) -> io::Result<()> {
        let mut text = format!("{first}\r\n");
        for (k, v) in headers {
            text += &format!("{k}: {v}\r\n");
        }
        if !body.is_empty() {
            text += &format!("Content-Type: text/parameters\r\nContent-Length: {}\r\n", body.len());
        }
        text += "\r\n";
        text += body;
        self.stream.write_all(text.as_bytes())
    }

    fn reply(&mut self, cseq: &str, extra: &[(&str, String)], body: &str) -> io::Result<()> {
        let mut headers = vec![("CSeq", cseq.to_string())];
        headers.extend_from_slice(extra);
        self.send("RTSP/1.0 200 OK", &headers, body)
    }

    fn request(&mut self, method: &str, url: &str, extra: &[(&str, String)], body: &str) -> io::Result<()> {
        self.cseq += 1;
        self.pending.insert(self.cseq, method.to_string());
        let mut headers = vec![("CSeq", self.cseq.to_string())];
        headers.extend_from_slice(extra);
        self.send(&format!("{method} {url} RTSP/1.0"), &headers, body)
    }
}

pub struct Session {
    pub source_ip: IpAddr,
    pub rtsp_port: u16,
    pub name: String,
    pub stop: Arc<AtomicBool>,
    /// Wi-Fi Direct through the box: the RTP of the source arrives forwarded by the box, which
    /// the router only lets through as an answer. Keep that path open by sending to the box.
    pub punch: Option<SocketAddr>,
}

impl Session {
    pub fn run(&self, o: &Options, hub: Arc<Hub>, output: Option<Output>) {
        let pipeline: Arc<Mutex<Option<Pipeline>>> = Arc::new(Mutex::new(None));
        let rtp_stop = Arc::new(AtomicBool::new(false));
        let rtp = match UdpSocket::bind(("0.0.0.0", o.rtp_port)) {
            Ok(s) => s,
            Err(e) => {
                log(format!("RTP port {} not available: {e}", o.rtp_port));
                return;
            }
        };
        if let (Some(target), Ok(sock)) = (self.punch, rtp.try_clone()) {
            let stop = rtp_stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = sock.send_to(b"keep-open", target);
                    thread::sleep(Duration::from_secs(5));
                }
            });
        }
        let rtp_thread = {
            let (pipeline, stop) = (pipeline.clone(), rtp_stop.clone());
            thread::spawn(move || receive_rtp(rtp, pipeline, stop))
        };
        let result = self.rtsp(o, &hub, output, &pipeline);
        if let Err(e) = result {
            log(format!("RTSP: {e}"));
        }
        rtp_stop.store(true, Ordering::Relaxed);
        let (packets, lost) = rtp_thread.join().unwrap_or((0, 0));
        let frames = pipeline.lock().unwrap().take().map(|p| p.frames.load(Ordering::Relaxed)).unwrap_or(0);
        log(format!("session with '{}' ended: {packets} RTP packets, {lost} lost, {frames} video frames", self.name));
    }

    fn rtsp(&self, o: &Options, hub: &Arc<Hub>, output: Option<Output>, pipeline: &Arc<Mutex<Option<Pipeline>>>) -> io::Result<()> {
        let stream = TcpStream::connect_timeout(&SocketAddr::new(self.source_ip, self.rtsp_port), Duration::from_secs(10))?;
        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
        stream.set_nodelay(true)?;
        log(format!("RTSP connected to {}:{}", self.source_ip, self.rtsp_port));
        let mut c = Rtsp { stream, buf: Vec::new(), cseq: 0, pending: HashMap::new() };
        let mut url = format!("rtsp://{}/wfd1.0/streamid=0", self.source_ip);
        let mut session_id: Option<String> = None;
        let mut last_seen = Instant::now();
        let mut output = output;
        // We are the RTSP client of the session: it times out (Session: ...;timeout=60) unless
        // we send a request now and then. Windows tears it down after ~70 s otherwise.
        let mut keepalive_every = Duration::from_secs(20);
        let mut keepalive_at = Instant::now() + keepalive_every;
        loop {
            if let Some(id) = &session_id {
                if Instant::now() >= keepalive_at {
                    keepalive_at = Instant::now() + keepalive_every;
                    c.request("GET_PARAMETER", &url, &[("Session", id.clone())], "")?;
                }
            }
            if self.stop.load(Ordering::Relaxed) {
                if let Some(id) = &session_id {
                    let _ = c.request("TEARDOWN", &url, &[("Session", id.clone())], "");
                }
                return Ok(());
            }
            if session_id.is_some() && hub.idr_wanted.swap(false, Ordering::Relaxed) {
                c.request("SET_PARAMETER", &url, &[("Session", session_id.clone().unwrap())], "wfd_idr_request\r\n")?;
            }
            let Some(msg) = c.read()? else {
                if last_seen.elapsed() > o.idle_timeout {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "nothing from the source, giving up"));
                }
                continue;
            };
            last_seen = Instant::now();
            let cseq = msg.headers.get("cseq").cloned().unwrap_or_else(|| "0".into());

            if msg.is_response() {
                let method = c.pending.remove(&cseq.parse().unwrap_or(0)).unwrap_or_default();
                if msg.status() != 200 {
                    if method == "SET_PARAMETER" || method == "GET_PARAMETER" {
                        log(format!("RTSP: {method} was answered with {} (not fatal)", msg.first));
                        continue; // IDR request / keep-alive refused: not fatal
                    }
                    return Err(io::Error::other(format!("{method} was answered with {}", msg.first)));
                }
                match method.as_str() {
                    "SETUP" => {
                        let header = msg.headers.get("session").cloned().unwrap_or_default();
                        let id = header.split(';').next().unwrap_or("").trim().to_string();
                        let timeout = header
                            .split(';')
                            .find_map(|p| p.trim().strip_prefix("timeout=").and_then(|t| t.trim().parse::<u64>().ok()))
                            .unwrap_or(60);
                        keepalive_every = Duration::from_secs((timeout / 3).clamp(5, 30));
                        keepalive_at = Instant::now() + keepalive_every;
                        log(format!("RTSP: session {id}, timeout {timeout} s, keep-alive every {} s", keepalive_every.as_secs()));
                        c.request("PLAY", &url, &[("Session", id.clone())], "")?;
                        session_id = Some(id);
                    }
                    "PLAY" => {
                        hub.update_session(|s| s.playing = true);
                        log("RTSP: PLAY accepted, stream is running");
                    }
                    "TEARDOWN" => {
                        log("RTSP: TEARDOWN done");
                        return Ok(());
                    }
                    _ => {}
                }
                continue;
            }

            match msg.method() {
                "OPTIONS" => {
                    c.reply(&cseq, &[("Public", "org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER".into())], "")?;
                    c.request("OPTIONS", "*", &[("Require", "org.wfa.wfd1.0".into())], "")?;
                }
                "GET_PARAMETER" => {
                    let wanted = msg.params();
                    if wanted.is_empty() {
                        c.reply(&cseq, &[], "")?; // M16 keep-alive
                        continue;
                    }
                    let mut body = String::new();
                    for (name, _) in wanted {
                        let value = match name.as_str() {
                            "wfd_video_formats" => video_formats(o),
                            "wfd_audio_codecs" => "LPCM 00000002 00".into(),
                            "wfd_client_rtp_ports" => format!("RTP/AVP/UDP;unicast {} 0 mode=play", o.rtp_port),
                            "wfd_connector_type" => "05".into(),
                            "wfd_idr_request_capability" => "1".into(),
                            _ => "none".into(),
                        };
                        body += &format!("{name}: {value}\r\n");
                    }
                    c.reply(&cseq, &[], &body)?;
                }
                "SET_PARAMETER" => {
                    let params: HashMap<String, String> = msg.params().into_iter().collect();
                    c.reply(&cseq, &[], "")?;
                    if let Some(p) = params.get("wfd_presentation_URL") {
                        if let Some(first) = p.split(' ').next().filter(|u| u.starts_with("rtsp://")) {
                            url = first.to_string();
                        }
                    }
                    if let Some(chosen) = params.get("wfd_video_formats") {
                        let direct = o.passthrough && is_projector_format(chosen);
                        let mut slot = pipeline.lock().unwrap();
                        if slot.is_some() {
                            log(format!("RTSP: source renegotiated video '{chosen}'"));
                        } else {
                            log(format!(
                                "RTSP: source chose video '{chosen}' -> {}",
                                if output.is_none() { "panel only" } else if direct { "forwarded unchanged" } else { "re-encoded by ffmpeg" }
                            ));
                            hub.update_session(|s| s.video = if direct { "direkt".into() } else { "umgerechnet".into() });
                            *slot = Some(Pipeline::new(hub.clone(), output.take(), direct, &o.video, o.preview));
                        }
                    }
                    match params.get("wfd_trigger_method").map(String::as_str) {
                        Some("SETUP") => {
                            let mut slot = pipeline.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(Pipeline::new(hub.clone(), output.take(), false, &o.video, o.preview));
                            }
                            drop(slot);
                            c.request("SETUP", &url, &[("Transport", format!("RTP/AVP/UDP;unicast;client_port={}", o.rtp_port))], "")?;
                        }
                        Some("TEARDOWN") => {
                            log("RTSP: the source asks to end the session (trigger TEARDOWN)");
                            if let Some(id) = &session_id {
                                c.request("TEARDOWN", &url, &[("Session", id.clone())], "")?;
                            } else {
                                return Ok(());
                            }
                        }
                        Some(t @ ("PAUSE" | "PLAY")) => {
                            if let Some(id) = &session_id {
                                c.request(t, &url, &[("Session", id.clone())], "")?;
                            }
                        }
                        _ => {}
                    }
                }
                _ => c.send("RTSP/1.0 405 Method Not Allowed", &[("CSeq", cseq)], "")?,
            }
        }
    }
}

/// Receives RTP (payload type 33, MPEG-TS) and passes the TS bytes on in sequence order,
/// holding back at most 16 packets while waiting for a missing one.
fn receive_rtp(sock: UdpSocket, pipeline: Arc<Mutex<Option<Pipeline>>>, stop: Arc<AtomicBool>) -> (u64, u64) {
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    let mut held: HashMap<u16, Vec<u8>> = HashMap::new();
    let mut expected: Option<u16> = None;
    let (mut packets, mut lost) = (0u64, 0u64);
    let mut buf = vec![0u8; 65536];
    while !stop.load(Ordering::Relaxed) {
        let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
        let d = &buf[..n];
        if n < 12 || d[0] >> 6 != 2 {
            continue;
        }
        let mut offset = 12 + 4 * (d[0] & 0x0f) as usize;
        if d[0] & 0x10 != 0 {
            if n < offset + 4 {
                continue;
            }
            offset += 4 + 4 * u16::from_be_bytes([d[offset + 2], d[offset + 3]]) as usize;
        }
        let mut end = n;
        if d[0] & 0x20 != 0 && end > offset {
            end = end.saturating_sub(d[n - 1] as usize);
        }
        if end <= offset {
            continue;
        }
        packets += 1;
        let seq = u16::from_be_bytes([d[2], d[3]]);
        let mut exp = *expected.get_or_insert(seq);
        let ahead = seq.wrapping_sub(exp);
        if ahead >= 0x8000 {
            continue; // older than what was already passed on
        }
        if ahead > 512 {
            // the source restarted its numbering (e.g. after renegotiating): follow it
            held.clear();
            exp = seq;
        }
        held.insert(seq, d[offset..end].to_vec());
        let mut slot = pipeline.lock().unwrap();
        loop {
            if let Some(payload) = held.remove(&exp) {
                if let Some(p) = slot.as_mut() {
                    p.feed(&payload);
                }
                exp = exp.wrapping_add(1);
            } else if held.len() > 16 {
                lost += 1;
                exp = exp.wrapping_add(1);
            } else {
                break;
            }
        }
        expected = Some(exp);
    }
    (packets, lost)
}

#[cfg(test)]
mod tests {
    #[test]
    fn projector_format() {
        assert!(super::is_projector_format("28 00 01 01 00000020 00000000 00000000 00 0000 0000 00 none none"));
        assert!(!super::is_projector_format("00 00 01 10 00000100 00000000 00000000 00 0000 0000 00 none none"));
    }
}
