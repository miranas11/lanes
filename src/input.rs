//! Global keyboard and scroll handling through one CoreGraphics event tap.
//!
//! The callback only reads the event and queues a command; all real work
//! happens later on the main run loop, so typing and scrolling never lag.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::time::Instant;

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};

use crate::layout;
use crate::wm::{self, Cmd, Mode};

// macOS virtual key codes (physical key positions, so they work on any layout).
mod kc {
    pub const H: i64 = 0x04;
    pub const Y: i64 = 0x10;
    pub const N1: i64 = 0x12;
    pub const N2: i64 = 0x13;
    pub const N3: i64 = 0x14;
    pub const N4: i64 = 0x15;
    pub const N6: i64 = 0x16;
    pub const N5: i64 = 0x17;
    pub const EQUAL: i64 = 0x18;
    pub const N9: i64 = 0x19;
    pub const N7: i64 = 0x1A;
    pub const MINUS: i64 = 0x1B;
    pub const N8: i64 = 0x1C;
    pub const RBRACKET: i64 = 0x1E;
    pub const O: i64 = 0x1F;
    pub const LBRACKET: i64 = 0x21;
    pub const P: i64 = 0x23;
    pub const RETURN: i64 = 0x24;
    pub const L: i64 = 0x25;
    pub const M: i64 = 0x2E;
    pub const ESCAPE: i64 = 0x35;
    pub const SPACE: i64 = 0x31;
    pub const DELETE: i64 = 0x33;
    pub const KP_ENTER: i64 = 0x4C;
    pub const LEFT: i64 = 0x7B;
    pub const RIGHT: i64 = 0x7C;
    pub const DOWN: i64 = 0x7D;
    pub const UP: i64 = 0x7E;
}

fn digit(code: i64) -> Option<usize> {
    Some(match code {
        kc::N1 => 0,
        kc::N2 => 1,
        kc::N3 => 2,
        kc::N4 => 3,
        kc::N5 => 4,
        kc::N6 => 5,
        kc::N7 => 6,
        kc::N8 => 7,
        kc::N9 => 8,
        _ => return None,
    })
}

fn arrow_bit(code: i64) -> Option<u8> {
    Some(match code {
        kc::LEFT => layout::LEFT,
        kc::RIGHT => layout::RIGHT,
        kc::UP => layout::UP,
        kc::DOWN => layout::DOWN,
        _ => return None,
    })
}

/// A quick second arrow (tapped, not held) still makes a corner if it comes
/// within this long after the first one.
const CORNER_GRACE_MS: u128 = 350;

/// The letter a key types. Uses the real keyboard layout when no modifier is
/// held; with ⌃/⌥ held (which change the character) it falls back to the
/// key's position on a US layout.
fn typed_letter(e: &CGEvent, code: i64, m: &Mods) -> Option<char> {
    if !m.ctrl && !m.alt {
        let mut buf = [0u16; 4];
        let mut len = 0;
        unsafe {
            CGEvent::keyboard_get_unicode_string(Some(e), 4, &mut len, buf.as_mut_ptr());
        }
        if len > 0
            && let Some(Ok(c)) = char::decode_utf16(buf[..len as usize].iter().copied()).next()
            && c.is_ascii_alphabetic()
        {
            return Some(c.to_ascii_uppercase());
        }
        return None;
    }
    const US: [(i64, char); 26] = [
        (0x00, 'A'),
        (0x0B, 'B'),
        (0x08, 'C'),
        (0x02, 'D'),
        (0x0E, 'E'),
        (0x03, 'F'),
        (0x05, 'G'),
        (0x04, 'H'),
        (0x22, 'I'),
        (0x26, 'J'),
        (0x28, 'K'),
        (0x25, 'L'),
        (0x2E, 'M'),
        (0x2D, 'N'),
        (0x1F, 'O'),
        (0x23, 'P'),
        (0x0C, 'Q'),
        (0x0F, 'R'),
        (0x01, 'S'),
        (0x11, 'T'),
        (0x20, 'U'),
        (0x09, 'V'),
        (0x0D, 'W'),
        (0x07, 'X'),
        (0x10, 'Y'),
        (0x06, 'Z'),
    ];
    US.iter().find(|(k, _)| *k == code).map(|(_, c)| *c)
}

