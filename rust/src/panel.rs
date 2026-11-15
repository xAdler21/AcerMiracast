//! Web panel: status of projector, beacon box and receiver; projector control.
//!
//!   GET  /                 the panel
//!   GET  /api/status       everything the panel shows, as JSON
//!   POST /api/projector    {"action": "power", "value": "on"|"off"} | {"action": "input", "value": "33"}
//!   GET  /stream           live picture of the running session (MJPEG)
//!   GET  /frame.jpg        newest frame

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::Config;
use crate::http::{self, acer_post};
use crate::hub::{Hub, log};

const PAGE: &str = include_str!("../panel.html");
const ACER_INPUTS: [(&str, &str); 8] = [
    ("3", "HDMI 1"), ("6", "HDMI 2"), ("33", "Drahtlos"), ("20", "VGA 1"),
    ("21", "VGA 2"), ("22", "Video"), ("34", "Medien"), ("35", "USB"),
];

fn acer_error(code: &str) -> Option<&'static str> {
    Some(match code {
        "1" => "Lüfter blockiert",
        "2" => "Übertemperatur",
        "3" => "Thermoschalter",
        "4" => "Lampe zündet nicht",
        "5" => "Lampe ging aus",
        "6" => "Farbrad",
        _ => return None,
    })
}

struct State {
    projector: Value,
    beacon: Value,
    waking_until: Option<Instant>,
    beacon_uptime: Option<Value>,
    beacon_changed: Instant,
    beacon_failures: u32,
}

pub struct Monitor {
    config: Config,
    hub: Arc<Hub>,
    state: Mutex<State>,
    poke: (Mutex<bool>, Condvar),
}

impl Monitor {
    pub fn start(config: Config, hub: Arc<Hub>) -> Arc<Monitor> {
        let m = Arc::new(Monitor {
            state: Mutex::new(State {
                projector: json!({"configured": !config.projector_host.is_empty(), "reachable": false}),
                beacon: json!({"configured": !config.beacon_host.is_empty(), "reachable": false}),
                waking_until: None,
                beacon_uptime: None,
                beacon_changed: Instant::now(),
                beacon_failures: 0,
            }),
            config,
            hub,
            poke: (Mutex::new(false), Condvar::new()),
        });
        let worker = m.clone();
        thread::spawn(move || worker.run());
        m
    }

    fn run(&self) {
        let mut beacon_due = Instant::now();
        loop {
            self.poll_projector();
            if Instant::now() >= beacon_due {
                let ok = self.poll_beacon();
                // a failed poll is repeated soon: the box answers slowly now and then
                beacon_due = Instant::now() + Duration::from_secs(if ok { 20 } else { 4 });
            }
            let (lock, cond) = &self.poke;
            let mut poked = lock.lock().unwrap();
            if !*poked {
                poked = cond.wait_timeout(poked, Duration::from_secs(5)).unwrap().0;
            }
            *poked = false;
        }
    }

    fn poke(&self) {
        *self.poke.0.lock().unwrap() = true;
        self.poke.1.notify_all();
    }

    fn poll_projector(&self) {
        let host = &self.config.projector_host;
        if host.is_empty() {
            return;
        }
        let inputs: Vec<Value> = ACER_INPUTS.iter().map(|(c, l)| json!({"code": c, "label": l})).collect();
        let mut p = json!({"configured": true, "reachable": false, "host": host, "model": self.config.projector_model,
                           "inputs": inputs, "errors": [], "lamp_hours": null});
        let answer = acer_post(host, "/form/home_cgi", "QueryControl");
        let mut st = self.state.lock().unwrap();
        match answer {
            Err(_) => {
                // switched off, the projector leaves the network; Wake-on-LAN brings it back
                let waking = st.waking_until.is_some_and(|t| Instant::now() < t);
                p["power"] = json!(if waking { "warming" } else { "off" });
                p["offline"] = json!(true);
            }
            Ok(home) => {
                st.waking_until = None;
                p["reachable"] = json!(true);
                let status = home.get("sysstatus").map(String::as_str).unwrap_or("");
                p["power"] = json!(match status {
                    "0" => "off",
                    "3" => "cooling",
                    "" => "unknown",
                    _ => "on",
                });
                if let Some(h) = home.get("lamphur").and_then(|v| v.parse::<u64>().ok()) {
                    p["lamp_hours"] = json!(h);
                }
                if let Some(e) = home.get("errorstatus").and_then(|c| acer_error(c)) {
                    p["errors"] = json!([{"part": e, "level": "error"}]);
                }
                if p["power"] == "on" {
                    let input = home.get("displaysrc").cloned().unwrap_or_default();
                    let label = ACER_INPUTS.iter().find(|(c, _)| *c == input).map(|(_, l)| *l).unwrap_or("");
                    p["input"] = json!(input);
                    p["input_label"] = json!(label);
                }
            }
        }
        st.projector = p;
    }

