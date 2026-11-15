//! MPEG-TS demultiplexing (H.264 video and LPCM audio) and H.264 access-unit clean-up.

/// One PES payload out of the transport stream.
pub enum Es<'a> {
    /// H.264, one access unit in Wi-Fi Display
    Video(&'a [u8]),
    /// LPCM with its private header (stream type 0x83/0x80); PTS in microseconds
    Audio { data: &'a [u8], pts_us: Option<u64> },
}

#[derive(Default)]
struct Pes {
    buf: Vec<u8>,
    len: usize,
    active: bool,
}

impl Pes {
    fn start(&mut self, payload: &[u8]) {
        self.buf.clear();
        self.buf.extend_from_slice(payload);
        self.active = true;
        self.len = if payload.len() >= 6 { u16::from_be_bytes([payload[4], payload[5]]) as usize } else { 0 };
    }

    fn complete(&self) -> bool {
        self.active && self.len > 0 && self.buf.len() >= self.len + 6
    }

    /// Ends the packet: (payload range, PTS in microseconds).
    fn finish(&mut self) -> Option<(std::ops::Range<usize>, Option<u64>)> {
        if !self.active {
            return None;
        }
        self.active = false;
        let p = &self.buf;
        if p.len() < 9 || p[0..3] != [0, 0, 1] {
            return None;
        }
        let pts = (p[7] & 0x80 != 0 && p.len() >= 14).then(|| {
            let ticks = ((p[9] as u64 >> 1) & 7) << 30
                | (p[10] as u64) << 22
                | (p[11] as u64 >> 1) << 15
                | (p[12] as u64) << 7
                | p[13] as u64 >> 1;
            ticks * 100 / 9
        });
        let start = 9 + p[8] as usize;
        let end = if self.len > 0 { (self.len + 6).min(p.len()) } else { p.len() };
        (start < end).then_some((start..end, pts))
    }
}

/// Pulls the H.264 elementary stream and the LPCM audio out of an MPEG-TS stream, one PES
/// packet at a time.
#[derive(Default)]
pub struct TsDemux {
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    video: Pes,
    audio: Pes,
    rest: Vec<u8>,
}

impl TsDemux {
    pub fn feed(&mut self, data: &[u8], out: &mut dyn FnMut(Es)) {
        self.rest.extend_from_slice(data);
        let mut pos = 0;
        while self.rest.len() - pos >= 188 {
            if self.rest[pos] != 0x47 {
                pos += 1; // resync
                continue;
            }
            let packet: [u8; 188] = self.rest[pos..pos + 188].try_into().unwrap();
            self.packet(&packet, out);
            pos += 188;
        }
        self.rest.drain(..pos);
    }

    fn packet(&mut self, p: &[u8; 188], out: &mut dyn FnMut(Es)) {
        let pusi = p[1] & 0x40 != 0;
        let pid = ((p[1] as u16 & 0x1f) << 8) | p[2] as u16;
        let afc = (p[3] >> 4) & 3;
        if afc & 1 == 0 {
            return;
        }
        let mut off = 4;
        if afc == 3 {
            off += 1 + p[4] as usize;
        }
        if off >= 188 {
            return;
        }
        let payload = &p[off..];
        if pid == 0 {
            if pusi {
                self.parse_pat(payload);
            }
        } else if Some(pid) == self.pmt_pid {
            if pusi {
                self.parse_pmt(payload);
            }
        } else if Some(pid) == self.video_pid || Some(pid) == self.audio_pid {
            let video = Some(pid) == self.video_pid;
            let pes = if video { &mut self.video } else { &mut self.audio };
            if pusi {
                Self::emit(pes, video, out);
                pes.start(payload);
            } else if pes.active {
                pes.buf.extend_from_slice(payload);
            }
            if pes.complete() {
                Self::emit(pes, video, out);
            }
        }
    }

    fn emit(pes: &mut Pes, video: bool, out: &mut dyn FnMut(Es)) {
        if let Some((range, pts_us)) = pes.finish() {
            let data = &pes.buf[range];
            out(if video { Es::Video(data) } else { Es::Audio { data, pts_us } });
        }
    }

    fn section(payload: &[u8]) -> Option<&[u8]> {
        let pointer = *payload.first()? as usize;
        let sec = payload.get(1 + pointer..)?;
        if sec.len() < 3 {
            return None;
        }
        let len = (((sec[1] & 0x0f) as usize) << 8) | sec[2] as usize;
        sec.get(..(3 + len).min(sec.len()))
    }

    fn parse_pat(&mut self, payload: &[u8]) {
        let Some(sec) = Self::section(payload) else { return };
        let mut i = 8;
        while i + 4 <= sec.len().saturating_sub(4) {
            let program = u16::from_be_bytes([sec[i], sec[i + 1]]);
            let pid = u16::from_be_bytes([sec[i + 2], sec[i + 3]]) & 0x1fff;
            if program != 0 {
                if self.pmt_pid != Some(pid) {
                    self.pmt_pid = Some(pid);
                    self.video_pid = None;
                    self.audio_pid = None;
                }
                return;
            }
            i += 4;
        }
    }

    fn parse_pmt(&mut self, payload: &[u8]) {
        let Some(sec) = Self::section(payload) else { return };
        if sec.len() < 12 {
            return;
        }
        let info_len = (((sec[10] & 0x0f) as usize) << 8) | sec[11] as usize;
        let mut i = 12 + info_len;
        let end = sec.len().saturating_sub(4);
        while i + 5 <= end {
            let stream_type = sec[i];
            let pid = u16::from_be_bytes([sec[i + 1], sec[i + 2]]) & 0x1fff;
            let es_len = (((sec[i + 3] & 0x0f) as usize) << 8) | sec[i + 4] as usize;
            match stream_type {
                0x1b => self.video_pid = Some(pid),
                // LPCM: 0x83 in Wi-Fi Display, 0x80 (HDMV) in some sources
                0x83 | 0x80 => self.audio_pid = Some(pid),
                _ => {}
            }
            i += 5 + es_len;
        }
    }
}

/// NAL units of an Annex B byte stream, without start codes.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (n, &s) in starts.iter().enumerate() {
        let mut end = if n + 1 < starts.len() { starts[n + 1] - 3 } else { data.len() };
        while end > s && data[end - 1] == 0 {
            end -= 1; // zero byte of a 4-byte start code / trailing zeros
        }
        if end > s {
            units.push(&data[s..end]);
        }
    }
    units
}

