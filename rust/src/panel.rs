//! Web panel: status of projector, beacon box and receiver; projector control.
//!
//!   GET  /                 the open panel (basic functions, screen sharing)
//!   GET  /admin            the whole panel; the Plintus app puts its own PIN in front of it
//!   GET  /api/status       everything the panel shows, as JSON
//!   GET  /api/audio        sound level of the running session
//!   GET  /api/share        WebSocket: screen sharing from the browser (share.rs)
//!   GET  /api/state        the short state for home automation (power, input, hide, freeze, presenter)
//!   POST /api/projector    {"action": "power", "value": "on"|"off"|"toggle"} | {"action": "input", "value": "hdmi1"|"33"}
//!                          {"action": "hide"|"freeze", "value": "on"|"off"|"toggle"} and the panel's own actions
//!   GET  /stream           live picture of the running session (MJPEG)
//!   GET  /frame.jpg        newest frame

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::Config;
use crate::admin;
use crate::ezdisplay::{Output, Remote};
use crate::share::{self, ShareOptions};
use crate::http::{self, acer_post};
use crate::hub::{Hub, log};

const PAGE: &str = include_str!("../panel.html");
const ACER_INPUTS: [(&str, &str); 8] = [
    ("3", "HDMI 1"), ("6", "HDMI 2"), ("33", "Drahtlos"), ("20", "VGA 1"),
    ("21", "VGA 2"), ("22", "Video"), ("34", "Medien"), ("35", "USB"),
];
/// Names of the inputs for home automation.
const INPUT_NAMES: [(&str, &str); 8] = [
    ("hdmi1", "3"), ("hdmi2", "6"), ("wireless", "33"), ("vga1", "20"),
    ("vga2", "21"), ("video", "22"), ("media", "34"), ("usb", "35"),
];

/// "on" / "off" / "toggle" (or nothing) of a home automation command.
fn wanted(value: &str) -> Result<Option<bool>, String> {
    match value {
        "on" | "1" | "true" => Ok(Some(true)),
        "off" | "0" | "false" => Ok(Some(false)),
        "toggle" | "" => Ok(None),
        _ => Err("Wert muss on, off oder toggle sein".into()),
    }
}

/// Settings of the projector's control page (/form/control_cgi) that take a value ("key=value").
const ACER_SETTINGS: [&str; 14] = ["src", "mod", "aks", "apr", "prj", "lgo", "bri", "con", "vks", "hks", "gam", "ctp", "vol", "zom"];
/// Buttons of the control page ("key=key"): toggles, one-shot actions and the -/+ steps
/// (suffix 1 = minus, 2 = plus) of the adjustable values.
const ACER_BUTTONS: [&str; 21] = [
    "hid", "frz", "eco", "syc", "rst", "bri1", "bri2", "con1", "con2", "vks1", "vks2", "hks1", "hks2",
    "gam1", "gam2", "ctp1", "ctp2", "vol1", "vol2", "zom1", "zom2",
];

/// Plain values only: digits, letters, '.', '-' (the projector answers e.g. gam "1.8", ctp "CT2").
fn acer_value_ok(v: &str) -> bool {
    !v.is_empty() && v.len() <= 8 && v.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
}

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

/// Wake-on-LAN magic packet (6 x 0xff, then the MAC 16 times) to UDP 9, sent to the
/// projector's address and to the directed broadcast of its /24.
fn wake_on_lan(mac: &str, host: &str) -> Result<(), String> {
    let bytes: Vec<u8> = mac.split([':', '-']).filter_map(|p| u8::from_str_radix(p, 16).ok()).collect();
    if bytes.len() != 6 {
        return Err("projector_mac fehlt".into());
    }
    let ip: std::net::Ipv4Addr = host.parse().map_err(|_| "projector_host ist keine IPv4-Adresse".to_string())?;
    let mut packet = vec![0xffu8; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&bytes);
    }
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    let _ = sock.set_broadcast(true);
    let o = ip.octets();
    let broadcast = std::net::Ipv4Addr::new(o[0], o[1], o[2], 255);
    let mut ok = false;
    for target in [ip, broadcast] {
        ok |= sock.send_to(&packet, (target, 9)).is_ok();
    }
    if ok { Ok(()) } else { Err("Weck-Paket konnte nicht gesendet werden".into()) }
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
    output: Option<Output>,
    state: Mutex<State>,
    admin: Mutex<Value>,
    poke: (Mutex<bool>, Condvar),
    admin_poke: (Mutex<bool>, Condvar),
}

