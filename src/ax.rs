//! Thin, safe-ish helpers over the macOS Accessibility (AX) API.
//!
//! Every call here is an IPC round-trip to another app, so callers should
//! keep the number of calls small (the window manager caches what it set).

use std::ffi::c_void;
use std::ptr::{self, NonNull};

use objc2_application_services::{
    AXError, AXIsProcessTrustedWithOptions, AXObserver, AXObserverCallback, AXUIElement, AXValue,
    AXValueType, kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFRetained, CFRunLoop, CFRunLoopSource, CFString, CFType,
    CGPoint, CGSize, kCFRunLoopCommonModes,
};

pub type Elem = CFRetained<AXUIElement>;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Private but long-stable API (used by every macOS window manager) that maps
    /// an AX window element to its CGWindowID.
    fn _AXUIElementGetWindow(element: NonNull<AXUIElement>, wid: *mut u32) -> AXError;
}

/// Attribute, action and notification names, created once.
pub struct Names {
    pub windows: CFRetained<CFString>,
    pub focused_window: CFRetained<CFString>,
    pub main: CFRetained<CFString>,
    pub frontmost: CFRetained<CFString>,
    pub position: CFRetained<CFString>,
    pub size: CFRetained<CFString>,
    pub title: CFRetained<CFString>,
    pub role: CFRetained<CFString>,
    pub subrole: CFRetained<CFString>,
    pub minimized: CFRetained<CFString>,
    pub fullscreen: CFRetained<CFString>,
    pub enhanced_ui: CFRetained<CFString>,
    pub raise: CFRetained<CFString>,
    pub n_window_created: CFRetained<CFString>,
    pub n_focused_window_changed: CFRetained<CFString>,
    pub n_destroyed: CFRetained<CFString>,
    pub n_miniaturized: CFRetained<CFString>,
    pub n_deminiaturized: CFRetained<CFString>,
    pub n_resized: CFRetained<CFString>,
}

impl Names {
    fn new() -> Self {
        let s = CFString::from_static_str;
        Self {
            windows: s("AXWindows"),
            focused_window: s("AXFocusedWindow"),
            main: s("AXMain"),
            frontmost: s("AXFrontmost"),
            position: s("AXPosition"),
            size: s("AXSize"),
            title: s("AXTitle"),
            role: s("AXRole"),
            subrole: s("AXSubrole"),
            minimized: s("AXMinimized"),
            fullscreen: s("AXFullScreen"),
            enhanced_ui: s("AXEnhancedUserInterface"),
            raise: s("AXRaise"),
            n_window_created: s("AXWindowCreated"),
            n_focused_window_changed: s("AXFocusedWindowChanged"),
            n_destroyed: s("AXUIElementDestroyed"),
            n_miniaturized: s("AXWindowMiniaturized"),
            n_deminiaturized: s("AXWindowDeminiaturized"),
            n_resized: s("AXWindowResized"),
        }
    }
}

thread_local! {
    static NAMES: Names = Names::new();
}

/// Run `f` with the shared attribute names.
pub fn names<R>(f: impl FnOnce(&Names) -> R) -> R {
    NAMES.with(f)
}

/// Is this process allowed to use Accessibility? Optionally shows the system prompt.
pub fn trusted(prompt: bool) -> bool {
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let value = CFBoolean::new(prompt);
    let dict = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[value]);
    let dict: &CFDictionary = dict.as_opaque();
    unsafe { AXIsProcessTrustedWithOptions(Some(dict)) }
}

pub fn app_element(pid: i32) -> Elem {
    let el = unsafe { AXUIElement::new_application(pid) };
    // Never let a frozen app freeze us for long.
    unsafe { el.set_messaging_timeout(0.5) };
    el
}

fn copy(el: &AXUIElement, attr: &CFString) -> Result<CFRetained<CFType>, AXError> {
    let mut out: *const CFType = ptr::null();
    let err = unsafe { el.copy_attribute_value(attr, NonNull::from(&mut out)) };
    if err != AXError::Success {
        return Err(err);
    }
    match NonNull::new(out as *mut CFType) {
        Some(p) => Ok(unsafe { CFRetained::from_raw(p) }),
        None => Err(AXError::NoValue),
    }
}

pub fn get_bool(el: &AXUIElement, attr: &CFString) -> Option<bool> {
    let v = copy(el, attr).ok()?;
    v.downcast::<CFBoolean>().ok().map(|b| b.as_bool())
}

pub fn set_bool(el: &AXUIElement, attr: &CFString, value: bool) -> bool {
    let v: &CFType = CFBoolean::new(value);
    unsafe { el.set_attribute_value(attr, v) == AXError::Success }
}

