//! Administration of the projector beyond picture control: the EZCast Pro module (port 8080)
//! and the admin pages of the Acer web interface (port 80). Request formats come from the
//! projector's own web pages (static inventory, analysis/out/WEB_FUNCTIONS.md); only the
//! functions this firmware answers are offered.
//!
//! Secrets are never read back: the projector hands out its passwords to anyone who asks
//! (network.asp, get_softap_psk, am_wifisetting), the panel only sets new ones.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::http::{self, acer_post};

const T: Duration = Duration::from_secs(5);

fn millis() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// application/x-www-form-urlencoded / query encoding
pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn text(body: Vec<u8>) -> String {
    String::from_utf8_lossy(&body).trim().to_string()
}

fn get(host: &str, path: &str) -> Result<String, String> {
    match http::request("GET", &format!("http://{host}:8080{path}"), b"", "", T) {
        Ok((200, body)) => Ok(text(body)),
        Ok((code, _)) => Err(format!("EZCast-Modul antwortet mit {code}")),
        Err(_) => Err("EZCast-Modul antwortet nicht".into()),
    }
}

fn post(host: &str, path: &str, body: &str) -> Result<String, String> {
    match http::request("POST", &format!("http://{host}:8080{path}"), body.as_bytes(), "application/x-www-form-urlencoded", T) {
        Ok((200, body)) => Ok(text(body)),
        Ok((code, _)) => Err(format!("EZCast-Modul antwortet mit {code}")),
        Err(_) => Err("EZCast-Modul antwortet nicht".into()),
    }
}

/// wifi_info_GET.cgi: one "type" value, fields separated by "\n"
fn wifi_info(host: &str, fields: &[&str]) -> Result<String, String> {
    get(host, &format!("/cgi-bin/wifi_info_GET.cgi?timeStamp={}&type={}", millis(), urlencode(&fields.join("\n"))))
}

fn ipv4(v: &str, what: &str) -> Result<String, String> {
    v.trim().parse::<Ipv4Addr>().map(|a| a.to_string()).map_err(|_| format!("{what}: keine gültige IPv4-Adresse"))
}

fn field<'a>(f: &'a Value, k: &str) -> &'a str {
    f.get(k).and_then(Value::as_str).unwrap_or("").trim()
}

fn safe(v: &str, max: usize, extra: &str, what: &str) -> Result<String, String> {
    if v.chars().count() > max || !v.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(c)) {
        return Err(format!("{what}: höchstens {max} Zeichen, nur Buchstaben, Ziffern und {extra}"));
    }
    Ok(v.to_string())
}

// ---- reading ------------------------------------------------------------------------------------

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{name}>"))? + start;
    Some(xml[start..end].trim())
}

/// conference_control.cgi?type=query_tcp_connections
pub fn parse_conference(xml: &str) -> Value {
    let split = tag(xml, "SplitCount").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1);
    let clients: Vec<Value> = xml
        .split("<Connection>")
        .skip(1)
        .map(|c| {
            let name = tag(c, "HostName").filter(|n| *n != "(null)").unwrap_or("");
            json!({"name": name, "ip": tag(c, "IPAddress").unwrap_or(""), "position": tag(c, "Position").unwrap_or(""),
                   "type": tag(c, "Type").unwrap_or(""), "role": tag(c, "Role").unwrap_or("")})
        })
        .collect();
    json!({"split": split, "clients": clients})
}

/// <input> fields of a server-rendered page: name -> value, radios -> the checked value,
/// checkboxes -> "1"/"0". Password fields are skipped.
pub fn form_values(html: &str) -> HashMap<String, String> {
    form_fields(html, false)
}

/// The stored password of a page field, used only to send it back unchanged when the panel
/// leaves the field empty (never shown).
fn stored_secret(host: &str, page: &str, name: &str) -> String {
    http::request("GET", &format!("http://{host}/{page}"), b"", "", T)
        .ok()
        .and_then(|(_, body)| form_fields(&String::from_utf8_lossy(&body), true).remove(name))
        .unwrap_or_default()
}