impl Monitor {
    pub fn start(config: Config, hub: Arc<Hub>, output: Option<Output>) -> Arc<Monitor> {
        let m = Arc::new(Monitor {
            output,
            admin: Mutex::new(Value::Null),
            admin_poke: (Mutex::new(false), Condvar::new()),
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
        let worker = m.clone();
        thread::spawn(move || worker.run_admin());
        m
    }

    /// The admin pages answer slowly: read them on their own thread, every 30 s while the
    /// projector is on, and right after an admin command.
    fn run_admin(&self) {
        loop {
            let on = self.state.lock().unwrap().projector["power"] == "on";
            let data = if on { admin::read(&self.config.projector_host) } else { Value::Null };
            *self.admin.lock().unwrap() = data;
            let (lock, cond) = &self.admin_poke;
            let mut poked = lock.lock().unwrap();
            if !*poked {
                poked = cond.wait_timeout(poked, Duration::from_secs(if on { 30 } else { 10 })).unwrap().0;
            }
            *poked = false;
        }
    }

    /// Admin commands (admin.rs) and actions through the open projector connection.
    pub fn admin_command(&self, action: &str, value: &str, fields: &Value) -> Result<Value, String> {
        if action == "p2p_mode" {
            return self.set_p2p_mode(value);
        }
        if action == "session_stop" {
            return if self.hub.stop_session() { Ok(Value::Null) } else { Err("Gerade zeigt niemand".into()) };
        }
        if action == "audio_target" {
            return self.hub.audio.set_target(value).map(|_| Value::Null);
        }
        let remote = match action {
            "key" => Some(Remote::Key(value.parse().ok().filter(|k| (10..=15).contains(k)).ok_or("Unbekannte Taste")?)),
            "volume" => Some(Remote::Volume { up: value == "up" }),
            "disconnect_all" => Some(Remote::DisconnectAll),
            "guest" => Some(Remote::AnswerGuest { ip: value.to_string(), allow: fields["allow"] == true }),
            _ => None,
        };
        if let Some(r) = remote {
            let out = self.output.as_ref().ok_or("Bildausgabe zum Beamer ist nicht eingerichtet")?;
            return out.remote(r).map(|_| Value::Null);
        }
        if self.config.projector_host.is_empty() {
            return Err("Kein Beamer konfiguriert".into());
        }
        if self.state.lock().unwrap().projector["reachable"] != true {
            return Err("Der Beamer ist aus".into());
        }
        let result = admin::command(&self.config.projector_host, action, value, fields);
        *self.admin_poke.0.lock().unwrap() = true;
        self.admin_poke.1.notify_all();
        result
    }

    /// Wi-Fi Direct on the box (install-p2p.sh): "off", "fallback" or "only". The box switches
    /// in the background (about 20 s, the beacon is gone for a moment meanwhile).
    fn set_p2p_mode(&self, mode: &str) -> Result<Value, String> {
        if !matches!(mode, "off" | "fallback" | "only") {
            return Err("Unbekannte Einstellung (off, fallback oder only)".into());
        }
        let host = &self.config.beacon_host;
        if host.is_empty() {
            return Err("Kein Funk-Sender eingerichtet (beacon_host)".into());
        }
        if self.hub.session().is_some() {
            return Err("Gerade läuft eine Übertragung; Umschalten würde sie unterbrechen. Bitte danach umschalten.".into());
        }
        let (status, body) = http::request("POST", &format!("http://{host}/cgi-bin/p2p-mode"), mode.as_bytes(),
                                           "text/plain", Duration::from_secs(10))
            .map_err(|_| "Der Funk-Sender antwortet nicht".to_string())?;
        let reply: Value = serde_json::from_slice(&body).map_err(|_| {
            format!("Der Funk-Sender kennt den Schalter nicht (HTTP {status}): install-p2p.sh auf der Box neu ausführen")
        })?;
        if reply["ok"] != true {
            return Err(match reply["error"].as_str() {
                Some("busy") => "Der Funk-Sender schaltet gerade noch um".to_string(),
                Some(e) => format!("Funk-Sender: {e}"),
                None => "Der Funk-Sender hat nicht umgeschaltet".to_string(),
            });
        }
        {
            let mut st = self.state.lock().unwrap();
            st.beacon["p2p"] = json!(mode);
            st.beacon["p2p_busy"] = json!(true);
        }
        log(format!("panel: Wi-Fi Direct mode {mode}"));
        Ok(Value::Null)
    }

    fn run(&self) {
        let mut beacon_due = Instant::now();
        loop {
            self.poll_projector();
            if Instant::now() >= beacon_due {
                let ok = self.poll_beacon();
                let switching = self.state.lock().unwrap().beacon["p2p_busy"] == true;
                // a failed poll is repeated soon: the box answers slowly now and then
                beacon_due = Instant::now() + Duration::from_secs(if !ok { 4 } else if switching { 5 } else { 20 });
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
        let on = p["power"] == "on";
        if on {
            if let Some(old) = st.projector.get("settings") {
                p["settings"] = old.clone(); // until the fresh query below is in
            }
        }
        st.projector = p;
        drop(st);
        if on {
            // all settings of the control page; the same query the projector's own page sends
            if let Ok(c) = acer_post(host, "/form/control_cgi", "QueryControl") {
                self.state.lock().unwrap().projector["settings"] = json!(c);
            }
        }
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
        // short forms for home automation
        match action {
            "power" if value == "toggle" => {
                let on = current["power"] == "on";
                return self.command("power", if on { "off" } else { "on" });
            }
            "input" => {
                if let Some((_, code)) = INPUT_NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(value)) {
                    return self.command("input", code);
                }
            }
            "hide" | "freeze" => {
                let key = if action == "hide" { "hid" } else { "frz" };
                let want = wanted(value)?;
                if current["power"] != "on" {
                    return Err("Der Beamer ist nicht eingeschaltet".into());
                }
                if let Some(want) = want {
                    // the button only toggles: ask the projector first
                    let fresh = acer_post(&host, "/form/control_cgi", "QueryControl")
                        .map_err(|_| "Der Beamer antwortet nicht".to_string())?;
                    if (fresh.get(key).map(String::as_str) == Some("1")) == want {
                        return Ok(());
                    }
                }
                return self.command("press", key);
            }
            _ => {}
        }
        let result = (|| -> Result<(), String> {
            let fail = |_| "Der Beamer antwortet nicht".to_string();
            if action == "power" && value == "on" {
                if reachable {
                    if current["power"] == "off" {
                        acer_post(&host, "/form/control_cgi", "pwr=pwr").map_err(fail)?;
                    }
                    return Ok(());
                }
                // The VM sends the wake packet itself (works if the router passes directed
                // broadcasts or keeps the projector's ARP entry); the box in the projector's
                // subnet is the fallback that always reaches it.
                let own = wake_on_lan(&self.config.projector_mac, &host);
                if own.is_err() && self.config.wake_url.is_empty() {
                    return Err("Einschalten ist nicht eingerichtet (projector_mac oder wake_url fehlt)".into());
                }
                if !self.config.wake_url.is_empty() {
                    let sent = http::request("POST", &self.config.wake_url, b"wake", "text/plain", Duration::from_secs(6))
                        .is_ok_and(|(_, body)| String::from_utf8_lossy(&body).contains("ok"));
                    if !sent && own.is_err() {
                        return Err("Der Funk-Sender konnte das Weck-Paket nicht schicken".into());
                    }
                }
                self.state.lock().unwrap().waking_until = Some(Instant::now() + Duration::from_secs(90));
                return Ok(());
            }
            if !reachable {
                return Err("Der Beamer ist aus".into());
            }
            let body = if action == "power" && value == "off" {
                if current["power"] != "on" {
                    return Ok(());
                }
                "pwr=pwr".to_string() // toggle only
            } else if action == "input" && ACER_INPUTS.iter().any(|(c, _)| *c == value) {
                format!("src={value}")
            } else if current["power"] != "on" {
                return Err("Der Beamer ist nicht eingeschaltet".into());
            } else if action == "press" && ACER_BUTTONS.contains(&value) {
                format!("{value}={value}")
            } else if let Some(key) = action.strip_prefix("set:").filter(|k| ACER_SETTINGS.contains(k)) {
                if !acer_value_ok(value) {
                    return Err("Ungültiger Wert".into());
                }
                format!("{key}={value}")
            } else {
                return Err("Unbekannter Befehl".into());
            };
            // the answer carries the new settings: show them at once
            let answer = acer_post(&host, "/form/control_cgi", &body).map_err(fail)?;
            if !answer.is_empty() {
                let mut st = self.state.lock().unwrap();
                if let Some(Value::Object(s)) = st.projector.get_mut("settings") {
                    for (k, v) in answer {
                        s.insert(k, json!(v));
                    }
                }
            }
            Ok(())
        })();
        self.poke();
        result
    }

    /// The short state for home automation; served from what the monitor already knows.
    fn short_state(&self) -> Value {
        let p = self.state.lock().unwrap().projector.clone();
        let on = p["power"] == "on";
        let setting = |k: &str| on && p["settings"][k] == "1";
        let input = p["input"].as_str()
            .and_then(|c| INPUT_NAMES.iter().find(|(_, code)| *code == c))
            .map(|(n, _)| *n);
        let presenter = self.hub.session().map(|s| s.source);
        json!({"power": p.get("power").cloned().unwrap_or(json!("unknown")), "input": input,
               "input_label": p.get("input_label").cloned().unwrap_or(Value::Null),
               "hide": setting("hid"), "freeze": setting("frz"), "presenter": presenter})
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
        let guests: Vec<Value> = self.hub.guests().into_iter()
            .map(|g| json!({"name": g.name, "ip": g.ip, "fullscreen": g.fullscreen})).collect();
        json!({"projector": projector, "beacon": beacon, "admin": *self.admin.lock().unwrap(), "guests": guests,
               "audio": self.hub.audio.status(),
               "receiver": {"session": session, "name": self.config.room_name, "wifi_direct": self.config.wifi_direct},
               "time": now})
    }
}

pub fn start(bind: &str, port: u16, monitor: Arc<Monitor>, hub: Arc<Hub>, share: ShareOptions) -> io::Result<()> {
    let listener = TcpListener::bind((bind, port))?;
    let share = Arc::new(share);
    thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (monitor, hub, share) = (monitor.clone(), hub.clone(), share.clone());
            thread::spawn(move || {
                let _ = handle(conn, &monitor, &hub, &share);
            });
        }
    });
    Ok(())
}

