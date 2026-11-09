//! A small append-only log at ~/Library/Logs/Lanes.log, so problems can be
//! diagnosed from what actually happened. Capped at 1 MB (plus one old copy).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Shared by every thread (the input thread and the window movers log too).
/// The number is how many bytes the file holds, to keep it under the cap.
static FILE: Mutex<Option<(File, u64)>> = Mutex::new(None);

const CAP: u64 = 1_000_000;

pub fn path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Logs/Lanes.log")
}

/// Opens the log, first moving a full one aside (one old copy is kept).
fn open() -> Option<(File, u64)> {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(&p).map(|m| m.len() > CAP).unwrap_or(false) {
        let _ = fs::rename(&p, p.with_extension("old.log"));
    }
    let f = OpenOptions::new().create(true).append(true).open(&p).ok()?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    Some((f, len))
}

pub fn init() {
    let f = open();
    if let Ok(mut slot) = FILE.lock() {
        *slot = f;
    }
}

pub fn write(msg: &str) {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    // Seconds within the day, UTC, are enough to line events up.
    let secs = t % 86_400.0;
    let (h, m, s) = (
        (secs / 3600.0) as u32,
        ((secs % 3600.0) / 60.0) as u32,
        secs % 60.0,
    );
    let line = format!("{h:02}:{m:02}:{s:06.3} {msg}\n");
    let Ok(mut slot) = FILE.lock() else { return };
    // Lanes runs for weeks: start a fresh file whenever this one is full.
    if slot.as_ref().is_some_and(|(_, len)| *len > CAP) {
        *slot = None;
        *slot = open();
    }
    if let Some((f, len)) = slot.as_mut() {
        let _ = f.write_all(line.as_bytes());
        *len += line.len() as u64;
    }
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::write(&format!($($t)*)) };
}