fn form_fields(html: &str, secrets: bool) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for tag in html.split("<input").skip(1) {
        let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
        let attr = |name: &str| -> Option<String> {
            let lower = tag.to_ascii_lowercase();
            let mut from = 0;
            while let Some(i) = lower[from..].find(&format!("{name}=")) {
                let at = from + i;
                from = at + 1;
                if at > 0 && !lower.as_bytes()[at - 1].is_ascii_whitespace() {
                    continue; // e.g. "id=" inside "valid="
                }
                let rest = &tag[at + name.len() + 1..];
                return Some(match rest.chars().next() {
                    Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or("").to_string(),
                    _ => rest.split(|c: char| c.is_whitespace()).next().unwrap_or("").to_string(),
                });
            }
            None
        };
        let (Some(name), kind) = (attr("name"), attr("type").unwrap_or_default().to_ascii_lowercase()) else { continue };
        let checked = tag.to_ascii_lowercase().split_whitespace().any(|w| w == "checked");
        match kind.as_str() {
            "password" if secrets => {
                out.insert(name, attr("value").unwrap_or_default());
            }
            "password" | "button" | "submit" | "hidden" => {}
            "radio" => {
                if checked {
                    out.insert(name, attr("value").unwrap_or_default());
                }
            }
            "checkbox" => {
                out.insert(name, if checked { "1" } else { "0" }.into());
            }
            _ => {
                out.insert(name, attr("value").unwrap_or_default());
            }
        }
    }
    out
}

/// Everything the admin part of the panel shows. Slow (the EZCast CGI is slow): call it from
/// a background thread.
pub fn read(host: &str) -> Value {
    let mut ez = json!({});
    if let Ok((200, body)) = http::request("GET", &format!("http://{host}:8080/dongleInfo.json"), b"", "", T) {
        if let Ok(info) = serde_json::from_slice::<Value>(&body) {
            ez["firmware"] = info["firmware_version"].clone();
            ez["chip"] = info["chip_id"].clone();
            ez["name"] = info["device_name"]["val"].clone();
            ez["resolution"] = json!(format!("{}x{} @ {} Hz", info["resolution"]["width"], info["resolution"]["height"],
                                              info["resolution"]["refresh_rate"]));
            ez["soft_ap"] = info["ssid_soft_ap"].clone();
        }
    }
    if let Ok(n) = wifi_info(host, &["get_connections"]) {
        ez["connections"] = json!(n);
    }
    if let Ok(s) = wifi_info(host, &["get_connected_ssid"]) {
        ez["wifi"] = json!(if s.starts_with("to_wifi_list") { String::new() } else { s });
    }
    if let Ok(l) = wifi_info(host, &["get_netlink"]) {
        ez["lan_cable"] = json!(l == "1");
    }
    if let Ok(x) = get(host, "/cgi-bin/conference_control.cgi?type=query_tcp_connections") {
        ez["conference"] = parse_conference(&x);
    }

    let mut acer = json!({});
    if let Ok((200, body)) = http::request("GET", &format!("http://{host}/network.asp"), b"", "", T) {
        let f = form_values(&String::from_utf8_lossy(&body));
        let g = |k: &str| f.get(k).cloned().unwrap_or_default();
        acer["lan"] = json!({"dhcp": g("LanDhcp") == "1", "ip": g("IP"), "mask": g("MSK"), "gateway": g("GW"), "dns": g("DNS")});
        acer["crestron"] = json!({"ip": g("ctrsysip"), "id": g("ctrsysipid"), "port": g("ctrsysport")});
        acer["login"] = json!({"admin": g("enadmin") == "1", "user": g("enuser") == "1"});
    }
    if let Ok((200, body)) = http::request("GET", &format!("http://{host}/email.asp"), b"", "", T) {
        let f = form_values(&String::from_utf8_lossy(&body));
        let g = |k: &str| f.get(k).cloned().unwrap_or_default();
        acer["email"] = json!({"to": g("RECIVER"), "cc": g("RECIVER2"), "subject": g("SUBJECT"), "from": g("SENDER"),
                               "server": g("MAIL_SERVER"), "user": g("MAIL_USER"),
                               "alerts": [g("altc1") == "1", g("altc2") == "1", g("altc3") == "1", g("altc4") == "1"]});
    }
    json!({"ezcast": ez, "acer": acer})
}

// ---- commands ------------------------------------------------------------------------------