/// The page, as the open panel or as the admin panel.
fn page(admin: bool) -> String {
    if admin { PAGE.replacen("<body data-page=\"open\">", "<body data-page=\"admin\">", 1) } else { PAGE.to_string() }
}

fn respond(stream: &mut TcpStream, code: u16, content_type: &str, body: &[u8]) -> io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
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

fn handle(mut stream: TcpStream, monitor: &Monitor, hub: &Arc<Hub>, share: &ShareOptions) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.by_ref().take(2048).read_line(&mut first)?;
    let mut content_type = String::new();
    let mut length = 0usize;
    let (mut upgrade, mut ws_key, mut origin, mut host) = (String::new(), String::new(), String::new(), String::new());
    loop {
        let mut line = String::new();
        if reader.by_ref().take(4096).read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-type" => content_type = v.trim().to_ascii_lowercase(),
                "content-length" => length = v.trim().parse().unwrap_or(0),
                "upgrade" => upgrade = v.trim().to_ascii_lowercase(),
                "sec-websocket-key" => ws_key = v.trim().to_string(),
                "origin" => origin = v.trim().to_ascii_lowercase(),
                "host" => host = v.trim().to_ascii_lowercase(),
                _ => {}
            }
        }
    }
    let mut parts = first.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let path = target.split('?').next().unwrap_or("");
    match (method, path) {
        ("GET", "/") => respond(&mut stream, 200, "text/html; charset=utf-8", page(false).as_bytes()),
        ("GET", "/admin") => respond(&mut stream, 200, "text/html; charset=utf-8", page(true).as_bytes()),
        ("GET", "/api/audio") => respond(&mut stream, 200, "application/json", hub.audio.status().to_string().as_bytes()),
        ("GET", "/api/share") => {
            // A WebSocket is not protected by CORS: only pages of this panel may open it.
            let own = origin.split_once("://").is_some_and(|(_, h)| h == host);
            if upgrade != "websocket" || ws_key.is_empty() || !own {
                return respond(&mut stream, 403, "text/plain", b"WebSocket only from this panel");
            }
            let peer = stream.peer_addr()?.ip();
            share::serve(stream, reader, &ws_key, peer, hub.clone(), monitor.output.clone(), share)
        }
        ("GET", "/api/status") => respond(&mut stream, 200, "application/json", monitor.status().to_string().as_bytes()),
        ("GET", "/api/state") => respond(&mut stream, 200, "application/json", monitor.short_state().to_string().as_bytes()),
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
            let mut body = vec![0u8; length.min(8192)];
            reader.read_exact(&mut body)?;
            let parsed: Option<(String, String, Value)> = serde_json::from_slice::<Value>(&body).ok().and_then(|v| {
                let text = |k: &str| v.get(k).map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string()));
                Some((text("action")?, text("value").unwrap_or_default(), v.get("fields").cloned().unwrap_or(Value::Null)))
            });
            let Some((action, value, fields)) = parsed else {
                return respond(&mut stream, 400, "application/json", json!({"ok": false, "error": "Ungültige Anfrage"}).to_string().as_bytes());
            };
            let picture = matches!(action.as_str(), "power" | "input" | "press" | "hide" | "freeze") || action.starts_with("set:");
            let result = if picture { monitor.command(&action, &value).map(|_| Value::Null) } else { monitor.admin_command(&action, &value, &fields) };
            // never log form fields: they may hold passwords
            match &result {
                Err(e) => log(format!("panel: {action} {value} failed: {e}")),
                Ok(_) if !picture => log(format!("panel: {action} {value}")),
                Ok(_) => {}
            }
            let code = if result.is_ok() { 200 } else { 502 };
            let reply = match result {
                Ok(data) => json!({"ok": true, "error": null, "data": data}),
                Err(e) => json!({"ok": false, "error": e}),
            };
            respond(&mut stream, code, "application/json", reply.to_string().as_bytes())
        }
        _ => respond(&mut stream, 404, "text/plain", b"not found"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn admin_page_only_changes_the_body_tag() {
        let admin = super::page(true);
        assert!(admin.contains("<body data-page=\"admin\">"));
        // the CSS rule that hides admin sections on the open page must stay as it is
        assert!(admin.contains("body[data-page=\"open\"] .admin-only"));
        assert!(super::page(false).contains("<body data-page=\"open\">"));
    }
}
