//! State shared between the session, the projector output and the web panel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::audio::AudioSink;

pub fn log(text: impl AsRef<str>) {
    println!("{}", text.as_ref());
}

#[derive(Clone)]
pub struct SessionInfo {
    pub source: String,
    pub ip: String,
    pub playing: bool,
    pub started: Instant,
    pub frames: u64,
    pub video: String,
}

/// Another presenter (EZCast / eDisplay app) who asks the projector for the screen; the
/// projector asks us, the current host, to allow or deny.
#[derive(Clone)]
pub struct Guest {
    pub name: String,
    pub ip: String,
    pub fullscreen: bool,
}

pub struct Hub {
    guests: Mutex<Vec<Guest>>,
    frame: Mutex<(u64, Option<Arc<Vec<u8>>>)>,
    cond: Condvar,
    session: Mutex<Option<SessionInfo>>,
    /// set to end the running session (Win+K or screen sharing from the panel)
    session_stop: Mutex<Option<Arc<AtomicBool>>>,
    output: Mutex<String>,
    /// Set by the projector output when it needs a key frame; the running source (RTSP loop
    /// or the browser) is asked for one.
    pub idr_wanted: AtomicBool,
    /// sound of the running session
    pub audio: AudioSink,
}

impl Hub {
    pub fn new(audio_target: &str) -> Arc<Hub> {
        Arc::new(Hub {
            guests: Mutex::new(Vec::new()),
            frame: Mutex::new((0, None)),
            cond: Condvar::new(),
            session: Mutex::new(None),
            session_stop: Mutex::new(None),
            output: Mutex::new(String::new()),
            idr_wanted: AtomicBool::new(false),
            audio: AudioSink::new(audio_target),
        })
    }

    // -- preview frames (JPEG) for the panel ---------------------------------------------------
    pub fn publish(&self, jpeg: Vec<u8>) {
        let mut g = self.frame.lock().unwrap();
        g.0 += 1;
        g.1 = Some(Arc::new(jpeg));
        self.cond.notify_all();
        drop(g);
        self.update_session(|s| s.frames += 1);
    }

    pub fn clear_frame(&self) {
        let mut g = self.frame.lock().unwrap();
        g.0 += 1;
        g.1 = None;
        self.cond.notify_all();
    }

    pub fn wait_frame(&self, last: u64, timeout: Duration) -> (u64, Option<Arc<Vec<u8>>>) {
        let mut g = self.frame.lock().unwrap();
        if g.0 == last {
            g = self.cond.wait_timeout(g, timeout).unwrap().0;
        }
        (g.0, g.1.clone())
    }

    // -- session --------------------------------------------------------------------------------
    pub fn session(&self) -> Option<SessionInfo> {
        self.session.lock().unwrap().clone()
    }

    /// Claims the receiver for a new session; Err(name of the current source) if one is running.
    /// `stop` is set when somebody ends the session from the panel.
    pub fn begin_session(&self, info: SessionInfo, stop: Arc<AtomicBool>) -> Result<(), String> {
        let mut g = self.session.lock().unwrap();
        if let Some(s) = g.as_ref() {
            return Err(s.source.clone());
        }
        *g = Some(info);
        *self.session_stop.lock().unwrap() = Some(stop);
        self.idr_wanted.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub fn end_session(&self) {
        *self.session.lock().unwrap() = None;
        *self.session_stop.lock().unwrap() = None;
        self.clear_frame();
    }

    /// Asks the running session to end; false if none runs.
    pub fn stop_session(&self) -> bool {
        match self.session_stop.lock().unwrap().as_ref() {
            Some(stop) => {
                stop.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    pub fn update_session(&self, f: impl FnOnce(&mut SessionInfo)) {
        if let Some(s) = self.session.lock().unwrap().as_mut() {
            f(s);
        }
    }

    // -- projector output state, shown in the panel -----------------------------------------
    pub fn set_output(&self, text: impl Into<String>) {
        *self.output.lock().unwrap() = text.into();
    }

    pub fn output(&self) -> String {
        self.output.lock().unwrap().clone()
    }

    // -- presenters waiting for the projector -------------------------------------------------
    pub fn guest_asks(&self, guest: Guest) {
        let mut g = self.guests.lock().unwrap();
        g.retain(|x| x.ip != guest.ip);
        g.push(guest);
    }

    pub fn guest_gone(&self, ip: &str) {
        self.guests.lock().unwrap().retain(|x| x.ip != ip);
    }

    pub fn guests(&self) -> Vec<Guest> {
        self.guests.lock().unwrap().clone()
    }

    pub fn request_idr(&self) {
        self.idr_wanted.store(true, Ordering::Relaxed);
    }
}
