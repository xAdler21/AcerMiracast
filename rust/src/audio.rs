//! Sound of the running session, from Win+K (LPCM in the MPEG-TS) or from the panel's screen
//! sharing (PCM from the browser). Everything ends in one sink: it measures the level for the
//! panel and hands the samples to the chosen target. For now the only target is "none"
//! (dropped); new targets (RTP into the network, ALSA, the DSP, the projector) implement
//! `AudioTarget` and are added to `TARGETS`.

use std::sync::Mutex;
use std::time::Instant;

use serde_json::{Value, json};

/// Where the sound goes. Samples are interleaved signed 16 bit; `pts_us` is the presentation
/// time of the first sample in the source's clock (the same clock as its video), if known.
pub trait AudioTarget: Send {
    fn push(&mut self, pts_us: Option<u64>, rate: u32, channels: u16, samples: &[i16]);
}

/// The sound is dropped.
struct Discard;

impl AudioTarget for Discard {
    fn push(&mut self, _: Option<u64>, _: u32, _: u16, _: &[i16]) {}
}

/// (name, label) of the targets that exist.
pub const TARGETS: [(&str, &str); 1] = [("none", "Nirgends (Ton wird verworfen)")];

fn make_target(name: &str) -> Option<Box<dyn AudioTarget>> {
    match name {
        "none" => Some(Box::new(Discard)),
        _ => None,
    }
}

struct Inner {
    target_name: String,
    target: Box<dyn AudioTarget>,
    source: String,
    rate: u32,
    channels: u16,
    /// peak per channel (0..1), falling 20 dB per second
    peak: [f32; 2],
    peak_at: Instant,
    last: Option<Instant>,
}

pub struct AudioSink {
    inner: Mutex<Inner>,
}

impl AudioSink {
    pub fn new(target: &str) -> AudioSink {
        let (target_name, target) = match make_target(target) {
            Some(t) => (target.to_string(), t),
            None => ("none".to_string(), make_target("none").unwrap()),
        };
        AudioSink {
            inner: Mutex::new(Inner {
                target_name, target, source: String::new(), rate: 0, channels: 0,
                peak: [0.0; 2], peak_at: Instant::now(), last: None,
            }),
        }
    }

    pub fn set_target(&self, name: &str) -> Result<(), String> {
        let t = make_target(name).ok_or("Unbekanntes Ton-Ziel")?;
        let mut g = self.inner.lock().unwrap();
        g.target = t;
        g.target_name = name.to_string();
        Ok(())
    }

    pub fn push(&self, source: &str, pts_us: Option<u64>, rate: u32, channels: u16, samples: &[i16]) {
        if channels == 0 || samples.is_empty() {
            return;
        }
        let mut block = [0f32; 2];
        for (i, s) in samples.iter().enumerate() {
            let ch = (i % channels as usize).min(1);
            block[ch] = block[ch].max((*s as f32).abs() / 32768.0);
        }
        if channels == 1 {
            block[1] = block[0];
        }
        let mut g = self.inner.lock().unwrap();
        let now = Instant::now();
        let fall = 10f32.powf(-g.peak_at.elapsed().as_secs_f32()); // -20 dB/s
        for (p, b) in g.peak.iter_mut().zip(block) {
            *p = (*p * fall).max(b);
        }
        g.peak_at = now;
        g.last = Some(now);
        if g.source != source {
            g.source = source.to_string();
        }
        g.rate = rate;
        g.channels = channels;
        g.target.push(pts_us, rate, channels, samples);
    }

    /// Level and state for the panel: peak in dBFS per channel (-60 = silence).
    pub fn status(&self) -> Value {
        let g = self.inner.lock().unwrap();
        let active = g.last.is_some_and(|t| t.elapsed().as_secs_f32() < 1.5);
        let fall = 10f32.powf(-g.peak_at.elapsed().as_secs_f32());
        let db = |p: f32| if active { (20.0 * (p * fall).max(1e-3).log10()).max(-60.0).round() } else { -60.0 };
        let targets: Vec<Value> = TARGETS.iter().map(|(n, l)| json!({"name": n, "label": l})).collect();
        json!({"active": active, "source": if active { g.source.as_str() } else { "" }, "rate": g.rate,
               "channels": g.channels, "peak_db": [db(g.peak[0]), db(g.peak[1])],
               "target": g.target_name, "targets": targets})
    }
}

/// LPCM of a Wi-Fi Display source (MPEG-TS stream type 0x83, private stream 1): a short private
/// header, then 16-bit big-endian samples, 48 kHz stereo as we asked for ("LPCM 00000002 00").
/// The header is 4 bytes (Blu-ray style) or 7 bytes (DVD style, sub_stream_id 0xa0); only one
/// of the two leaves whole stereo frames. Returns (header length, samples).
pub fn wfd_lpcm(payload: &[u8]) -> Option<(usize, Vec<i16>)> {
    let header = [4usize, 7].into_iter().find(|h| payload.len() > *h && (payload.len() - h) % 4 == 0)?;
    let samples = payload[header..].chunks_exact(2).map(|b| i16::from_be_bytes([b[0], b[1]])).collect();
    Some((header, samples))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lpcm_header_and_samples() {
        // 4-byte header, two stereo frames
        let p = [0x07, 0x80, 0x31, 0x40, 0x7f, 0xff, 0x80, 0x00, 0x00, 0x01, 0xff, 0xff];
        let (h, s) = wfd_lpcm(&p).unwrap();
        assert_eq!(h, 4);
        assert_eq!(s, vec![32767, -32768, 1, -1]);
        // 7-byte header (DVD style), one stereo frame
        let p = [0xa0, 0x06, 0x00, 0x00, 0x00, 0x11, 0x80, 0x00, 0x10, 0x00, 0x20];
        assert_eq!(wfd_lpcm(&p).unwrap(), (7, vec![16, 32]));
    }

    #[test]
    fn level_follows_the_samples() {
        let sink = AudioSink::new("none");
        sink.push("test", Some(0), 48000, 2, &[16384, 0, -16384, 0]);
        let s = sink.status();
        assert_eq!(s["active"], true);
        assert_eq!(s["peak_db"][0], -6.0);
        assert_eq!(s["peak_db"][1], -60.0);
        assert_eq!(s["target"], "none");
    }
}