    /// Returns true if the box answered.
    fn poll_beacon(&self) -> bool {
        let host = &self.config.beacon_host;
        if host.is_empty() {
            return true;
        }
        let reply = http::request("GET", &format!("http://{host}/beacon.json"), b"", "", Duration::from_secs(5))
            .ok()
            .filter(|(status, _)| *status == 200)
            .and_then(|(_, body)| serde_json::from_slice::<Value>(&body).ok())
            .filter(Value::is_object);
        let mut st = self.state.lock().unwrap();
        match reply {
            Some(mut b) => {
                st.beacon_failures = 0;
                // The box has no reliable clock: its report is stale when the uptime in it has
                // not moved for three minutes (it rewrites the file every minute).
                let uptime = b.get("uptime").cloned();
                if uptime != st.beacon_uptime {
                    st.beacon_uptime = uptime;
                    st.beacon_changed = Instant::now();
                }
                b["configured"] = json!(true);
                b["reachable"] = json!(true);
                b["host"] = json!(host);
                b["stale"] = json!(st.beacon_changed.elapsed() > Duration::from_secs(180));
                st.beacon = b;
                true
            }
            None => {
                st.beacon_failures += 1;
                // one missed answer is not shown; the box is busy now and then
                if st.beacon_failures >= 2 || st.beacon["reachable"] != true {
                    st.beacon = json!({"configured": true, "reachable": false, "host": host, "error": "antwortet nicht"});
                }
                false
            }
        }
    }

    /// Sends one control command to the projector. Returns an error text.
    pub fn command(&self, action: &str, value: &str) -> Result<(), String> {
        let host = self.config.projector_host.clone();
        if host.is_empty() {
            return Err("Kein Beamer konfiguriert".into());
        }
        let current = self.state.lock().unwrap().projector.clone();
        let reachable = current["reachable"] == true;
        let result = (|| -> Result<(), String> {
            let fail = |_| "Der Beamer antwortet nicht".to_string();
            if action == "power" && value == "on" {
                if reachable {
                    if current["power"] == "off" {
                        acer_post(&host, "/form/control_cgi", "pwr=pwr").map_err(fail)?;
                    }
                    return Ok(());
                }
                if self.config.wake_url.is_empty() {
                    return Err("Einschalten ist nicht eingerichtet (wake_url fehlt)".into());
                }
                let (_, body) = http::request("POST", &self.config.wake_url, b"wake", "text/plain", Duration::from_secs(6))
                    .map_err(|_| "Der Funk-Sender antwortet nicht".to_string())?;
                if !String::from_utf8_lossy(&body).contains("ok") {
                    return Err("Der Funk-Sender konnte das Weck-Paket nicht schicken".into());
                }
                self.state.lock().unwrap().waking_until = Some(Instant::now() + Duration::from_secs(90));
                return Ok(());
            }
            if !reachable {
                return Err("Der Beamer ist aus".into());
            }
            if action == "power" && value == "off" {
                if current["power"] == "on" {
                    acer_post(&host, "/form/control_cgi", "pwr=pwr").map_err(fail)?; // toggle only
                }
            } else if action == "input" && ACER_INPUTS.iter().any(|(c, _)| *c == value) {
                acer_post(&host, "/form/control_cgi", &format!("src={value}")).map_err(fail)?;
            } else {
                return Err("Unbekannter Befehl".into());
            }
            Ok(())
        })();
        self.poke();
        result
    }

