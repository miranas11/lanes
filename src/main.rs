//! Lanes — a lightweight scrolling window manager for macOS.
//!
//! Work and Personal live in separate contexts. Inside a context, windows sit
//! side by side in one row that you scroll through with ⌃⌥ shortcuts or by
//! holding ⌃⌥ while scrolling.

mod ax;
mod input;
mod layout;
mod log;
mod mover;
mod spaces;
mod store;
mod ui;
mod wm;

use std::ffi::c_void;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

fn main() {
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("Lanes must start on the main thread");
        return;
    };
    log::init();
    crate::log!(
        "---- Lanes {} starting (pid {})",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    );
    let app = NSApplication::sharedApplication(mtm);
    // Two copies would fight over the same windows.
    if ui::another_copy_running() {
        crate::log!("another copy is already running; exiting");
        return;
    }
    // Menu bar app: no Dock icon, no app menu.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    ui::init(mtm);
    signals::install();
    ax::limit_waiting();
    wm::boot(mtm);
    app.run();
}

/// Quit cleanly on Ctrl-C, `kill` or logout so hidden windows come back.
mod signals {
    use super::*;

    #[repr(C)]
    struct Opaque([u8; 0]);

    #[link(name = "System", kind = "dylib")]
    unsafe extern "C" {
        static _dispatch_source_type_signal: Opaque;
        static _dispatch_main_q: Opaque;
        fn dispatch_source_create(
            ty: *const Opaque,
            handle: usize,
            mask: usize,
            queue: *const Opaque,
        ) -> *mut c_void;
        fn dispatch_source_set_event_handler_f(
            source: *mut c_void,
            handler: extern "C" fn(*mut c_void),
        );
        fn dispatch_resume(object: *mut c_void);
    }

    extern "C" fn on_signal(_ctx: *mut c_void) {
        wm::push(wm::Cmd::Quit);
        // If the window manager never started, just leave.
        if wm::mode() == wm::Mode::Off {
            ui::terminate();
        }
    }

    pub fn install() {
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            unsafe {
                libc::signal(sig, libc::SIG_IGN);
                let src = dispatch_source_create(
                    &raw const _dispatch_source_type_signal,
                    sig as usize,
                    0,
                    &raw const _dispatch_main_q,
                );
                if !src.is_null() {
                    dispatch_source_set_event_handler_f(src, on_signal);
                    dispatch_resume(src);
                }
            }
        }
    }
}