/// Lowers level_idc in every SPS of an access unit to `level` (31 = 3.1). Browser encoders
/// (WebCodecs, hardware) write e.g. 4.1 although 3.1 was asked for; 720p30 fits 3.1, and Win+K
/// sources send 3.1, the level the projector is known to take. level_idc is the fourth byte of
/// the SPS NAL (header, profile_idc, constraint flags, level_idc), before any emulation
/// prevention can occur.
pub fn clamp_sps_level(au: &mut [u8], level: u8) {
    let mut i = 0;
    while i + 6 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if au[i + 3] & 0x1f == 7 && au[i + 6] > level {
                au[i + 6] = level;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
}

/// Rewrites access units the way the projector got them from the Acer software: 4-byte start
/// codes, no delimiter / filler NALs, SPS and PPS in front of every key frame.
#[derive(Default)]
pub struct AccessUnits {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

pub struct AccessUnit {
    pub data: Vec<u8>,
    pub key: bool,
}

impl AccessUnits {
    pub fn build(&mut self, es: &[u8]) -> Option<AccessUnit> {
        let nals = nal_units(es);
        let key = nals.iter().any(|n| n[0] & 0x1f == 5);
        let has_sps = nals.iter().any(|n| n[0] & 0x1f == 7);
        let has_pps = nals.iter().any(|n| n[0] & 0x1f == 8);
        let mut out = Vec::with_capacity(es.len() + 64);
        if key && !(has_sps && has_pps) {
            if let (Some(sps), Some(pps)) = (&self.sps, &self.pps) {
                for nal in [sps, pps] {
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(nal);
                }
            }
        }
        for nal in &nals {
            match nal[0] & 0x1f {
                9 | 12 => continue, // access unit delimiter, filler
                7 => self.sps = Some(nal.to_vec()),
                8 => self.pps = Some(nal.to_vec()),
                _ => {}
            }
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        if nals.iter().any(|n| matches!(n[0] & 0x1f, 1 | 5)) { Some(AccessUnit { data: out, key }) } else { None }
    }
}

/// Splits a continuous H.264 byte stream (with access unit delimiters, as ffmpeg writes it
/// with aud=1) into access units.
#[derive(Default)]
pub struct AudSplitter {
    buf: Vec<u8>,
}

impl AudSplitter {
    pub fn feed(&mut self, data: &[u8], out: &mut dyn FnMut(&[u8])) {
        self.buf.extend_from_slice(data);
        const MARK: [u8; 5] = [0, 0, 0, 1, 9];
        let mut start = 0;
        loop {
            let from = start + 5;
            let Some(rel) = self.buf.get(from..).and_then(|b| b.windows(5).position(|w| w == MARK)) else { break };
            let end = from + rel;
            out(&self.buf[start..end]);
            start = end;
        }
        self.buf.drain(..start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_unit_gets_parameter_sets() {
        let mut au = AccessUnits::default();
        let first = au.build(b"\x00\x00\x00\x01\x09\xf0\x00\x00\x00\x01\x67\x42\x00\x00\x00\x01\x68\xce\x00\x00\x01\x65\x88").unwrap();
        assert!(first.key);
        assert_eq!(first.data, b"\x00\x00\x00\x01\x67\x42\x00\x00\x00\x01\x68\xce\x00\x00\x00\x01\x65\x88");
        let p = au.build(b"\x00\x00\x00\x01\x41\x9a").unwrap();
        assert!(!p.key);
        let later = au.build(b"\x00\x00\x01\x65\x99").unwrap();
        assert_eq!(later.data, b"\x00\x00\x00\x01\x67\x42\x00\x00\x00\x01\x68\xce\x00\x00\x00\x01\x65\x99");
    }

    #[test]
    fn demux_finds_video() {
        // PAT -> PMT pid 0x100, PMT -> H.264 on pid 0x1011, one PES with a small payload
        let mut ts = Vec::new();
        let mut pkt = |pid: u16, pusi: bool, body: &[u8]| {
            let mut p = vec![0x47, ((pusi as u8) << 6) | (pid >> 8) as u8, pid as u8, 0x10];
            p.extend_from_slice(body);
            p.resize(188, 0xff);
            ts.extend_from_slice(&p);
        };
        pkt(0, true, &[0, 0x00, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0x00, 0, 0, 0, 0]);
        pkt(0x100, true, &[0, 0x02, 0xb0, 18, 0, 1, 0xc1, 0, 0, 0xf0, 0x11, 0xf0, 0, 0x1b, 0xf0, 0x11, 0xf0, 0, 0, 0, 0, 0]);
        let es = b"\x00\x00\x00\x01\x65\x88\x84";
        let mut pes = vec![0, 0, 1, 0xe0, 0, (3 + es.len()) as u8, 0x80, 0, 0];
        pes.extend_from_slice(es);
        pkt(0x1011, true, &pes);
        let mut got = Vec::new();
        TsDemux::default().feed(&ts, &mut |d| if let Es::Video(v) = d { got.push(v.to_vec()) });
        assert_eq!(got, vec![es.to_vec()]);
    }

    #[test]
    fn demux_finds_lpcm_with_pts() {
        // PMT with H.264 on 0x1011 and LPCM (0x83) on 0x1100; one audio PES with PTS 90000 (1 s)
        let mut ts = Vec::new();
        let mut pkt = |pid: u16, pusi: bool, body: &[u8]| {
            let mut p = vec![0x47, ((pusi as u8) << 6) | (pid >> 8) as u8, pid as u8, 0x10];
            p.extend_from_slice(body);
            p.resize(188, 0xff);
            ts.extend_from_slice(&p);
        };
        pkt(0, true, &[0, 0x00, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xe1, 0x00, 0, 0, 0, 0]);
        pkt(0x100, true, &[0, 0x02, 0xb0, 23, 0, 1, 0xc1, 0, 0, 0xf0, 0x11, 0xf0, 0,
                           0x1b, 0xf0, 0x11, 0xf0, 0, 0x83, 0xf1, 0x00, 0xf0, 0, 0, 0, 0, 0]);
        let lpcm = [0xa0, 0x06, 0x00, 0x00, 0x00, 0x11, 0x80, 0x12, 0x34, 0x56, 0x78];
        let t: u64 = 90000; // PTS in 90 kHz ticks, coded with marker bits as in a real PES header
        let pts = [0x21 | (((t >> 30) & 7) << 1) as u8, (t >> 22) as u8, (((t >> 15) & 0x7f) << 1) as u8 | 1,
                   (t >> 7) as u8, ((t & 0x7f) << 1) as u8 | 1];
        let mut pes = vec![0, 0, 1, 0xbd, 0, (8 + lpcm.len()) as u8, 0x80, 0x80, 5];
        pes.extend_from_slice(&pts);
        pes.extend_from_slice(&lpcm);
        pkt(0x1100, true, &pes);
        let mut got = Vec::new();
        TsDemux::default().feed(&ts, &mut |d| if let Es::Audio { data, pts_us } = d { got.push((data.to_vec(), pts_us)) });
        assert_eq!(got, vec![(lpcm.to_vec(), Some(1_000_000))]);
    }

    #[test]
    fn sps_level_lowered() {
        // as encoded by Chromium's hardware encoder: Constrained Baseline, level 4.1
        let mut au = b"\x00\x00\x00\x01\x09\x10\x00\x00\x00\x01\x67\x42\x40\x29\x95\xb8\x00\x00\x01\x65\x88\x29".to_vec();
        clamp_sps_level(&mut au, 31);
        assert_eq!(&au[10..16], b"\x67\x42\x40\x1f\x95\xb8");
        assert_eq!(au[au.len() - 1], 0x29); // slice data untouched
    }

    #[test]
    fn aud_splitter() {
        let mut s = AudSplitter::default();
        let mut got = Vec::new();
        s.feed(b"\x00\x00\x00\x01\x09\xf0\x00\x00\x00\x01\x65\x01\x00\x00\x00\x01\x09\xf0\x00\x00", &mut |u| got.push(u.to_vec()));
        s.feed(b"\x00\x01\x41\x02\x00\x00\x00\x01\x09\xf0", &mut |u| got.push(u.to_vec()));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], b"\x00\x00\x00\x01\x09\xf0\x00\x00\x00\x01\x65\x01");
    }
}
