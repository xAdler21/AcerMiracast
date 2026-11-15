//! EZCast "remote" control channel (TCP 63630), as the Acer software uses it before it may
//! stream. Taken from a capture and the decompiled Acer eDisplay Pro 1.12.
//!
//! Framing: u32 little-endian length, then text.
//!   client -> projector   "6:" + JSON          plain JSON-RPC (only the first request)
//!                         "7:" + base64(AES)   encrypted JSON-RPC
//!                         "1:100"              sent once after the role is assigned
//!                         {"jsonrpc":2.0, "method": "heartbeat"}   plain, once a second
//!   projector -> client   "JSONRPC:2:" + JSON, "EZREMOTE:2:...", "CUSTOMER:4:..."
//!
//! Encryption: AES-128-CBC, IV zero, PKCS#7. The session key is the "key" from the answer to
//! common.set_device_description, base64-decoded and decrypted with the fixed key below.
//! Every JSON text ends with "\n" (jsoncpp FastWriter, keys sorted).

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use aes::Aes128;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use serde_json::Value;

use crate::hub::{Guest, Hub, log};

pub const PORT: u16 = 63630;
const FIXED_KEY: &[u8; 16] = b"AM2feY5ysJAA4oAM";
const HEARTBEAT: &[u8] = br#"{"jsonrpc":2.0, "method": "heartbeat"}"#;

pub fn encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    cbc::Encryptor::<Aes128>::new(key.into(), &[0u8; 16].into()).encrypt_padded_vec_mut::<Pkcs7>(data)
}

pub fn decrypt(key: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    cbc::Decryptor::<Aes128>::new(key.into(), &[0u8; 16].into()).decrypt_padded_vec_mut::<Pkcs7>(data).ok()
}

/// Session key from the "key" field of the projector's answer.
pub fn session_key(field: &str) -> Option<[u8; 16]> {
    let raw = B64.decode(field.trim()).ok()?;
    decrypt(FIXED_KEY, &raw)?.try_into().ok()
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

pub fn encrypted_frame(key: &[u8; 16], json: &str) -> Vec<u8> {
    let mut text = b"7:".to_vec();
    text.extend_from_slice(B64.encode(encrypt(key, json.as_bytes())).as_bytes());
    frame(&text)
}

/// JSON-RPC request text the way jsoncpp's FastWriter writes it (sorted keys, trailing \n).
fn request_text(id: u32, method: &str, params: Option<&str>) -> String {
    match params {
        Some(p) => format!("{{\"id\":{id},\"jsonrpc\":\"2.0\",\"method\":\"{method}\",\"params\":{p}}}\n"),
        None => format!("{{\"id\":{id},\"jsonrpc\":\"2.0\",\"method\":\"{method}\"}}\n"),
    }
}

fn read_frame(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > 1 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "control frame too long"));
    }
    let mut data = vec![0u8; len];
    stream.read_exact(&mut data)?;
    Ok(data)
}

enum Event {
    Result(u64, Value),
    RoleAssigned,
}

struct Shared {
    writer: Mutex<TcpStream>,
    key: OnceLock<[u8; 16]>,
    alive: AtomicBool,
    pending: Mutex<VecDeque<u64>>, // requests from the projector that wait for the key
}

impl Shared {
    fn send(&self, data: &[u8]) -> io::Result<()> {
        self.writer.lock().unwrap().write_all(data)
    }

    fn answer(&self, id: u64) {
        if let Some(key) = self.key.get() {
            let _ = self.send(&encrypted_frame(key, &format!("{{\"id\":{id},\"jsonrpc\":\"2.0\",\"result\":0}}\n")));
        } else {
            self.pending.lock().unwrap().push_back(id);
        }
    }
}

pub struct Control {
    shared: Arc<Shared>,
    events: Receiver<Event>,
    next_id: u32,
}

