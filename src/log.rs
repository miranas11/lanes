//! A small append-only log at ~/Library/Logs/Lanes.log, so problems can be
//! diagnosed from what actually happened. Capped at 1 MB.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

thread_local! {
    static FILE: RefCell<Option<File>> = const { RefCell::new(None) };
}

pub fn path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Logs/Lanes.log")
}

pub fn init() {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(&p)
        .map(|m| m.len() > 1_000_000)
        .unwrap_or(false)
    {
        let _ = fs::rename(&p, p.with_extension("old.log"));
    }
    let f = OpenOptions::new().create(true).append(true).open(&p).ok();
    FILE.with_borrow_mut(|slot| *slot = f);
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
    FILE.with_borrow_mut(|f| {
        if let Some(f) = f {
            let _ = writeln!(f, "{h:02}:{m:02}:{s:06.3} {msg}");
        }
    });
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::write(&format!($($t)*)) };
}