pub fn get_string(el: &AXUIElement, attr: &CFString) -> Option<String> {
    let v = copy(el, attr).ok()?;
    v.downcast::<CFString>().ok().map(|s| s.to_string())
}

pub fn get_point(el: &AXUIElement) -> Option<CGPoint> {
    let v = names(|n| copy(el, &n.position))
        .ok()?
        .downcast::<AXValue>()
        .ok()?;
    let mut p = CGPoint::new(0.0, 0.0);
    let ok = unsafe { v.value(AXValueType::CGPoint, NonNull::from(&mut p).cast()) };
    ok.then_some(p)
}

pub fn get_size(el: &AXUIElement) -> Option<CGSize> {
    let v = names(|n| copy(el, &n.size))
        .ok()?
        .downcast::<AXValue>()
        .ok()?;
    let mut s = CGSize::new(0.0, 0.0);
    let ok = unsafe { v.value(AXValueType::CGSize, NonNull::from(&mut s).cast()) };
    ok.then_some(s)
}

pub fn set_point(el: &AXUIElement, p: CGPoint) -> bool {
    let Some(v) = (unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&p).cast()) }) else {
        return false;
    };
    let v: &CFType = &v;
    names(|n| unsafe { el.set_attribute_value(&n.position, v) == AXError::Success })
}

pub fn set_size(el: &AXUIElement, s: CGSize) -> bool {
    let Some(v) = (unsafe { AXValue::new(AXValueType::CGSize, NonNull::from(&s).cast()) }) else {
        return false;
    };
    let v: &CFType = &v;
    names(|n| unsafe { el.set_attribute_value(&n.size, v) == AXError::Success })
}

/// Can the window be resized? Fixed-size windows float instead of tiling.
pub fn size_settable(el: &AXUIElement) -> bool {
    let mut settable: u8 = 0;
    let err = names(|n| unsafe { el.is_attribute_settable(&n.size, NonNull::from(&mut settable)) });
    err != AXError::Success || settable != 0
}

pub fn windows(app: &AXUIElement) -> Vec<Elem> {
    let Ok(v) = names(|n| copy(app, &n.windows)) else {
        return Vec::new();
    };
    let Ok(arr) = v.downcast::<CFArray>() else {
        return Vec::new();
    };
    let arr: CFRetained<CFArray<AXUIElement>> = unsafe { CFRetained::cast_unchecked(arr) };
    arr.to_vec()
}

pub fn focused_window(app: &AXUIElement) -> Option<Elem> {
    let v = names(|n| copy(app, &n.focused_window)).ok()?;
    v.downcast::<AXUIElement>().ok()
}

pub fn window_id(el: &AXUIElement) -> Option<u32> {
    let mut id = 0u32;
    let err = unsafe { _AXUIElementGetWindow(NonNull::from(el), &mut id) };
    (err == AXError::Success && id != 0).then_some(id)
}

/// False once the element's process or window is gone.
pub fn is_alive(el: &AXUIElement) -> bool {
    !matches!(names(|n| copy(el, &n.role)), Err(AXError::InvalidUIElement))
}

pub fn raise(el: &AXUIElement) {
    names(|n| unsafe {
        el.perform_action(&n.raise);
    });
}

/// An observer delivering notifications for one app on the main run loop.
pub struct Observer {
    pub obs: CFRetained<AXObserver>,
    src: CFRetained<CFRunLoopSource>,
}

impl Observer {
    pub fn new(pid: i32, cb: AXObserverCallback) -> Option<Self> {
        let mut out: *mut AXObserver = ptr::null_mut();
        let err = unsafe { AXObserver::create(pid, cb, NonNull::from(&mut out)) };
        if err != AXError::Success {
            return None;
        }
        let obs = unsafe { CFRetained::from_raw(NonNull::new(out)?) };
        let src = unsafe { obs.run_loop_source() };
        CFRunLoop::main()?.add_source(Some(&src), unsafe { kCFRunLoopCommonModes });
        Some(Self { obs, src })
    }

    /// Returns false if the app is not ready to be observed yet.
    pub fn add(&self, el: &AXUIElement, note: &CFString, refcon: *mut c_void) -> bool {
        let err = unsafe { self.obs.add_notification(el, note, refcon) };
        err == AXError::Success || err == AXError::NotificationAlreadyRegistered
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        if let Some(rl) = CFRunLoop::main() {
            rl.remove_source(Some(&self.src), unsafe { kCFRunLoopCommonModes });
        }
    }
}