/// Runs one admin command. `f` holds the fields of forms. Returns data for the panel, if any.
pub fn command(host: &str, action: &str, value: &str, f: &Value) -> Result<Value, String> {
    let done = Ok(Value::Null);
    match action {
        // -- conference control (who is on the screen) --
        "conf" => {
            let ip = ipv4(field(f, "ip"), "Teilnehmer")?;
            let control = match value {
                "kick" => "control_disconnect".to_string(),
                "host" => "control_assign_host".to_string(),
                "stop" => "control_stop".to_string(),
                v => {
                    // "1:1" full screen, "2:1".."2:2" halves, "4:1".."4:4" quarters
                    let (count, pos) = v.split_once(':').ok_or("Unbekannte Aufteilung")?;
                    let ok = matches!((count, pos), ("1", "1") | ("2", "1" | "2") | ("4", "1" | "2" | "3" | "4"));
                    if !ok {
                        return Err("Unbekannte Aufteilung".into());
                    }
                    format!("control_change&split_count={count}&position={pos}")
                }
            };
            let r = get(host, &format!("/cgi-bin/conference_control.cgi?type=control&control={control}&ip_address={ip}"))?;
            if r != "1" { Err(format!("Der Beamer lehnt ab ({r})")) } else { done }
        }
        "conf_all" => {
            // the projector's own page sends a fixed address with this command
            get(host, "/cgi-bin/conference_control.cgi?type=control&control=control_disconnect_all&ip_address=192.168.168.20")?;
            done
        }

        // -- Wi-Fi client of the EZCast module --
        "wifi_scan" => {
            wifi_info(host, &["wifi_start"])?;
            for _ in 0..8 {
                thread::sleep(Duration::from_secs(2));
                if wifi_info(host, &["wifi_scan"]).ok().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0) > 0 {
                    break;
                }
            }
            let names = wifi_info(host, &["get_apindex_string"])?;
            let signal = wifi_info(host, &["get_ssid_signal"]).unwrap_or_default();
            let mut s = signal.lines();
            let list: Vec<Value> = names
                .lines()
                .filter(|n| !n.trim().is_empty())
                .map(|n| json!({"ssid": n.trim(), "signal": s.next().unwrap_or("").trim()}))
                .collect();
            Ok(json!(list))
        }
        "wifi_join" | "wifi_forget" => {
            let ssid = field(f, "ssid");
            let psk = field(f, "psk");
            if ssid.is_empty() || ssid.len() > 32 {
                return Err("Netzname fehlt".into());
            }
            let open = psk.is_empty();
            if action == "wifi_join" && !open && !(8..=63).contains(&psk.len()) {
                return Err("Das WLAN-Passwort hat 8 bis 63 Zeichen".into());
            }
            let body = format!("psk={}&ssid={}&ssid_index=&AuthenType={}", urlencode(psk), urlencode(ssid), if open { 2 } else { 0 });
            let cgi = if action == "wifi_join" { "/cgi-bin/set_wifi_POST.cgi" } else { "/cgi-bin/delete_password_POST.cgi" };
            post(host, cgi, &body)?;
            done
        }
        "wifi_status" => Ok(json!(wifi_info(host, &["check_wifi"])?)),

        // -- hotspot of the projector --
        "softap_psk" => {
            let psk = field(f, "psk");
            if !(8..=15).contains(&psk.len()) || !psk.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err("Hotspot-Passwort: 8 bis 15 Buchstaben oder Ziffern".into());
            }
            post(host, "/cgi-bin/am_websetting.cgi", &format!("type=set_softap_psk&time={}&data={psk}", millis()))?;
            done
        }

        // -- firmware --
        "ota_check" => {
            post(host, "/cgi-bin/amUpdateAppPOST.cgi", "type=requestOTAInfo")?;
            thread::sleep(Duration::from_secs(5));
            let r = get(host, "/cgi-bin/amUpdateAppPOST.cgi?type=requestOTACase")?;
            let info = r.lines().find_map(|l| serde_json::from_str::<Value>(l.trim()).ok()).ok_or("Keine Antwort vom Update-Server")?;
            Ok(info)
        }
        "ota_upgrade" => {
            post(host, "/cgi-bin/amUpdateAppPOST.cgi", "type=upgrade")?;
            done
        }
        "ota_progress" => {
            let q = |t: &str| get(host, &format!("/cgi-bin/am_websetting.cgi?timeStamp={}&type={t}", millis())).unwrap_or_default();
            Ok(json!({"state": q("get_ota_check_status"), "percent": q("get_ota_dwn_status")}))
        }

        // -- EZCast module system --
        "ez_reboot" => wifi_info(host, &["reboot"]).map(|_| Value::Null),
        "ez_reset" => wifi_info(host, &["reset_default"]).map(|_| Value::Null),

        // -- Acer network page --
        "lan" => {
            let body = if field(f, "dhcp") == "1" {
                "LanDhcp=1&Submit=Submit".to_string()
            } else {
                format!(
                    "LanDhcp=0&IP={}&MSK={}&GW={}&DNS={}&Submit=Submit",
                    ipv4(field(f, "ip"), "IP-Adresse")?,
                    ipv4(field(f, "mask"), "Netzmaske")?,
                    ipv4(field(f, "gateway"), "Gateway")?,
                    ipv4(field(f, "dns"), "DNS-Server")?
                )
            };
            acer_post(host, "/form/setting_cgi", &body).map_err(|_| "Der Beamer antwortet nicht".to_string())?;
            done
        }
        "crestron" => {
            let id = field(f, "id").parse::<u32>().map_err(|_| "IP-ID ist keine Zahl")?;
            let port = field(f, "port").parse::<u16>().map_err(|_| "Port ist keine Zahl")?;
            let body = format!("ctrsysip={}&ctrsysipid={id}&ctrsysport={port}&CresClient=Apply", ipv4(field(f, "ip"), "Crestron-Adresse")?);
            acer_post(host, "/form/setting_cgi", &body).map_err(|_| "Der Beamer antwortet nicht".to_string())?;
            done
        }
        "passwords" => {
            let mut body = String::new();
            for (role, en, p1, p2) in [("admin", "enadmin", "admpwd", "admpwd2"), ("user", "enuser", "usrpwd", "usrpwd2")] {
                let on = field(f, &format!("{role}_on")) == "1";
                body += &format!("{en}={}&", on as u8);
                if on {
                    let mut pw = safe(field(f, &format!("{role}_pw")), 8, "", "Passwort")?;
                    if pw.is_empty() {
                        pw = stored_secret(host, "network.asp", p1); // keep the current one
                    }
                    if pw.is_empty() {
                        return Err("Passwort fehlt".into());
                    }
                    body += &format!("{p1}={pw}&{p2}={pw}&");
                }
            }
            acer_post(host, "/form/setting_cgi", &(body + "passwdSubmit=Apply")).map_err(|_| "Der Beamer antwortet nicht".to_string())?;
            done
        }
        "email" => {
            let mail = "@.-_";
            let mut pw = field(f, "password").to_string();
            if pw.is_empty() {
                pw = stored_secret(host, "email.asp", "MAIN_PASWD"); // keep the current one
            }
            let pw = pw.as_str();
            let alerts: Vec<&str> = (0..4).map(|i| if f["alerts"][i] == true { "1" } else { "0" }).collect();
            let body = format!(
                "RECIVER={}&RECIVER2={}&SUBJECT={}&SENDER={}&MAIL_SERVER={}&MAIL_USER={}&MAIN_PASWD={}&alertcondit={}&Submit=Submit",
                safe(field(f, "to"), 34, mail, "An")?,
                safe(field(f, "cc"), 39, mail, "Kopie")?,
                safe(field(f, "subject"), 30, " -_.", "Betreff")?,
                safe(field(f, "from"), 39, mail, "Absender")?,
                safe(field(f, "server"), 30, ".-", "SMTP-Server")?,
                safe(field(f, "user"), 20, mail, "Benutzer")?,
                safe(pw, 14, "", "SMTP-Passwort")?,
                alerts.join(","),
            );
            http::request("POST", &format!("http://{host}/form/email_cgi"), body.as_bytes(), "application/x-www-form-urlencoded", T)
                .map_err(|_| "Der Beamer antwortet nicht".to_string())?;
            done
        }
        _ => Err("Unbekannter Befehl".into()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn conference_xml() {
        // as answered by the projector on 15.11.2026
        let x = "<ConnectInfo>   <SplitCount>1</SplitCount>   <Connection>     <HostName>(null)</HostName>     <Position>1</Position>     <IPAddress>10.1.34.178</IPAddress>     <Type>EZCast-stream</Type>     <Interface>eth0</Interface>     <Role>host</Role>     <Version>2</Version>   </Connection> </ConnectInfo>";
        let v = super::parse_conference(x);
        assert_eq!(v["split"], 1);
        assert_eq!(v["clients"][0]["ip"], "10.1.34.178");
        assert_eq!(v["clients"][0]["name"], "");
        assert_eq!(v["clients"][0]["role"], "host");
    }

    #[test]
    fn form_fields_without_passwords() {
        let html = r#"<input type="text" class="net_text" id="ipaddr" name="IP" value=10.1.34.179>
            <input type="radio" id="endhcp" value="1" name=LanDhcp checked>DHCP
            <input type="radio" id="manual" value="0" name=LanDhcp >Manual
            <input type="password" id="admpwd" name="admpwd" value=admin>
            <input type="checkbox" id="altc1" name="altc1" value="1" checked>Fan"#;
        let f = super::form_values(html);
        assert_eq!(f["IP"], "10.1.34.179");
        assert_eq!(f["LanDhcp"], "1");
        assert_eq!(f["altc1"], "1");
        assert!(!f.contains_key("admpwd"));
    }

    #[test]
    fn url_encoding() {
        assert_eq!(super::urlencode("a b&c\n"), "a+b%26c%0A");
    }
}