thread_local! {
    /// Arrows held down right now, and every arrow pressed since the first
    /// went down. The area is decided when the last one is released.
    static ARROWS_HELD: Cell<u8> = const { Cell::new(0) };
    static ARROWS_SEEN: Cell<u8> = const { Cell::new(0) };
    /// Arrow key-ups Lanes must also swallow, because it took the key-down.
    static SWALLOW_UP: Cell<u8> = const { Cell::new(0) };
    /// The last single arrow that picked a half, for tapped corners.
    static LAST_SINGLE: Cell<Option<(Instant, u8)>> = const { Cell::new(None) };
    static OUTLINE_ON: Cell<bool> = const { Cell::new(false) };
    static TAP: RefCell<Option<CFRetained<CFMachPort>>> = const { RefCell::new(None) };
    static LAST_ROW_SCROLL: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Creates the event tap. Needs the Accessibility permission.
pub fn install() -> bool {
    let mask: u64 = (1 << CGEventType::KeyDown.0)
        | (1 << CGEventType::KeyUp.0)
        | (1 << CGEventType::FlagsChanged.0)
        | (1 << CGEventType::MouseMoved.0)
        | (1 << CGEventType::ScrollWheel.0)
        | (1 << CGEventType::LeftMouseDown.0)
        | (1 << CGEventType::RightMouseDown.0);
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(callback),
            ptr::null_mut(),
        )
    };
    let Some(port) = port else { return false };
    let Some(src) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        return false;
    };
    let Some(rl) = CFRunLoop::main() else {
        return false;
    };
    rl.add_source(Some(&src), unsafe { kCFRunLoopCommonModes });
    CGEvent::tap_enable(&port, true);
    let enabled = CGEvent::tap_is_enabled(&port);
    crate::log!("event tap: installed, enabled={enabled}");
    TAP.with_borrow_mut(|t| *t = Some(port));
    enabled
}

unsafe extern "C-unwind" fn callback(
    _proxy: CGEventTapProxy,
    ty: CGEventType,
    event: NonNull<CGEvent>,
    _info: *mut c_void,
) -> *mut CGEvent {
    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        crate::log!("event tap: disabled by macOS ({}), re-enabling", ty.0);
        TAP.with_borrow(|t| {
            if let Some(port) = t {
                CGEvent::tap_enable(port, true);
            }
        });
        return event.as_ptr();
    }
    let e = unsafe { event.as_ref() };
    let swallow = if ty == CGEventType::KeyDown {
        on_key(e)
    } else if ty == CGEventType::KeyUp {
        on_key_up(e)
    } else if ty == CGEventType::FlagsChanged {
        on_flags(e);
        false
    } else if ty == CGEventType::MouseMoved {
        // Focus follows the pointer once it rests on a window for a moment.
        if wm::mode() == Mode::Normal && crate::ui::follow_mouse() {
            let p = CGEvent::location(Some(e));
            crate::ui::schedule_hover(p.x, p.y);
        }
        false
    } else if ty == CGEventType::ScrollWheel {
        on_scroll(e)
    } else if ty == CGEventType::LeftMouseDown || ty == CGEventType::RightMouseDown {
        // A click outside an open overview (or the area map) closes it.
        if matches!(
            wm::mode(),
            Mode::Overview | Mode::PlaceMap | Mode::PlaceApps
        ) {
            let p = CGEvent::location(Some(e));
            wm::push(Cmd::MouseDown(p.x, p.y));
        }
        false
    } else {
        false
    };
    if swallow {
        ptr::null_mut()
    } else {
        event.as_ptr()
    }
}

struct Mods {
    ctrl: bool,
    alt: bool,
    cmd: bool,
    shift: bool,
}

impl Mods {
    fn of(e: &CGEvent) -> Self {
        let f = CGEvent::flags(Some(e));
        Self {
            ctrl: f.contains(CGEventFlags::MaskControl),
            alt: f.contains(CGEventFlags::MaskAlternate),
            cmd: f.contains(CGEventFlags::MaskCommand),
            shift: f.contains(CGEventFlags::MaskShift),
        }
    }
    /// Control + Option and nothing else: the Lanes chord (max three keys).
    fn chord(&self) -> bool {
        self.ctrl && self.alt && !self.cmd && !self.shift
    }
    fn none(&self) -> bool {
        !self.ctrl && !self.alt && !self.cmd
    }
}