    fn status(&self) -> Value {
        let (projector, beacon) = {
            let st = self.state.lock().unwrap();
            (st.projector.clone(), st.beacon.clone())
        };
        let session = self.hub.session().map(|s| {
            json!({"source": s.source, "ip": s.ip, "playing": s.playing, "seconds": s.started.elapsed().as_secs(),
                   "frames": s.frames, "video": s.video, "output": self.hub.output()})
        });
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        json!({"projector": projector, "beacon": beacon,
               "receiver": {"session": session, "name": self.config.room_name}, "time": now})
    }
}

pub fn start(bind: &str, port: u16, monitor: Arc<Monitor>, hub: Arc<Hub>) -> io::Result<()> {
    let listener = TcpListener::bind((bind, port))?;
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (monitor, hub) = (monitor.clone(), hub.clone());
            thread::spawn(move || {
                let _ = handle(conn, &monitor, &hub);
            });
        }
    });
    Ok(())
}

fn respond(stream: &mut TcpStream, code: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        415 => "Unsupported Media Type",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)
}

fn handle(mut stream: TcpStream, monitor: &Monitor, hub: &Hub) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.by_ref().take(2048).read_line(&mut first)?;
    let mut content_type = String::new();
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.by_ref().take(4096).read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-type" => content_type = v.trim().to_ascii_lowercase(),
                "content-length" => length = v.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut parts = first.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    match (method, path) {
        ("GET", "/") => respond(&mut stream, 200, "text/html; charset=utf-8", PAGE.as_bytes()),
        ("GET", "/api/status") => respond(&mut stream, 200, "application/json", monitor.status().to_string().as_bytes()),
        ("GET", "/frame.jpg") => match hub.wait_frame(u64::MAX, Duration::ZERO).1 {
            Some(f) => respond(&mut stream, 200, "image/jpeg", &f),
            None => respond(&mut stream, 503, "text/plain", b"no session"),
        },
        ("GET", "/stream") => {
            stream.set_write_timeout(Some(Duration::from_secs(10)))?;
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n")?;
            let mut serial = u64::MAX;
            loop {
                let (s, frame) = hub.wait_frame(serial, Duration::from_secs(5));
                let changed = s != serial;
                serial = s;
                if let (true, Some(f)) = (changed, frame) {
                    stream.write_all(format!("--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", f.len()).as_bytes())?;
                    stream.write_all(&f)?;
                    stream.write_all(b"\r\n")?;
                }
            }
        }
        ("POST", "/api/projector") => {
            // browsers cannot send JSON cross-site without a preflight, which is never answered
            if !content_type.contains("application/json") {
                return respond(&mut stream, 415, "application/json",
                    json!({"ok": false, "error": "Content-Type muss application/json sein"}).to_string().as_bytes());
            }
            let mut body = vec![0u8; length.min(1024)];
            reader.read_exact(&mut body)?;
            let parsed: Option<(String, String)> = serde_json::from_slice::<Value>(&body).ok().and_then(|v| {
                let text = |k: &str| v.get(k).map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string()));
                Some((text("action")?, text("value")?))
            });
            let Some((action, value)) = parsed else {
                return respond(&mut stream, 400, "application/json", json!({"ok": false, "error": "Ungültige Anfrage"}).to_string().as_bytes());
            };
            let result = monitor.command(&action, &value);
            if let Err(e) = &result {
                log(format!("panel: {action} {value} failed: {e}"));
            }
            let code = if result.is_ok() { 200 } else { 502 };
            respond(&mut stream, code, "application/json", json!({"ok": result.is_ok(), "error": result.err()}).to_string().as_bytes())
        }
        _ => respond(&mut stream, 404, "text/plain", b"not found"),
    }
}