impl Control {
    /// Connects, introduces itself as `name` and waits until the projector assigned a role.
    pub fn open(host: &str, name: &str, hub: Arc<Hub>) -> io::Result<Control> {
        let address: SocketAddr = format!("{host}:{PORT}")
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "projector_host must be an IP address"))?;
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(4))?;
        stream.set_nodelay(true)?;
        let reader = stream.try_clone()?;
        let shared = Arc::new(Shared {
            writer: Mutex::new(stream),
            key: OnceLock::new(),
            alive: AtomicBool::new(true),
            pending: Mutex::new(VecDeque::new()),
        });
        let (tx, rx) = mpsc::channel();
        {
            let shared = shared.clone();
            thread::spawn(move || read_loop(reader, shared, tx, hub));
        }
        let name = name.replace(['"', '\\'], "");
        let first = format!(
            "{{\"id\":1,\"jsonrpc\":\"2.0\",\"method\":\"common.set_device_description\",\"params\":{{\"capability\":{{\"host_control\":{{\"version\":2}}}},\"hostname\":\"{name}\"}}}}\n"
        );
        let mut text = b"6:".to_vec();
        text.extend_from_slice(first.as_bytes());
        shared.send(&frame(&text))?;
        let mut control = Control { shared, events: rx, next_id: 2 };

        let deadline = Instant::now() + Duration::from_secs(6);
        let mut role = false;
        while control.shared.key.get().is_none() || !role {
            match control.next_event(deadline)? {
                Event::RoleAssigned => role = true,
                Event::Result(1, result) => {
                    let key = result
                        .get("key")
                        .and_then(Value::as_str)
                        .and_then(session_key)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no usable key in the answer"))?;
                    let _ = control.shared.key.set(key);
                    let waiting: Vec<u64> = control.shared.pending.lock().unwrap().drain(..).collect();
                    for id in waiting {
                        control.shared.answer(id);
                    }
                }
                Event::Result(..) => {}
            }
        }
        control.shared.send(&frame(b"1:100"))?;
        {
            let shared = control.shared.clone();
            thread::spawn(move || {
                while shared.alive.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_secs(1));
                    if shared.send(&frame(HEARTBEAT)).is_err() {
                        shared.alive.store(false, Ordering::Relaxed);
                    }
                }
            });
        }
        let id = control.call("common.get_service", Some(r#"{"country":"DE","os":"windows","role":"host"}"#))?;
        control.wait_result(id, Duration::from_secs(4))?;
        Ok(control)
    }

    fn next_event(&self, deadline: Instant) -> io::Result<Event> {
        let left = deadline.saturating_duration_since(Instant::now());
        match self.events.recv_timeout(left) {
            Ok(e) => Ok(e),
            Err(RecvTimeoutError::Timeout) => Err(io::Error::new(io::ErrorKind::TimedOut, "projector did not answer")),
            Err(RecvTimeoutError::Disconnected) => {
                Err(io::Error::new(io::ErrorKind::ConnectionAborted, "projector closed the control connection"))
            }
        }
    }

    fn call(&mut self, method: &str, params: Option<&str>) -> io::Result<u32> {
        let id = self.next_id;
        self.next_id += 1;
        let key = *self.shared.key.get().expect("key is set in open()");
        self.shared.send(&encrypted_frame(&key, &request_text(id, method, params)))?;
        Ok(id)
    }

    fn wait_result(&self, id: u32, timeout: Duration) -> io::Result<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Event::Result(got, value) = self.next_event(deadline)? {
                if got == id as u64 {
                    return Ok(value);
                }
            }
        }
    }

    /// Asks for the screen. Ok(true) = "allow".
    pub fn request_stream(&mut self) -> io::Result<bool> {
        let id = self.call("common.disable_control", None)?;
        let _ = self.wait_result(id, Duration::from_secs(3));
        let id = self.call("common.request_stream", None)?;
        let answer = self.wait_result(id, Duration::from_secs(20))?;
        let verdict = answer.get("result").and_then(Value::as_str).unwrap_or("").to_string();
        log(format!("projector: control channel answers the stream request with {answer}"));
        Ok(verdict == "allow")
    }

    /// After "allow" the Acer software announces its input devices; sent the same way.
    pub fn enable_control(&mut self) {
        let _ = self.call(
            "common.enable_control",
            Some(r#"{"devlist":{"keyboard":false,"mouse":true,"point":false,"touch":true},"version":1}"#),
        );
    }

    /// Remote-control key, as the Acer software's key pad sends it ("1:<code>"):
    /// 10 up, 11 down, 12 left, 13 right, 14 OK, 15 back.
    pub fn send_key(&self, code: u32) -> io::Result<()> {
        self.shared.send(&frame(format!("1:{code}").as_bytes()))
    }

    /// Answer to another presenter's request for the screen (we are the host).
    pub fn answer_guest(&mut self, ip: &str, allow: bool) -> io::Result<()> {
        let params = serde_json::json!({"ip_address": ip, "result": if allow { "allow" } else { "deny" }});
        self.call("common.answer_request_stream", Some(&params.to_string()))?;
        Ok(())
    }

    /// Ends every presentation on the projector (host only).
    pub fn disconnect_all(&mut self) -> io::Result<()> {
        self.call("common.disconnect_all", None)?;
        Ok(())
    }

    pub fn alive(&self) -> bool {
        self.shared.alive.load(Ordering::Relaxed)
    }

    /// Drains notifications; returns false once the connection is gone.
    pub fn poll(&self) -> bool {
        while self.events.try_recv().is_ok() {}
        self.alive()
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.shared.alive.store(false, Ordering::Relaxed);
        let _ = self.shared.writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}

fn read_loop(mut stream: TcpStream, shared: Arc<Shared>, events: Sender<Event>, hub: Arc<Hub>) {
    while shared.alive.load(Ordering::Relaxed) {
        let data = match read_frame(&mut stream) {
            Ok(d) => d,
            Err(e) => {
                if shared.alive.swap(false, Ordering::Relaxed) {
                    log(format!("projector: control connection lost: {e}"));
                }
                break;
            }
        };
        let text = String::from_utf8_lossy(&data).into_owned();
        let json = if let Some(b64) = text.strip_prefix("7:") {
            // encrypted (not seen from the projector so far, handled anyway)
            let Some(key) = shared.key.get() else { continue };
            match B64.decode(b64.trim()).ok().and_then(|raw| decrypt(key, &raw)) {
                Some(plain) => String::from_utf8_lossy(&plain).into_owned(),
                None => continue,
            }
        } else if text.starts_with("JSONRPC:") {
            match text.find('{') {
                Some(i) => text[i..].to_string(),
                None => continue,
            }
        } else {
            if !text.starts_with("EZREMOTE:") {
                log(format!("projector: control message {}", text.chars().take(120).collect::<String>()));
            }
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&json) else { continue };
        let id = value.get("id").and_then(Value::as_u64);
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            if let Some(id) = id {
                shared.answer(id);
            }
            let param = |k: &str| value["params"][k].as_str().unwrap_or("").to_string();
            if method == "common.assign_role" {
                let _ = events.send(Event::RoleAssigned);
            } else if method.starts_with("common.ask_request_stream") {
                let guest = Guest { name: param("hostname"), ip: param("ip_address"),
                                    fullscreen: method.ends_with("_fullscreen") };
                log(format!("projector: {} ({}) asks for the screen", guest.name, guest.ip));
                hub.guest_asks(guest);
            } else if method == "common.cancel_request_stream" {
                hub.guest_gone(&param("ip_address"));
            } else {
                log(format!("projector: control request {method}"));
            }
        } else if let (Some(id), Some(result)) = (id, value.get("result")) {
            let _ = events.send(Event::Result(id, result.clone()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // values from the capture of the Acer software (14.11.2026)
    #[test]
    fn key_and_messages_match_the_capture() {
        let key = session_key("LWH9MTzXhGlecLgnP3X2EaQZI40UEDM4eM8uP9XiGCs=").unwrap();
        assert_eq!(&key, b"22E67o()TeiM\"N\\7");
        let frame = encrypted_frame(&key, "{\"id\":0,\"jsonrpc\":\"2.0\",\"result\":0}\n");
        assert_eq!(&frame[4..], b"7:kFFvKdA7LhqhMXPafR++whiBywPbBuyyq4z1bVZ5uwNc+kndhrTrwrI42HWOojBe");
        assert_eq!(u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize, frame.len() - 4);
        let frame = encrypted_frame(&key, &request_text(5, "common.request_stream", None));
        assert_eq!(&frame[4..], b"7:F3+74whCdr85SNEHnQ9yY0bF/GOO+PT9v2mb3kwRX2vVLKYrUKqQPQFWebsn9XaXqgv9hmUl2nu1SBpgWFgJRQ==");
        let frame = encrypted_frame(
            &key,
            &request_text(2, "common.get_service", Some(r#"{"country":"DE","os":"windows","role":"host"}"#)),
        );
        assert_eq!(
            &frame[4..],
            b"7:F2VmATpr/FF66jrdq8UnvUX8uhgKWS62I40k3V8aosz1RY4KdWNqiNpEBC6+XFQxLkRQBegfM5cLQ0Qolz0x97aHd2C/qWbUh/TUDi5YygnO2UIKigk80MJp8uiLYjV/wxlLLqhmupBhdxd/2L6zNA=="
        );
    }

    #[test]
    fn heartbeat_frame_matches_the_capture() {
        let f = frame(HEARTBEAT);
        assert_eq!(f[..4], [0x26, 0, 0, 0]);
    }
}