/// Returns true when the key press belongs to Lanes and must not reach the app.
fn on_key(e: &CGEvent) -> bool {
    let code = CGEvent::integer_value_field(Some(e), CGEventField::KeyboardEventKeycode);
    let repeat = CGEvent::integer_value_field(Some(e), CGEventField::KeyboardEventAutorepeat) != 0;
    let m = Mods::of(e);

    match wm::mode() {
        Mode::Off => false,
        Mode::PlaceMap => {
            if m.cmd {
                wm::push(Cmd::PlaceDone);
                return false;
            }
            if let Some(bit) = arrow_bit(code) {
                if !repeat {
                    ARROWS_HELD.set(ARROWS_HELD.get() | bit);
                    ARROWS_SEEN.set(ARROWS_SEEN.get() | bit);
                    SWALLOW_UP.set(SWALLOW_UP.get() | bit);
                    // Light up the area these arrows would pick while they're held.
                    wm::push(Cmd::PlaceHover(layout::area_for_arrows(ARROWS_SEEN.get())));
                }
                return true;
            }
            match code {
                kc::ESCAPE => wm::push(Cmd::PlaceCancel),
                kc::DELETE => wm::push(Cmd::PlaceUngroup),
                kc::RETURN | kc::KP_ENTER => wm::push(Cmd::PlaceDone),
                kc::SPACE if m.chord() => wm::push(Cmd::PlaceDone),
                _ => {
                    // Anything else: finish, and let the key do its normal job.
                    wm::push(Cmd::PlaceDone);
                    if m.chord()
                        && let Some(c) = chord_command(code)
                    {
                        if !repeat {
                            wm::push(c);
                        }
                        return true;
                    }
                    return false;
                }
            }
            true
        }
        Mode::PlaceApps => {
            if m.cmd {
                wm::push(Cmd::PlaceDone);
                return false;
            }
            // A second arrow tapped right after the first: make it a corner.
            if let Some(bit) = arrow_bit(code)
                && let Some((t, first)) = LAST_SINGLE.get()
                && t.elapsed().as_millis() < CORNER_GRACE_MS
                && layout::at_right_angles(first, bit)
            {
                LAST_SINGLE.set(None);
                SWALLOW_UP.set(SWALLOW_UP.get() | bit);
                if let Some(i) = layout::area_for_arrows(first | bit) {
                    wm::push(Cmd::PlaceArea(i));
                }
                return true;
            }
            let cmd = match code {
                kc::ESCAPE => Some(Cmd::PlaceCancel),
                kc::DELETE => Some(Cmd::PlaceBack),
                kc::RETURN | kc::KP_ENTER => Some(Cmd::OvEnter),
                kc::LEFT => Some(Cmd::OvMove(-1, 0)),
                kc::RIGHT => Some(Cmd::OvMove(1, 0)),
                kc::UP => Some(Cmd::OvMove(0, -1)),
                kc::DOWN => Some(Cmd::OvMove(0, 1)),
                _ => typed_letter(e, code, &m).map(Cmd::PlaceKey),
            };
            match cmd {
                Some(c) => {
                    if !repeat || matches!(c, Cmd::OvMove(..)) {
                        wm::push(c);
                    }
                    true
                }
                None => false,
            }
        }
        Mode::Overview => {
            let cmd = match code {
                kc::LEFT if m.none() || m.chord() => Some(Cmd::OvMove(-1, 0)),
                kc::RIGHT if m.none() || m.chord() => Some(Cmd::OvMove(1, 0)),
                kc::H if m.chord() => Some(Cmd::OvMove(-1, 0)),
                kc::L if m.chord() => Some(Cmd::OvMove(1, 0)),
                kc::UP if m.none() => Some(Cmd::OvMove(0, -1)),
                kc::DOWN if m.none() => Some(Cmd::OvMove(0, 1)),
                kc::RETURN | kc::KP_ENTER if m.none() => Some(Cmd::OvEnter),
                kc::ESCAPE if m.none() => Some(Cmd::OvClose),
                kc::UP if m.chord() => Some(Cmd::ZoomOut),
                kc::DOWN if m.chord() => Some(Cmd::ZoomIn),
                kc::O if m.chord() => Some(Cmd::ShowApps),
                kc::P if m.chord() => Some(Cmd::ShowContexts),
                _ => None,
            };
            if let Some(c) = cmd {
                if !repeat || matches!(c, Cmd::OvMove(..)) {
                    wm::push(c);
                }
                return true;
            }
            // Any other Lanes chord closes the overview and runs normally.
            if m.chord()
                && let Some(c) = chord_command(code)
            {
                wm::push(Cmd::OvClose);
                if !repeat {
                    wm::push(c);
                }
                return true;
            }
            false
        }
        Mode::Normal => {
            if !m.chord() {
                return false;
            }
            let Some(c) = chord_command(code) else {
                return false;
            };
            if c == Cmd::PlaceStart {
                reset_arrows();
            }
            let repeatable = matches!(c, Cmd::FocusPrev | Cmd::FocusNext);
            if !repeat || repeatable {
                wm::push(c);
            }
            true
        }
    }
}

