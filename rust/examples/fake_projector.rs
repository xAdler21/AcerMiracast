//! Test stand-in for the Acer P5, answering like it did in the capture of the Acer software:
//! control channel on 63630 (with the captured key), UDP and TCP on 2425.
//!
//!     cargo run --example fake_projector [seconds]
//!
//! Prints what it got and ends with "fake projector: OK" if every step happened in order.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aes::Aes128;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};

const CAPTURED_KEY: &str = "LWH9MTzXhGlecLgnP3X2EaQZI40UEDM4eM8uP9XiGCs=";
const SESSION_KEY: &[u8; 16] = b"22E67o()TeiM\"N\\7";

#[derive(Default)]
struct Seen {
    steps: Vec<String>,
}

fn note(seen: &Arc<Mutex<Seen>>, step: impl Into<String>) {
    let s = step.into();
    println!("  {s}");
    seen.lock().unwrap().steps.push(s);
}

fn frame(text: &str) -> Vec<u8> {
    let mut f = (text.len() as u32).to_le_bytes().to_vec();
    f.extend_from_slice(text.as_bytes());
    f
}

fn read_frame(s: &mut TcpStream) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).ok()?;
    let mut d = vec![0u8; u32::from_le_bytes(len) as usize];
    s.read_exact(&mut d).ok()?;
    Some(d)
}

