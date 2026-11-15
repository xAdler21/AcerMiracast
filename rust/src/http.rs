//! Minimal HTTP/1.0 client (projector web interface, beacon box).

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

fn invalid(text: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, text.to_string())
}

/// Returns (status, body).
pub fn request(method: &str, url: &str, body: &[u8], content_type: &str, timeout: Duration) -> io::Result<(u16, Vec<u8>)> {
    let rest = url.strip_prefix("http://").ok_or_else(|| invalid("only http:// is supported"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let address = if authority.contains(':') { authority.to_string() } else { format!("{authority}:80") };
    let target = address.to_socket_addrs()?.next().ok_or_else(|| invalid("no address"))?;
    let mut stream = TcpStream::connect_timeout(&target, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut head = format!("{method} {path} HTTP/1.0\r\nHost: {authority}\r\nConnection: close\r\n");
    if method == "POST" {
        head += &format!("Content-Type: {content_type}\r\nContent-Length: {}\r\n", body.len());
    }
    head += "\r\n";
    let mut data = head.into_bytes();
    data.extend_from_slice(body);
    stream.write_all(&data)?;
    let mut reply = Vec::new();
    stream.take(256 * 1024).read_to_end(&mut reply)?;
    let (head_end, body_start) = header_end(&reply).ok_or_else(|| invalid("incomplete answer"))?;
    let status_line = String::from_utf8_lossy(&reply[..head_end]).lines().next().unwrap_or("").to_string();
    let status = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| invalid("bad status"))?;
    Ok((status, reply[body_start..].to_vec()))
}

/// End of the header block. The projector's GoAhead server mixes "\n" and "\r\n" line ends,
/// so an empty line is "\n\n" or "\n\r\n".
fn header_end(reply: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < reply.len() {
        if reply[i] == b'\n' {
            if reply.get(i + 1) == Some(&b'\n') {
                return Some((i, i + 2));
            }
            if reply.get(i + 1) == Some(&b'\r') && reply.get(i + 2) == Some(&b'\n') {
                return Some((i, i + 3));
            }
        }
        i += 1;
    }
    None
}

/// POST to the Acer projector's web interface. The answer looks like
/// `<html>\n{pwr:"1",hid:"0",src:"6"}` (keys unquoted); returns {key: value}.
pub fn acer_post(host: &str, path: &str, body: &str) -> io::Result<HashMap<String, String>> {
    let (_status, reply) = request(
        "POST",
        &format!("http://{host}{path}"),
        body.as_bytes(),
        "application/x-www-form-urlencoded",
        Duration::from_secs(4),
    )?;
    Ok(parse_acer(&String::from_utf8_lossy(&reply)))
}

pub fn parse_acer(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // key: word characters followed by :"
        if b[i].is_ascii_alphanumeric() || b[i] == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            if b.get(i) == Some(&b':') && b.get(i + 1) == Some(&b'"') {
                let vstart = i + 2;
                if let Some(len) = b[vstart..].iter().position(|&c| c == b'"') {
                    out.insert(text[start..i].to_string(), text[vstart..vstart + len].to_string());
                    i = vstart + len + 1;
                    continue;
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn acer_answer() {
        let m = super::parse_acer("<html>\n{pwr:\"1\",hid:\"0\",src:\"6\"}");
        assert_eq!(m["pwr"], "1");
        assert_eq!(m["src"], "6");
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn goahead_header_end() {
        // as read from the projector on 15.11.2026
        let r = b"HTTP/1.0 200 OK\nServer: GoAhead-Webs\r\nPragma: no-cache\nContent-Type: text/html\n\n<html>\n{a:\"1\"}";
        let (_, body) = super::header_end(r).unwrap();
        assert_eq!(&r[body..], b"<html>\n{a:\"1\"}");
        let r = b"HTTP/1.1 200 OK\r\nA: b\r\n\r\nX";
        assert_eq!(&r[super::header_end(r).unwrap().1..], b"X");
    }
}