/// Arrow released: once every arrow is up, the arrows that were pressed pick the area.
fn on_key_up(e: &CGEvent) -> bool {
    let code = CGEvent::integer_value_field(Some(e), CGEventField::KeyboardEventKeycode);
    let Some(bit) = arrow_bit(code) else {
        return false;
    };
    if SWALLOW_UP.get() & bit == 0 {
        return false;
    }
    SWALLOW_UP.set(SWALLOW_UP.get() & !bit);
    ARROWS_HELD.set(ARROWS_HELD.get() & !bit);
    if ARROWS_HELD.get() == 0 {
        let seen = ARROWS_SEEN.replace(0);
        if wm::mode() == Mode::PlaceMap
            && let Some(i) = layout::area_for_arrows(seen)
        {
            wm::push(Cmd::PlaceArea(i));
            LAST_SINGLE.set((seen.count_ones() == 1).then(|| (Instant::now(), seen)));
        }
    }
    true
}

/// ⌃⌥ down: outline the focused window; released: remove the outline.
fn on_flags(e: &CGEvent) {
    if wm::mode() == Mode::Off {
        return;
    }
    let held = Mods::of(e).chord();
    if held != OUTLINE_ON.get() {
        OUTLINE_ON.set(held);
        wm::push(Cmd::Outline(held));
    }
}

/// Forget half-finished arrow presses (when ⌃⌥Space starts or ends).
fn reset_arrows() {
    ARROWS_HELD.set(0);
    ARROWS_SEEN.set(0);
    LAST_SINGLE.set(None);
}

fn chord_command(code: i64) -> Option<Cmd> {
    if let Some(n) = digit(code) {
        return (n < wm::context_count()).then_some(Cmd::GoTo(n));
    }
    Some(match code {
        kc::LEFT | kc::H => Cmd::FocusPrev,
        kc::RIGHT | kc::L => Cmd::FocusNext,
        kc::LBRACKET => Cmd::ContextPrev,
        kc::RBRACKET => Cmd::ContextNext,
        kc::O => Cmd::ShowApps,
        kc::P => Cmd::ShowContexts,
        kc::UP => Cmd::ZoomOut,
        kc::EQUAL => Cmd::Wider,
        kc::MINUS => Cmd::Narrower,
        kc::M => Cmd::Center,
        kc::RETURN => Cmd::FullView,
        kc::Y => Cmd::SnapBack,
        kc::SPACE => Cmd::PlaceStart,
        _ => return None,
    })
}

/// Hold ⌃⌥ and scroll to slide the row. Plain scrolling is left to the app.
fn on_scroll(e: &CGEvent) -> bool {
    if wm::mode() != Mode::Normal {
        return false;
    }
    let m = Mods::of(e);
    let now = Instant::now();
    if m.ctrl && m.alt && !m.cmd {
        let dy =
            CGEvent::integer_value_field(Some(e), CGEventField::ScrollWheelEventPointDeltaAxis1);
        let dx =
            CGEvent::integer_value_field(Some(e), CGEventField::ScrollWheelEventPointDeltaAxis2);
        let d = if dx.abs() > dy.abs() { -dx } else { -dy };
        if d != 0 {
            wm::push(Cmd::Scroll(d as f64));
        }
        LAST_ROW_SCROLL.set(Some(now));
        return true;
    }
    // Keys released mid-glide: swallow the leftover momentum so the row stops
    // where you stopped and the app underneath doesn't suddenly scroll.
    let momentum =
        CGEvent::integer_value_field(Some(e), CGEventField::ScrollWheelEventMomentumPhase) != 0;
    if momentum
        && let Some(t) = LAST_ROW_SCROLL.get()
        && now.duration_since(t).as_millis() < 400
    {
        LAST_ROW_SCROLL.set(Some(now));
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_row_maps_to_contexts() {
        assert_eq!(digit(kc::N1), Some(0));
        assert_eq!(digit(kc::N9), Some(8));
        assert_eq!(digit(kc::O), None);
    }

    #[test]
    fn every_chord_is_on_the_right_hand() {
        for code in [kc::H, kc::L, kc::Y, kc::M, kc::O, kc::P] {
            assert!(chord_command(code).is_some());
        }
    }
}
