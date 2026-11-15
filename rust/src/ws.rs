//! Minimal WebSocket server side (RFC 6455) for the panel's screen sharing: handshake, reading
//! masked client frames, writing unmasked frames. Pure Rust (own SHA-1), no extra crates.

use std::io::{self, Read, Write};

use base64::Engine;

/// SHA-1 (FIPS 180-1); only used for the handshake.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

/// Value of Sec-WebSocket-Accept for the client's Sec-WebSocket-Key.
pub fn accept_key(key: &str) -> String {
    let digest = sha1(format!("{}258EAFA5-E914-47DA-95CA-C5AB0DC85B11", key.trim()).as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

pub fn handshake(stream: &mut impl Write, key: &str) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(key)
    );
    stream.write_all(head.as_bytes())
}

pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong,
    Close,
}

/// Collects frames from a stream that may time out between reads (partial data is kept).
pub struct Reader {
    buf: Vec<u8>,
    fragments: Vec<u8>,
    fragment_text: bool,
    max: usize,
}

impl Reader {
    pub fn new(max: usize) -> Reader {
        Reader { buf: Vec::new(), fragments: Vec::new(), fragment_text: false, max }
    }

    /// Next message; Ok(None) when the read timed out without a whole message.
    pub fn read(&mut self, stream: &mut impl Read) -> io::Result<Option<Message>> {
        loop {
            if let Some(m) = self.parse()? {
                return Ok(Some(m));
            }
            let mut chunk = [0u8; 65536];
            match stream.read(&mut chunk) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed")),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(None),
                Err(e) => return Err(e),
            }
        }
    }

    fn parse(&mut self) -> io::Result<Option<Message>> {
        loop {
            let b = &self.buf;
            if b.len() < 2 {
                return Ok(None);
            }
            let fin = b[0] & 0x80 != 0;
            let opcode = b[0] & 0x0f;
            let masked = b[1] & 0x80 != 0;
            let (len, mut pos) = match b[1] & 0x7f {
                126 if b.len() >= 4 => (u16::from_be_bytes([b[2], b[3]]) as usize, 4),
                127 if b.len() >= 10 => (u64::from_be_bytes(b[2..10].try_into().unwrap()) as usize, 10),
                126 | 127 => return Ok(None),
                n => (n as usize, 2),
            };
            if len > self.max {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
            }
            if !masked {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "client frame not masked"));
            }
            if b.len() < pos + 4 + len {
                return Ok(None);
            }
            let mask = [b[pos], b[pos + 1], b[pos + 2], b[pos + 3]];
            pos += 4;
            let mut payload: Vec<u8> = b[pos..pos + len].to_vec();
            for (i, x) in payload.iter_mut().enumerate() {
                *x ^= mask[i % 4];
            }
            self.buf.drain(..pos + len);
            match opcode {
                0x0 => {
                    if self.fragments.len() + payload.len() > self.max {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
                    }
                    self.fragments.extend_from_slice(&payload);
                    if fin {
                        let data = std::mem::take(&mut self.fragments);
                        return Ok(Some(self.whole(self.fragment_text, data)));
                    }
                }
                0x1 | 0x2 => {
                    if fin {
                        return Ok(Some(self.whole(opcode == 0x1, payload)));
                    }
                    self.fragment_text = opcode == 0x1;
                    self.fragments = payload;
                }
                0x8 => return Ok(Some(Message::Close)),
                0x9 => return Ok(Some(Message::Ping(payload))),
                0xA => return Ok(Some(Message::Pong)),
                _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown opcode")),
            }
        }
    }

    fn whole(&self, text: bool, data: Vec<u8>) -> Message {
        if text { Message::Text(String::from_utf8_lossy(&data).into_owned()) } else { Message::Binary(data) }
    }
}

/// One unmasked frame from the server.
pub fn write(stream: &mut impl Write, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let mut head = vec![0x80 | opcode];
    match payload.len() {
        n if n < 126 => head.push(n as u8),
        n if n < 65536 => {
            head.push(126);
            head.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            head.push(127);
            head.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    head.extend_from_slice(payload);
    stream.write_all(&head)
}

pub fn text(stream: &mut impl Write, s: &str) -> io::Result<()> {
    write(stream, 0x1, s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_vectors() {
        let hex = |d: [u8; 20]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(hex(sha1(long)), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
    }

    #[test]
    fn handshake_example_from_rfc() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn reads_masked_and_fragmented_frames() {
        // "Hel" + "lo" as text fragments, masked with 37 fa 21 3d (RFC 6455 5.7)
        let data: Vec<u8> = vec![
            0x01, 0x83, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, // "Hel"
            0x80, 0x82, 0x37, 0xfa, 0x21, 0x3d, 0x5b, 0x95, // "lo"
        ];
        let mut r = Reader::new(1024);
        let mut src = &data[..];
        match r.read(&mut src).unwrap() {
            Some(Message::Text(t)) => assert_eq!(t, "Hello"),
            _ => panic!("expected text"),
        }
    }
}
