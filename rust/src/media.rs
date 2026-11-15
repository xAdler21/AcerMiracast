//! Turns the received MPEG-TS into what the outputs need:
//!   - H.264 access units for the projector, either passed straight through (the source was
//!     asked for exactly the format the projector takes) or re-encoded by ffmpeg
//!   - JPEG preview frames for the web panel (ffmpeg, low frame rate)
//! Writes to ffmpeg go through bounded queues, so a slow ffmpeg never stalls the receiver.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::ezdisplay::Output;
use crate::hub::{Hub, log};
use crate::ts::{AccessUnits, AudSplitter, TsDemux};

pub struct VideoSize {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Queue in front of a child's stdin; drops data instead of blocking when the child is slow.
struct Feeder {
    tx: Option<SyncSender<Vec<u8>>>,
}

impl Feeder {
    fn new(mut stdin: ChildStdin) -> Feeder {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(512);
        thread::spawn(move || {
            for chunk in rx {
                if stdin.write_all(&chunk).is_err() {
                    break;
                }
            }
        });
        Feeder { tx: Some(tx) }
    }

    fn feed(&self, data: &[u8]) {
        if let Some(tx) = &self.tx {
            if let Err(TrySendError::Full(_)) = tx.try_send(data.to_vec()) {
                // dropped; the decoder resynchronises
            }
        }
    }
}

fn ffmpeg_input() -> Vec<String> {
    [
        "-hide_banner", "-loglevel", "error", "-nostdin", "-fflags", "nobuffer", "-flags", "low_delay",
        "-probesize", "200000", "-analyzeduration", "500000", "-f", "mpegts", "-i", "pipe:0", "-an",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn spawn(args: Vec<String>) -> Option<Child> {
    match Command::new("ffmpeg").args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn() {
        Ok(c) => Some(c),
        Err(e) => {
            log(format!("ffmpeg could not be started: {e}"));
            None
        }
    }
}

pub struct Pipeline {
    demux: TsDemux,
    units: AccessUnits,
    output: Option<Output>,
    passthrough: bool,
    preview: Option<(Child, Feeder)>,
    transcoder: Option<(Child, Feeder)>,
    pub frames: Arc<AtomicU64>,
}

impl Pipeline {
    pub fn new(hub: Arc<Hub>, output: Option<Output>, passthrough: bool, size: &VideoSize, preview: Option<(u32, u32)>) -> Pipeline {
        let frames = Arc::new(AtomicU64::new(0));
        let mut p = Pipeline {
            demux: TsDemux::default(),
            units: AccessUnits::default(),
            output: output.clone(),
            passthrough,
            preview: None,
            transcoder: None,
            frames: frames.clone(),
        };
        if let Some((fps, width)) = preview {
            let mut args = ffmpeg_input();
            args.extend(
                ["-vf", &format!("fps={fps},scale={width}:-2,format=yuvj420p"), "-q:v", "6", "-f", "mjpeg", "pipe:1"]
                    .iter()
                    .map(|s| s.to_string()),
            );
            if let Some(mut child) = spawn(args) {
                let stdout = child.stdout.take().unwrap();
                let feeder = Feeder::new(child.stdin.take().unwrap());
                let hub = hub.clone();
                thread::spawn(move || read_jpegs(stdout, hub));
                p.preview = Some((child, feeder));
            }
        }
        if let (Some(out), false) = (output, passthrough) {
            let mut args = ffmpeg_input();
            let (w, h, fps) = (size.width, size.height, size.fps);
            args.extend(
                [
                    "-vf",
                    &format!("fps={fps},scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,format=yuv420p"),
                    "-c:v", "libx264", "-profile:v", "baseline", "-level", "3.1", "-preset", "ultrafast",
                    "-tune", "zerolatency", "-g", &fps.to_string(), "-bf", "0", "-b:v", "3M", "-maxrate", "4M",
                    "-bufsize", "2M", "-x264-params", "aud=1:repeat-headers=1", "-f", "h264", "pipe:1",
                ]
                .iter()
                .map(|s| s.to_string()),
            );
            if let Some(mut child) = spawn(args) {
                let mut stdout = child.stdout.take().unwrap();
                let feeder = Feeder::new(child.stdin.take().unwrap());
                thread::spawn(move || {
                    let mut split = AudSplitter::default();
                    let mut units = AccessUnits::default();
                    let mut buf = vec![0u8; 65536];
                    while let Ok(n) = stdout.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        split.feed(&buf[..n], &mut |au| {
                            if let Some(unit) = units.build(au) {
                                out.send_video(unit.data, unit.key);
                            }
                        });
                    }
                });
                p.transcoder = Some((child, feeder));
            }
        }
        p
    }

    /// One RTP payload (whole TS packets).
    pub fn feed(&mut self, ts: &[u8]) {
        if let Some((_, f)) = &self.preview {
            f.feed(ts);
        }
        if let Some((_, f)) = &self.transcoder {
            f.feed(ts);
        }
        let (units, frames, output, passthrough) = (&mut self.units, &self.frames, &self.output, self.passthrough);
        self.demux.feed(ts, &mut |es| {
            frames.fetch_add(1, Ordering::Relaxed);
            if passthrough {
                if let (Some(out), Some(unit)) = (output, units.build(es)) {
                    out.send_video(unit.data, unit.key);
                }
            }
        });
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        for slot in [&mut self.preview, &mut self.transcoder] {
            if let Some((mut child, mut feeder)) = slot.take() {
                feeder.tx = None; // closes stdin
                thread::sleep(std::time::Duration::from_millis(100));
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if let Some(out) = &self.output {
            out.session_ended();
        }
    }
}

fn read_jpegs(mut stdout: impl Read, hub: Arc<Hub>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 65536];
    while let Ok(n) = stdout.read(&mut chunk) {
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        loop {
            let Some(start) = buf.windows(2).position(|w| w == [0xff, 0xd8]) else {
                buf.clear();
                break;
            };
            let Some(end) = buf[start + 2..].windows(2).position(|w| w == [0xff, 0xd9]) else {
                buf.drain(..start);
                break;
            };
            let end = start + 2 + end + 2;
            hub.publish(buf[start..end].to_vec());
            buf.drain(..end);
        }
    }
}
