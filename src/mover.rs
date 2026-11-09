//! Moves windows off the main thread, one worker per app.
//!
//! Every window move is a round trip into the owning app (1–2 ms, sometimes
//! 10+ ms when the app is busy). Done one after another on the main thread,
//! a scroll frame costs the sum of all of them and the row stutters. Here each
//! app has its own worker, so a frame costs as much as the slowest app, the
//! windows move together, and the main thread never waits.
//!
//! Only the newest position of each window is kept: if an app falls behind,
//! it skips straight to where the row is now instead of replaying the past.
//! Work that must happen in order with these moves (resizing) runs on the
//! caller's thread under the same per-app lock, after dropping any queued
//! move for that window.
//!
//! Making an app active is also slow (20–70 ms while the app wakes up), so
//! it goes through the same worker, after that app's pending moves.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use objc2_core_foundation::CGPoint;

use crate::ax::{self, Elem};

/// AX elements are plain CF objects that may be used from any thread.
struct SendElem(Elem);
unsafe impl Send for SendElem {}

#[derive(Default)]
struct Pending {
    /// window id → (element, where it goes), newest wins
    moves: HashMap<u32, (SendElem, CGPoint)>,
    /// Bring this (app, window) to the front; newest wins.
    activate: Option<(SendElem, SendElem)>,
    quit: bool,
}

struct Shared {
    pending: Mutex<Pending>,
    wake: Condvar,
    /// Held while talking to the app, by the worker or by `exclusive`.
    busy: Mutex<()>,
}

/// Owns an app's worker; dropping it stops the worker.
pub struct Mover {
    shared: Arc<Shared>,
}

/// What a worker reports back when it couldn't do something itself.
pub type Fallback = fn(i32);

/// A handle to an app's worker, cheap to clone.
#[derive(Clone)]
pub struct Lane(Arc<Shared>);

impl Mover {
    /// `fallback(pid)` runs (from the worker) when Accessibility can't make the
    /// app active, so the main thread can ask AppKit instead.
    pub fn new(pid: i32, app: &Elem, fallback: Fallback) -> Option<Self> {
        let shared = Arc::new(Shared {
            pending: Mutex::new(Pending::default()),
            wake: Condvar::new(),
            busy: Mutex::new(()),
        });
        let s = shared.clone();
        let app = SendElem(app.clone());
        std::thread::Builder::new()
            .name(format!("lanes-mover-{pid}"))
            .spawn(move || run(&s, pid, &app, fallback))
            .ok()?;
        Some(Self { shared })
    }

    pub fn lane(&self) -> Lane {
        Lane(self.shared.clone())
    }
}

impl Lane {
    /// Queue a move; replaces any not-yet-applied move of the same window.
    pub fn move_to(&self, id: u32, el: &Elem, p: CGPoint) {
        if let Ok(mut q) = self.0.pending.lock() {
            q.moves.insert(id, (SendElem(el.clone()), p));
        }
        self.0.wake.notify_one();
    }

    /// Queue making the app active with `win` as its main window.
    pub fn activate(&self, app: &Elem, win: &Elem) {
        if let Ok(mut q) = self.0.pending.lock() {
            q.activate = Some((SendElem(app.clone()), SendElem(win.clone())));
        }
        self.0.wake.notify_one();
    }

    /// Drop a queued move and wait until the app isn't being talked to, so the
    /// caller can act on the window without a stale move landing afterwards.
    /// Keep the returned guard for as long as the caller is acting.
    pub fn exclusive(&self, id: Option<u32>) -> Option<MutexGuard<'_, ()>> {
        if let Ok(mut q) = self.0.pending.lock() {
            match id {
                Some(id) => {
                    q.moves.remove(&id);
                }
                None => {
                    q.moves.clear();
                    q.activate = None;
                }
            }
        }
        self.0.busy.lock().ok()
    }
}

impl Drop for Mover {
    fn drop(&mut self) {
        if let Ok(mut q) = self.shared.pending.lock() {
            q.quit = true;
        }
        self.shared.wake.notify_one();
    }
}

fn run(s: &Shared, pid: i32, app: &SendElem, fallback: Fallback) {
    // Apps like Chrome turn on an accessibility mode that animates every move
    // (windows slide across the screen), so it is switched off around moves.
    // The flag is read here, never while a move is in flight: read at the
    // wrong moment it looks off, and the next moves would be animated.
    let mut enhanced = false;
    let mut checked: Option<Instant> = None;
    loop {
        // Sleep until there's work, without holding `busy`.
        {
            let Ok(mut q) = s.pending.lock() else { return };
            while q.moves.is_empty() && q.activate.is_none() && !q.quit {
                q = match s.wake.wait(q) {
                    Ok(q) => q,
                    Err(_) => return,
                };
            }
            if q.quit {
                return;
            }
        }
        // Take `busy` before the batch, so `exclusive` can't slip in between
        // taking a move and applying it. (Lock order: busy, then pending;
        // `exclusive` never holds pending while waiting for busy.)
        let Ok(busy) = s.busy.lock() else { return };
        let (moves, activate) = {
            let Ok(mut q) = s.pending.lock() else { return };
            if q.quit {
                return;
            }
            (std::mem::take(&mut q.moves), q.activate.take())
        };
        if !moves.is_empty() {
            if checked.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
                enhanced = ax::names(|n| ax::get_bool(&app.0, &n.enhanced_ui)).unwrap_or(false);
                checked = Some(Instant::now());
            }
            if enhanced {
                ax::names(|n| ax::set_bool(&app.0, &n.enhanced_ui, false));
            }
            for (_, (el, p)) in moves {
                ax::set_point(&el.0, p);
            }
            if enhanced {
                ax::names(|n| ax::set_bool(&app.0, &n.enhanced_ui, true));
            }
        }
        if let Some((app, win)) = activate {
            // The window first, then the app: the app comes forward already
            // showing the right window, never another one of its windows
            // (which may belong to a different context) for a moment.
            ax::names(|n| ax::set_bool(&win.0, &n.main, true));
            ax::raise(&win.0);
            let front = ax::names(|n| ax::set_bool(&app.0, &n.frontmost, true));
            if !front {
                fallback(pid);
            }
        }
        drop(busy);
    }
}