fn control(mut s: TcpStream, seen: Arc<Mutex<Seen>>, beats: Arc<AtomicU32>) {
    while let Some(d) = read_frame(&mut s) {
        let text = String::from_utf8_lossy(&d).into_owned();
        if text.starts_with("{\"jsonrpc\":2.0, \"method\": \"heartbeat\"}") {
            beats.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if text == "1:100" {
            note(&seen, "control: 1:100");
            continue;
        }
        let json = if let Some(plain) = text.strip_prefix("6:") {
            plain.to_string()
        } else if let Some(b64) = text.strip_prefix("7:") {
            let raw = B64.decode(b64).expect("base64");
            let plain = cbc::Decryptor::<Aes128>::new(SESSION_KEY.into(), &[0u8; 16].into())
                .decrypt_padded_vec_mut::<Pkcs7>(&raw)
                .expect("decrypts with the session key");
            String::from_utf8(plain).unwrap()
        } else {
            note(&seen, format!("control: UNEXPECTED {text}"));
            continue;
        };
        assert!(json.ends_with('\n'), "JSON text ends with a newline");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let id = v["id"].as_u64().unwrap();
        match v.get("method").and_then(|m| m.as_str()) {
            Some("common.set_device_description") => {
                note(&seen, format!("control: set_device_description hostname={}", v["params"]["hostname"]));
                let answer = format!(
                    "JSONRPC:2:{{\n\t\"jsonrpc\":\t\"2.0\",\n\t\"result\":\t{{\n\t\t\"capability\":\t{{\n\t\t\t\"host_control\":\t{{\n\t\t\t\t\"version\":\t2\n\t\t\t}}\n\t\t}},\n\t\t\"key\":\t\"{CAPTURED_KEY}\"\n\t}},\n\t\"id\":\t1\n}}"
                );
                s.write_all(&frame(&answer)).unwrap();
                s.write_all(&frame("JSONRPC:2:{\n\t\"jsonrpc\":\t\"2.0\",\n\t\"method\":\t\"common.assign_role\",\n\t\"params\":\t{\n\t\t\"role\":\t\"host\"\n\t},\n\t\"id\":\t0\n}")).unwrap();
            }
            Some(m) => {
                note(&seen, format!("control: {m} (id {id})"));
                let result = if m == "common.request_stream" { "{\"result\":\"allow\"}".to_string() } else { "0".to_string() };
                s.write_all(&frame(&format!("JSONRPC:2:{{\"jsonrpc\":\"2.0\",\"result\":{result},\"id\":{id}}}"))).unwrap();
            }
            None => note(&seen, format!("control: answer to request {id}: {}", v["result"])),
        }
    }
}

fn words(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
}

fn picture(mut s: TcpStream, seen: Arc<Mutex<Seen>>, beats: Arc<AtomicU32>) {
    let mut frames = 0u32;
    let mut last_seq = None;
    loop {
        let mut head = [0u8; 8];
        if s.read_exact(&mut head).is_err() {
            break;
        }
        let w = words(&head);
        let mut body = vec![0u8; w[1] as usize];
        if s.read_exact(&mut body).is_err() {
            break;
        }
        let b = words(&body[..24]);
        match b[0] {
            9 => note(&seen, format!("tcp: hello '{}'", String::from_utf8_lossy(&body[24..]))),
            6 => {
                note(&seen, format!("tcp: request stream format {}", b[5]));
                thread::sleep(Duration::from_millis(300));
                let mut answer = Vec::new();
                for v in [1u32, 24, 6, 1, 1, 1, 1, 1] {
                    answer.extend_from_slice(&v.to_le_bytes());
                }
                s.write_all(&answer).unwrap();
            }
            4 => {
                beats.fetch_add(1, Ordering::Relaxed);
            }
            2 => {
                let unit = &body[24..];
                assert_eq!((b[1], b[2], b[3], b[4], b[5] as usize), (0x1000, 7, 1280, 720, unit.len()), "video header");
                assert_eq!(&unit[..4], &[0, 0, 0, 1], "4-byte start code");
                if frames == 0 {
                    assert_eq!(unit[4] & 0x1f, 7, "first frame starts with SPS");
                    note(&seen, "tcp: first video frame is a key frame with SPS");
                }
                if let Some(prev) = last_seq {
                    assert_eq!(w[0], prev + 1, "sequence numbers count up");
                }
                last_seq = Some(w[0]);
                frames += 1;
            }
            other => note(&seen, format!("tcp: command {other}")),
        }
    }
    note(&seen, format!("tcp: closed after {frames} video frames"));
}

fn main() {
    let seconds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let seen = Arc::new(Mutex::new(Seen::default()));
    let control_beats = Arc::new(AtomicU32::new(0));
    let picture_beats = Arc::new(AtomicU32::new(0));

    let ctl = TcpListener::bind("127.0.0.1:63630").unwrap();
    let pic = TcpListener::bind("127.0.0.1:2425").unwrap();
    let udp = UdpSocket::bind("127.0.0.1:2425").unwrap();
    {
        let (seen, beats) = (seen.clone(), control_beats.clone());
        thread::spawn(move || {
            for c in ctl.incoming().flatten() {
                note(&seen, "control: connected");
                let (seen, beats) = (seen.clone(), beats.clone());
                thread::spawn(move || control(c, seen, beats));
            }
        });
    }
    {
        let (seen, beats) = (seen.clone(), picture_beats.clone());
        thread::spawn(move || {
            for c in pic.incoming().flatten() {
                let (seen, beats) = (seen.clone(), beats.clone());
                thread::spawn(move || picture(c, seen, beats));
            }
        });
    }
    {
        let seen = seen.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while let Ok((n, peer)) = udp.recv_from(&mut buf) {
                let d = &buf[..n];
                if d.windows(3).any(|w| w == b":1:") {
                    note(&seen, "udp: greeting");
                    udp.send_to(b"2:91782:root:AcerProjector_3E020800:3:vendor=acer:model=ezcast:service=13bd", peer).unwrap();
                } else if d.windows(4).any(|w| w == b":34:") {
                    note(&seen, format!("udp: mirror request ({n} bytes)"));
                    let mut a = b"1:91788:root:AcerProjector_3E020800:34\0".to_vec();
                    for v in [1u32, 0x1001, 1280, 720, 0, 0] {
                        a.extend_from_slice(&v.to_le_bytes());
                    }
                    udp.send_to(&a, peer).unwrap();
                }
            }
        });
    }
    println!("fake projector listening on 127.0.0.1 (63630, 2425) for {seconds} s");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(seconds) {
        thread::sleep(Duration::from_millis(200));
        if seen.lock().unwrap().steps.iter().any(|s| s.starts_with("tcp: closed")) {
            thread::sleep(Duration::from_millis(500));
            break;
        }
    }
    let steps = seen.lock().unwrap().steps.clone();
    let pos = |p: &str| steps.iter().position(|s| s.starts_with(p));
    let order = [
        "control: set_device_description", "control: answer to request 0", "control: 1:100",
        "control: common.request_stream", "udp: mirror request", "tcp: hello", "tcp: request stream format 7",
        "tcp: first video frame",
    ];
    let mut ok = true;
    let mut last = None;
    for p in order {
        let at = pos(p);
        let good = at.is_some() && (last.is_none() || at > last);
        println!("{} {p}", if good { "ok  " } else { "FAIL" });
        ok &= good;
        last = at.or(last);
    }
    let cb = control_beats.load(Ordering::Relaxed);
    let pb = picture_beats.load(Ordering::Relaxed);
    println!("{} heartbeats: control {cb}, picture {pb}", if cb > 0 && pb > 0 { "ok  " } else { "FAIL" });
    ok &= cb > 0 && pb > 0;
    println!("fake projector: {}", if ok { "OK" } else { "FAILED" });
    std::process::exit(if ok { 0 } else { 1 });
}
