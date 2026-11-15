//! State shared between the session, the projector output and the web panel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

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

pub struct Hub {
    frame: Mutex<(u64, Option<Arc<Vec<u8>>>)>,
    cond: Condvar,
    session: Mutex<Option<SessionInfo>>,
    output: Mutex<String>,
    /// Set by the projector output when it needs a key frame; the RTSP loop asks the source.
    pub idr_wanted: AtomicBool,
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        Arc::new(Hub {
            frame: Mutex::new((0, None)),
            cond: Condvar::new(),
            session: Mutex::new(None),
            output: Mutex::new(String::new()),
            idr_wanted: AtomicBool::new(false),
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

    /// Claims the receiver for a new session; false if one is already running.
    pub fn begin_session(&self, info: SessionInfo) -> Result<(), String> {
        let mut g = self.session.lock().unwrap();
        if let Some(s) = g.as_ref() {
            return Err(s.source.clone());
        }
        *g = Some(info);
        Ok(())
    }

    pub fn end_session(&self) {
        *self.session.lock().unwrap() = None;
        self.clear_frame();
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

    pub fn request_idr(&self) {
        self.idr_wanted.store(true, Ordering::Relaxed);
    }
}
