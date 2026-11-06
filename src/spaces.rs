//! Counts macOS desktops (Spaces). Read-only: Lanes never changes them.
//!
//! macOS hides windows on other desktops from the Accessibility API and does
//! not let apps move windows between desktops, so Lanes works on one desktop
//! and tells you when there are more.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::NSString;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    // Private but long-standing, read-only SkyLight calls (used by most window managers).
    fn CGSMainConnectionID() -> i32;
    fn CGSCopyManagedDisplaySpaces(cid: i32) -> *mut AnyObject;
}

unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *mut AnyObject;
}

/// (window id, owner pid) of normal windows currently on screen.
pub fn onscreen_windows() -> Vec<(u32, i32)> {
    const ON_SCREEN_ONLY: u32 = 1 << 0;
    const EXCLUDE_DESKTOP: u32 = 1 << 4;
    let raw = unsafe { CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0) };
    let Some(list) = (unsafe { Retained::from_raw(raw) }) else {
        return Vec::new();
    };
    let num = |obj: &AnyObject, key: &str| -> Option<i64> {
        let v = get(obj, key)?;
        Some(unsafe { msg_send![&*v, longLongValue] })
    };
    let mut out = Vec::new();
    for i in 0..count(&list) {
        let Some(w) = at(&list, i) else { continue };
        if num(&w, "kCGWindowLayer") != Some(0) {
            continue;
        }
        if let (Some(id), Some(pid)) = (num(&w, "kCGWindowNumber"), num(&w, "kCGWindowOwnerPID")) {
            out.push((id as u32, pid as i32));
        }
    }
    out
}

pub struct Desktops {
    /// Regular desktops on the main display.
    pub desktops: usize,
    /// Apps in native full screen (each is its own Space).
    pub fullscreen: usize,
}

fn get(obj: &AnyObject, key: &str) -> Option<Retained<AnyObject>> {
    let key = NSString::from_str(key);
    unsafe { msg_send![obj, objectForKey: &*key] }
}

fn count(arr: &AnyObject) -> usize {
    unsafe { msg_send![arr, count] }
}

fn at(arr: &AnyObject, i: usize) -> Option<Retained<AnyObject>> {
    unsafe { msg_send![arr, objectAtIndex: i] }
}

pub fn main_display() -> Option<Desktops> {
    let raw = unsafe { CGSCopyManagedDisplaySpaces(CGSMainConnectionID()) };
    // The call returns a +1 (copied) CFArray, toll-free bridged to NSArray.
    let displays: Retained<AnyObject> = unsafe { Retained::from_raw(raw)? };
    let main = at(&displays, 0)?;
    let spaces = get(&main, "Spaces")?;
    let mut d = Desktops {
        desktops: 0,
        fullscreen: 0,
    };
    for i in 0..count(&spaces) {
        let Some(space) = at(&spaces, i) else {
            continue;
        };
        let ty: i64 = match get(&space, "type") {
            Some(n) => unsafe { msg_send![&*n, longLongValue] },
            None => 0,
        };
        match ty {
            0 => d.desktops += 1,
            4 => d.fullscreen += 1,
            _ => {}
        }
    }
    Some(d)
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "reads this Mac's desktops; run with --ignored"]
    fn reads_desktops() {
        let d = super::main_display().expect("could not read desktops");
        println!("desktops={} fullscreen={}", d.desktops, d.fullscreen);
        assert!(d.desktops >= 1);
    }
}

#[cfg(test)]
mod scan_tests {
    #[test]
    #[ignore = "reads this Mac's windows; run with --ignored"]
    fn lists_onscreen_windows() {
        let w = super::onscreen_windows();
        println!("onscreen windows: {}", w.len());
        assert!(
            w.iter().any(|(id, _)| *id == 37820),
            "WhatsApp window not seen"
        );
    }
}
