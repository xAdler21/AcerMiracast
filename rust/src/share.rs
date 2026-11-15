//! Screen sharing from the panel: the browser captures the screen (getDisplayMedia), encodes
//! H.264 Constrained Baseline 1280x720 itself (WebCodecs) and sends it over a WebSocket on the
//! panel's port (GET /api/share). The access units go to the projector unchanged, like the
//! Win+K passthrough; sound goes to the shared audio sink.
//!
//! Messages from the browser:
//!   text   {"type":"hello","name":"Partyraum-PC","takeover":false}   first message
//!   binary 0x01 flags(1: key frame) pts_us(u64 BE) | H.264 access unit (Annex B)
//!   binary 0x02 channels(u8) pts_us(u64 BE) rate(u32 BE) | PCM s16 little endian, interleaved
//! Messages to the browser (text):
//!   {"type":"started"}  {"type":"busy","by":"..."}  {"type":"key"}  {"type":"ended","reason":"..."}

use std::io::{self, BufReader, Write};
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::ezdisplay::Output;
use crate::hub::{Hub, SessionInfo, log};
use crate::media::{Pipeline, VideoSize};
use crate::ws::{self, Message};

pub struct ShareOptions {
    pub video: VideoSize,
    pub preview: Option<(u32, u32)>,
}

const MAX_MESSAGE: usize = 4 * 1024 * 1024;

pub fn serve(mut stream: TcpStream, mut reader: BufReader<TcpStream>, key: &str, peer: IpAddr,
             hub: Arc<Hub>, output: Option<Output>, o: &ShareOptions) -> io::Result<()> {
    ws::handshake(&mut stream, key)?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    reader.get_ref().set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut frames = ws::Reader::new(MAX_MESSAGE);

    // hello
    let deadline = Instant::now() + Duration::from_secs(10);
    let hello: Value = loop {
        match frames.read(&mut reader)? {
            Some(Message::Text(t)) => break serde_json::from_str(&t).unwrap_or(Value::Null),
            Some(Message::Close) => return Ok(()),
            _ if Instant::now() > deadline => return Ok(()),
            _ => {}
        }
    };
    if hello["type"] != "hello" {
        return ws::write(&mut stream, 0x8, &[]);
    }
    let name: String = hello["name"].as_str().unwrap_or("").chars()
        .filter(|c| !c.is_control()).take(40).collect::<String>().trim().to_string();
    let name = if name.is_empty() { "Browser".to_string() } else { name };

    // claim the receiver; somebody already projecting is ended first if the user agreed
    let stop = Arc::new(AtomicBool::new(false));
    let info = || SessionInfo {
        source: name.clone(), ip: format!("{peer} (Bildschirm teilen)"), playing: true,
        started: Instant::now(), frames: 0, video: "direkt aus dem Browser".into(),
    };
    if let Err(busy) = hub.begin_session(info(), stop.clone()) {
        if hello["takeover"] != true {
            return ws::text(&mut stream, &json!({"type": "busy", "by": busy}).to_string());
        }
        log(format!("share: '{name}' ({peer}) takes over from '{busy}'"));
        hub.stop_session();
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            thread::sleep(Duration::from_millis(100));
            match hub.begin_session(info(), stop.clone()) {
                Ok(()) => break,
                Err(still) if Instant::now() > until => {
                    return ws::text(&mut stream, &json!({"type": "busy", "by": still}).to_string());
                }
                Err(_) => {}
            }
        }
    }
    log(format!("share: '{name}' from {peer} started"));
    let result = run(&mut stream, &mut reader, &mut frames, &hub, output, o, &stop);
    let reason = match &result {
        Ok(r) => r.to_string(),
        Err(e) => format!("Verbindung verloren: {e}"),
    };
    let _ = ws::text(&mut stream, &json!({"type": "ended", "reason": reason}).to_string());
    let _ = ws::write(&mut stream, 0x8, &[]);
    hub.end_session();
    log(format!("share: '{name}' ended ({reason})"));
    Ok(())
}

/// Runs the shared session; Ok(reason it ended).
fn run(stream: &mut TcpStream, reader: &mut BufReader<TcpStream>, frames: &mut ws::Reader, hub: &Arc<Hub>,
       output: Option<Output>, o: &ShareOptions, stop: &AtomicBool) -> io::Result<&'static str> {
    ws::text(stream, &json!({"type": "started"}).to_string())?;
    let mut pipeline = Pipeline::new_h264(hub.clone(), output.clone(), &o.video, o.preview);
    let mut last_message = Instant::now();
    let mut ping_at = Instant::now() + Duration::from_secs(5);
    let mut alive_at = Instant::now();
    let mut got_video = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok("Beendet am Panel oder von jemand anderem übernommen");
        }
        if hub.idr_wanted.swap(false, Ordering::Relaxed) {
            ws::text(stream, &json!({"type": "key"}).to_string())?;
        }
        let now = Instant::now();
        if now >= ping_at {
            ping_at = now + Duration::from_secs(5);
            ws::write(stream, 0x9, b"p")?;
        }
        if got_video && now >= alive_at {
            alive_at = now + Duration::from_secs(1);
            if let Some(out) = &output {
                out.alive();
            }
        }
        if last_message.elapsed() > Duration::from_secs(20) {
            return Ok("Der Browser antwortet nicht mehr");
        }
        let Some(msg) = frames.read(reader)? else { continue };
        last_message = Instant::now();
        match msg {
            Message::Binary(mut b) if b.len() > 10 && b[0] == 1 => {
                got_video = true;
                pipeline.feed_h264(&mut b[10..]);
            }
            Message::Binary(b) if b.len() > 14 && b[0] == 2 => {
                let channels = b[1].max(1) as u16;
                let pts = u64::from_be_bytes(b[2..10].try_into().unwrap());
                let rate = u32::from_be_bytes(b[10..14].try_into().unwrap());
                let samples: Vec<i16> = b[14..].chunks_exact(2).map(|x| i16::from_le_bytes([x[0], x[1]])).collect();
                hub.audio.push("Bildschirm teilen", Some(pts), rate, channels, &samples);
            }
            Message::Ping(p) => ws::write(stream, 0xA, &p)?,
            Message::Close => return Ok("Teilen beendet"),
            _ => {}
        }
        stream.flush()?;
    }
}
