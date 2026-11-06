//! The window manager: contexts, the scrolling row, focus and the overviews.
//!
//! Everything runs on the main thread. Input callbacks only `push` commands;
//! `drain` handles them in one batch, so a burst of scroll events results in
//! a single layout pass.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplicationActivationOptions, NSApplicationActivationPolicy, NSRunningApplication, NSScreen,
    NSWorkspace,
};
use objc2_application_services::{AXObserver, AXUIElement};
use objc2_core_foundation::{CFString, CGPoint, CGRect, CGSize};

use crate::ax::{self, Elem};
use crate::layout::{self, AREA_NAMES, AREAS, Rect};
use crate::store::{self, Saved};
use crate::ui::{self, AppTile, ContextCard};

const GAP: f64 = 8.0;
const MIN_TILE: f64 = 240.0;
const PRESETS: [f64; 4] = [1.0 / 3.0, 0.5, 2.0 / 3.0, 1.0];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Off,
    Normal,
    Overview,
    /// ⌃⌥Space: the area map is showing.
    PlaceMap,
    /// ⌃⌥Space, then an area key: the app picker is showing.
    PlaceApps,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cmd {
    FocusPrev,
    FocusNext,
    Wider,
    Narrower,
    Center,
    FullView,
    SnapBack,
    ContextPrev,
    ContextNext,
    GoTo(usize),
    ShowApps,
    ShowContexts,
    ZoomOut,
    ZoomIn,
    OvMove(i32, i32),
    OvEnter,
    OvClose,
    OvPick(usize),
    Scroll(f64),
    ScrollEnd,
    AppChanged(i32),
    FocusChanged(i32),
    Activated(i32),
    Resized(u32),
    AppLaunched(i32),
    AppTerminated(i32),
    AppHidden(i32, bool),
    ScreensChanged,
    MouseDown(f64, f64),
    Scan,
    PlaceStart,
    PlaceArea(usize),
    PlaceHover(Option<usize>),
    PlaceKey(char),
    PlaceBack,
    PlaceDone,
    PlaceCancel,
    PlaceUngroup,
    Quit,
}

thread_local! {
    static WM: RefCell<Option<Wm>> = const { RefCell::new(None) };
    static QUEUE: RefCell<Vec<Cmd>> = const { RefCell::new(Vec::new()) };
    static MODE: Cell<Mode> = const { Cell::new(Mode::Off) };
    static CTX_COUNT: Cell<usize> = const { Cell::new(0) };
    static QUIT: Cell<bool> = const { Cell::new(false) };
    static RESTORED: Cell<bool> = const { Cell::new(false) };
}

pub fn mode() -> Mode {
    MODE.get()
}

pub fn context_count() -> usize {
    CTX_COUNT.get()
}

/// Queue a command and make sure a drain is scheduled. Cheap; safe from any callback.
pub fn push(c: Cmd) {
    QUEUE.with_borrow_mut(|q| {
        if let Cmd::Scroll(d) = c
            && let Some(Cmd::Scroll(prev)) = q.last_mut()
        {
            *prev += d;
            return;
        }
        let dedupe = matches!(
            c,
            Cmd::AppChanged(_) | Cmd::FocusChanged(_) | Cmd::Activated(_) | Cmd::Resized(_)
        );
        if dedupe && q.contains(&c) {
            return;
        }
        q.push(c);
    });
    ui::schedule_drain();
}

/// Handle every queued command. Called from the run loop.
pub fn drain() {
    loop {
        let cmds = QUEUE.with_borrow_mut(std::mem::take);
        if cmds.is_empty() {
            break;
        }
        WM.with_borrow_mut(|w| {
            if let Some(w) = w {
                for c in cmds {
                    w.handle(c);
                }
                w.flush();
            }
        });
    }
    if QUIT.get() {
        ui::terminate();
    }
}

pub fn save_now() {
    WM.with_borrow(|w| {
        if let Some(w) = w {
            w.save();
        }
    });
}

/// Put every window back on screen. Runs once, on quit or logout.
pub fn restore_now() {
    if RESTORED.replace(true) {
        return;
    }
    WM.with_borrow_mut(|w| {
        if let Some(w) = w {
            w.save();
            w.restore_all();
        }
    });
}

/// First launch step: start now if trusted, otherwise wait for the permission.
pub fn boot(mtm: MainThreadMarker) {
    if ax::trusted(true) {
        crate::log!("accessibility: allowed");
        start(mtm);
    } else {
        crate::log!("accessibility: not allowed yet, waiting");
        ui::set_trusted(false);
        ui::schedule_trust_poll();
    }
}

pub fn trust_poll(mtm: MainThreadMarker) {
    if WM.with_borrow(|w| w.is_some()) {
        return;
    }
    if ax::trusted(false) {
        // macOS applies the permission to the keyboard listener only after a
        // restart, so restart instead of starting half-working.
        crate::log!("accessibility: just allowed, restarting");
        ui::set_trusted(true);
        if !ui::relaunch() {
            start(mtm);
        }
    } else {
        ui::schedule_trust_poll();
    }
}

fn start(mtm: MainThreadMarker) {
    let mut wm = Wm::new(mtm);
    wm.adopt_all();
    CTX_COUNT.set(wm.ctxs.len());
    let names: Vec<String> = wm.ctxs.iter().map(|c| c.name.clone()).collect();
    let cur = wm.cur;
    WM.with_borrow_mut(|w| *w = Some(wm));
    WM.with_borrow_mut(|w| {
        if let Some(w) = w {
            if let Some(f) = w.ctxs[w.cur].focus {
                w.ensure_visible(f);
            }
            w.layout_needed = true;
            w.flush();
            let (tiles, total) = w.tiles(w.cur);
            crate::log!(
                "boot: scroll={:.0} total={:.0} usable={:.0} focus={:?}",
                w.ctxs[w.cur].scroll,
                total,
                w.usable(),
                w.ctxs[w.cur].focus
            );
            for t in &tiles {
                for (id, _) in &t.ids {
                    if let Some(win) = w.wins.get(id) {
                        crate::log!(
                            "boot: {} {} off={:.0} w={:.0} parked={} applied={:?}",
                            id,
                            w.app_name_of(*id),
                            t.off,
                            t.w,
                            win.parked,
                            win.applied.map(|f| (f.x, f.y, f.w))
                        );
                    }
                }
            }
        }
    });
    ui::rebuild_menu(&names, cur);
    ui::observe_workspace();
    if input::install() {
        MODE.set(Mode::Normal);
    } else {
        crate::log!("event tap: FAILED to install");
        ui::notice_for(
            "Lanes can't read shortcuts",
            "Quit Lanes, allow it in System Settings › Privacy & Security › Accessibility, then reopen it",
            8.0,
        );
        return;
    }
    check_desktops();
}

/// Lanes only sees the desktop it's on; say so when there are more.
fn check_desktops() {
    let Some(d) = crate::spaces::main_display() else {
        return;
    };
    crate::log!(
        "desktops: {} (+{} full-screen apps)",
        d.desktops,
        d.fullscreen
    );
    ui::set_desktop_warning(d.desktops, d.fullscreen);
    if d.desktops > 1 {
        ui::notice_for(
            &format!(
                "You have {} desktops. Lanes uses only this one.",
                d.desktops
            ),
            "Remove the others in Mission Control (hover a desktop, click ✕): their windows move here",
            9.0,
        );
    } else if d.fullscreen > 0 {
        ui::notice_for(
            "Some apps are in full screen",
            "Exit full screen (⌃⌘F) so Lanes can place them in a context",
            7.0,
        );
    }
}

use crate::input;

#[derive(Clone, Copy, PartialEq, Debug)]
struct Frame {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

struct Win {
    id: u32,
    pid: i32,
    el: Elem,
    ctx: usize,
    /// Width as a share of the screen (½, ⅔ …), so it survives resolution changes.
    frac: f64,
    /// The frame Lanes last set, to skip redundant (slow) AX calls.
    applied: Option<Frame>,
    parked: bool,
    present: bool,
    /// Smallest width the app accepted, learned when it refused a smaller one.
    min_w: f64,
    full_prev: Option<f64>,
    /// When Lanes last moved or resized it; resize events right after are ours.
    touched: Instant,
    size: CGSize,
    /// Set when the window shares a screen with others.
    area: Option<Area>,
    /// Its spot in a screen while ⌃⌥Return has it full width, so a second
    /// ⌃⌥Return puts it back.
    left_area: Option<Area>,
}

/// A window's spot inside a screen (several windows sharing one stop in the row).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Area {
    group: u32,
    r: Rect,
}

/// Everything ⌃⌥Space can change, so Esc can put it back.
struct Snapshot {
    order: Vec<u32>,
    areas: Vec<(u32, Option<Area>)>,
    focus: Option<u32>,
}

enum PlaceStage {
    Map,
    Apps {
        area: usize,
        ids: Vec<u32>,
        keys: Vec<char>,
        sel: usize,
        cols: usize,
    },
}

struct AppEntry {
    el: Elem,
    observer: Option<ax::Observer>,
    observing: bool,
    name: String,
    hidden: bool,
    enhanced_ui: bool,
    /// Clicking the desktop activates Finder; never treat that as crossing the wall.
    is_finder: bool,
}

struct Ctx {
    name: String,
    order: Vec<u32>,
    focus: Option<u32>,
    scroll: f64,
}

#[derive(Clone, Copy, Default)]
struct Screen {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    park_right: f64,
    park_left: f64,
    park_y: f64,
    use_left: bool,
    /// The whole main display (including menu bar and Dock), in AX coordinates.
    full: (f64, f64, f64, f64),
}

impl Screen {
    /// Lanes manages the main display only; windows elsewhere are left alone.
    fn overlaps(&self, p: CGPoint, s: CGSize) -> bool {
        let (fx, fy, fw, fh) = self.full;
        p.x < fx + fw && p.x + s.width > fx && p.y < fy + fh && p.y + s.height > fy
    }
}

enum Overlay {
    None,
    Apps {
        sel: usize,
        ids: Vec<u32>,
        cols: usize,
    },
    Contexts {
        sel: usize,
        cols: usize,
    },
    Place {
        /// The screen being built; None until the first app is placed.
        group: Option<u32>,
        /// Where a new screen goes in the row: the focused window's spot.
        anchor: usize,
        snapshot: Snapshot,
        stage: PlaceStage,
        /// The window you were in; the first area you pick takes it directly.
        first: Option<u32>,
    },
}

/// One stop in the row: a single window, or all the windows of a screen.
struct Tile {
    ids: Vec<(u32, Option<Rect>)>,
    off: f64,
    w: f64,
}

impl Tile {
    fn has(&self, id: u32) -> bool {
        self.ids.iter().any(|(x, _)| *x == id)
    }
}

struct Wm {
    mtm: MainThreadMarker,
    own_pid: i32,
    wins: HashMap<u32, Win>,
    apps: HashMap<i32, AppEntry>,
    ctxs: Vec<Ctx>,
    cur: usize,
    scr: Screen,
    saved: HashMap<u32, Saved>,
    overlay: Overlay,
    booting: bool,
    boot_pending: Vec<(u32, usize, f64)>,
    /// On-screen windows Lanes looked at and decided not to manage.
    ignored: HashSet<u32>,
    layout_needed: bool,
    save_needed: bool,
    dots_needed: bool,
    quiet_until: Instant,
    next_group: u32,
    /// When each window last had focus (a counter), for "most recent first".
    used: HashMap<u32, u64>,
    use_tick: u64,
    /// True while ⌃⌥-scrolling, so windows can slide in before slivers are tidied.
    scrolling: bool,
}

fn read_screen(mtm: MainThreadMarker) -> Screen {
    let screens = NSScreen::screens(mtm);
    let Some(primary) = screens.firstObject() else {
        return Screen {
            w: 1440.0,
            h: 900.0,
            park_right: 1439.0,
            park_y: 899.0,
            full: (0.0, 0.0, 1440.0, 900.0),
            ..Default::default()
        };
    };
    let pf = primary.frame();
    let ph = pf.size.height;
    let to_ax = |r: CGRect| {
        (
            r.origin.x,
            ph - (r.origin.y + r.size.height),
            r.size.width,
            r.size.height,
        )
    };
    let (x, y, w, h) = to_ax(primary.visibleFrame());
    let (fx, fy, fw, fh) = to_ax(pf);
    let (mut right, mut left) = (false, false);
    for s in screens.iter().skip(1) {
        let (sx, _, sw, _) = to_ax(s.frame());
        if sx >= fx + fw - 1.0 {
            right = true;
        }
        if sx + sw <= fx + 1.0 {
            left = true;
        }
    }
    Screen {
        x,
        y,
        w,
        h,
        park_right: fx + fw - 1.0,
        park_left: fx + 1.0,
        park_y: fy + fh - 1.0,
        use_left: right && !left,
        full: (fx, fy, fw, fh),
    }
}

fn snap_frac(f: f64) -> f64 {
    let f = f.clamp(0.25, 1.0);
    PRESETS
        .iter()
        .copied()
        .find(|p| (p - f).abs() < 0.04)
        .unwrap_or(f)
}

unsafe extern "C-unwind" fn on_ax_note(
    _obs: NonNull<AXObserver>,
    el: NonNull<AXUIElement>,
    note: NonNull<CFString>,
    refcon: *mut c_void,
) {
    let pid = refcon as isize as i32;
    let note = unsafe { note.as_ref() }.to_string();
    let cmd = match note.as_str() {
        "AXWindowCreated"
        | "AXUIElementDestroyed"
        | "AXWindowMiniaturized"
        | "AXWindowDeminiaturized" => Cmd::AppChanged(pid),
        "AXFocusedWindowChanged" => Cmd::FocusChanged(pid),
        "AXWindowResized" => match ax::window_id(unsafe { el.as_ref() }) {
            Some(id) => Cmd::Resized(id),
            None => return,
        },
        _ => return,
    };
    push(cmd);
}

impl Wm {
    fn new(mtm: MainThreadMarker) -> Self {
        let names = store::load_contexts();
        let (cur, saved) = store::load_state();
        let ctxs: Vec<Ctx> = names
            .into_iter()
            .map(|name| Ctx {
                name,
                order: Vec::new(),
                focus: None,
                scroll: 0.0,
            })
            .collect();
        let cur = cur.min(ctxs.len() - 1);
        let next_group = saved
            .values()
            .filter_map(|s| s.area.map(|a| a.0))
            .max()
            .unwrap_or(0);
        Self {
            mtm,
            own_pid: std::process::id() as i32,
            wins: HashMap::new(),
            apps: HashMap::new(),
            cur,
            ctxs,
            scr: read_screen(mtm),
            saved,
            overlay: Overlay::None,
            booting: false,
            boot_pending: Vec::new(),
            ignored: HashSet::new(),
            layout_needed: false,
            save_needed: false,
            dots_needed: false,
            quiet_until: Instant::now(),
            next_group,
            used: HashMap::new(),
            use_tick: 0,
            scrolling: false,
        }
    }

    fn usable(&self) -> f64 {
        self.scr.w - 2.0 * GAP
    }

    // ---------- adopting apps and windows ----------

    fn adopt_all(&mut self) {
        self.booting = true;
        let apps = NSWorkspace::sharedWorkspace().runningApplications();
        for app in apps.iter() {
            if app.activationPolicy() == NSApplicationActivationPolicy::Regular {
                self.register_app(app.processIdentifier());
            }
        }
        self.booting = false;

        // Saved windows keep their order; new ones follow, left to right.
        let mut pending = std::mem::take(&mut self.boot_pending);
        pending.sort_by(|a, b| {
            (a.1, a.2)
                .partial_cmp(&(b.1, b.2))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (id, ctx, _) in pending {
            self.ctxs[ctx].order.push(id);
        }
        let front = self.frontmost_window();
        for (i, c) in self.ctxs.iter_mut().enumerate() {
            c.focus = match front {
                Some(f) if i == self.cur && c.order.contains(&f) => Some(f),
                _ => c.order.first().copied(),
            };
        }
        self.saved.clear();
        self.save_needed = true;
    }

    fn frontmost_window(&self) -> Option<u32> {
        let pid = NSWorkspace::sharedWorkspace()
            .frontmostApplication()?
            .processIdentifier();
        let app = self.apps.get(&pid)?;
        let el = ax::focused_window(&app.el)?;
        ax::window_id(&el)
    }

    fn register_app(&mut self, pid: i32) {
        if pid == self.own_pid || self.apps.contains_key(&pid) {
            return;
        }
        let running = NSRunningApplication::runningApplicationWithProcessIdentifier(pid);
        if let Some(r) = &running
            && r.activationPolicy() != NSApplicationActivationPolicy::Regular
        {
            return;
        }
        let name = running
            .as_ref()
            .and_then(|r| r.localizedName())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let hidden = running.as_ref().is_some_and(|r| r.isHidden());
        let is_finder = running
            .as_ref()
            .and_then(|r| r.bundleIdentifier())
            .is_some_and(|b| b.to_string() == "com.apple.finder");
        let el = ax::app_element(pid);
        let enhanced_ui = ax::names(|n| ax::get_bool(&el, &n.enhanced_ui)).unwrap_or(false);
        let observer = ax::Observer::new(pid, Some(on_ax_note));
        self.apps.insert(
            pid,
            AppEntry {
                el,
                observer,
                observing: false,
                name,
                hidden,
                enhanced_ui,
                is_finder,
            },
        );
        self.observe_app(pid);
        self.refresh_app(pid);
    }

    fn observe_app(&mut self, pid: i32) {
        let Some(app) = self.apps.get_mut(&pid) else {
            return;
        };
        if app.observing {
            return;
        }
        let Some(obs) = &app.observer else { return };
        let refcon = pid as isize as *mut c_void;
        app.observing = ax::names(|n| {
            obs.add(&app.el, &n.n_window_created, refcon)
                && obs.add(&app.el, &n.n_focused_window_changed, refcon)
        });
    }

    /// Re-reads one app's windows. Returns true if anything changed.
    fn refresh_app(&mut self, pid: i32) -> bool {
        self.observe_app(pid);
        let Some(app_el) = self.apps.get(&pid).map(|a| a.el.clone()) else {
            return false;
        };
        let mut seen = HashSet::new();
        let mut changed = false;
        for el in ax::windows(&app_el) {
            let Some(id) = ax::window_id(&el) else {
                continue;
            };
            let (subrole, minimized, fullscreen) = ax::names(|n| {
                (
                    ax::get_string(&el, &n.subrole),
                    ax::get_bool(&el, &n.minimized).unwrap_or(false),
                    ax::get_bool(&el, &n.fullscreen).unwrap_or(false),
                )
            });
            if subrole.as_deref() != Some("AXStandardWindow") || minimized || fullscreen {
                continue;
            }
            seen.insert(id);
            if let Some(w) = self.wins.get_mut(&id) {
                if !w.present {
                    w.present = true;
                    w.applied = None;
                    changed = true;
                }
                continue;
            }
            // New windows on another display stay there, unmanaged.
            let known = self.saved.contains_key(&id);
            if !known {
                let (Some(p), Some(sz)) = (ax::get_point(&el), ax::get_size(&el)) else {
                    continue;
                };
                if !self.scr.overlaps(p, sz) {
                    seen.remove(&id);
                    continue;
                }
            }
            self.adopt(pid, id, el);
            changed = true;
        }
        let missing: Vec<u32> = self
            .wins
            .values()
            .filter(|w| w.pid == pid && !seen.contains(&w.id))
            .map(|w| w.id)
            .collect();
        for id in missing {
            let Some(w) = self.wins.get_mut(&id) else {
                continue;
            };
            if ax::is_alive(&w.el) {
                if w.present {
                    w.present = false;
                    changed = true;
                }
            } else {
                self.remove_window(id);
                changed = true;
            }
        }
        if changed {
            self.layout_needed = true;
            self.save_needed = true;
        }
        changed
    }

    fn adopt(&mut self, pid: i32, id: u32, el: Elem) {
        let size = ax::get_size(&el).unwrap_or(CGSize::new(800.0, 600.0));
        let pos = ax::get_point(&el).unwrap_or(CGPoint::new(0.0, 0.0));
        let app_name = self
            .apps
            .get(&pid)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let saved = self
            .saved
            .remove(&id)
            .filter(|s| s.pid == pid && s.app == app_name && s.ctx < self.ctxs.len());
        let usable = self.usable();
        let natural = ((size.width + GAP) / (usable + GAP)).clamp(0.05, 1.0);
        // Windows that can't be resized (Calculator, About boxes) keep their own width.
        let fixed = !ax::size_settable(&el);
        let area = saved
            .as_ref()
            .and_then(|s| s.area)
            .map(|(group, r)| Area { group, r });
        let (ctx, frac) = match &saved {
            Some(s) if !fixed => (s.ctx, s.frac.clamp(0.2, 1.0)),
            Some(s) => (s.ctx, natural),
            None if fixed => (self.cur, natural),
            // Big windows start at half width so you always see two side by side.
            None if natural > 0.7 => (self.cur, 0.5),
            None => (self.cur, snap_frac(natural.max(1.0 / 3.0))),
        };

        if let Some(app) = self.apps.get(&pid)
            && let Some(obs) = &app.observer
        {
            let refcon = pid as isize as *mut c_void;
            ax::names(|n| {
                obs.add(&el, &n.n_destroyed, refcon);
                obs.add(&el, &n.n_miniaturized, refcon);
                obs.add(&el, &n.n_deminiaturized, refcon);
                obs.add(&el, &n.n_resized, refcon);
            });
        }

        self.wins.insert(
            id,
            Win {
                id,
                pid,
                el,
                ctx,
                frac,
                applied: None,
                parked: false,
                present: true,
                min_w: 0.0,
                full_prev: None,
                touched: Instant::now(),
                size,
                area,
                left_area: None,
            },
        );

        if self.booting {
            let key = saved.map(|s| s.order as f64).unwrap_or(1.0e6 + pos.x);
            self.boot_pending.push((id, ctx, key));
            return;
        }
        // A new window takes the spot right after the focused one and gets shown.
        let c = &mut self.ctxs[ctx];
        let at = c
            .focus
            .and_then(|f| c.order.iter().position(|&x| x == f))
            .map_or(c.order.len(), |i| i + 1);
        c.order.insert(at, id);
        c.focus = Some(id);
        if ctx == self.cur {
            self.ensure_visible(id);
        }
    }

    fn remove_window(&mut self, id: u32) {
        if self.wins.remove(&id).is_none() {
            return;
        }
        self.used.remove(&id);
        for c in &mut self.ctxs {
            if let Some(i) = c.order.iter().position(|&x| x == id) {
                c.order.remove(i);
                if c.focus == Some(id) {
                    c.focus = c
                        .order
                        .get(i)
                        .or_else(|| i.checked_sub(1).and_then(|j| c.order.get(j)))
                        .copied();
                }
            }
        }
    }

    // ---------- layout ----------

    fn shown(&self, w: &Win) -> bool {
        w.present && !self.apps.get(&w.pid).is_some_and(|a| a.hidden)
    }

    /// The row: each stop is a window, or a whole screen of windows.
    fn tiles(&self, ci: usize) -> (Vec<Tile>, f64) {
        let usable = self.usable();
        let order = &self.ctxs[ci].order;
        let mut out = Vec::new();
        let mut off = 0.0;
        let mut seen_groups: Vec<u32> = Vec::new();
        for &id in order {
            let Some(w) = self.wins.get(&id) else {
                continue;
            };
            if !self.shown(w) {
                continue;
            }
            let (ids, width) = match w.area {
                Some(a) => {
                    if seen_groups.contains(&a.group) {
                        continue;
                    }
                    seen_groups.push(a.group);
                    let mut ids: Vec<(u32, Option<Rect>)> = order
                        .iter()
                        .filter_map(|m| self.wins.get(m))
                        .filter(|m| self.shown(m))
                        .filter_map(|m| {
                            m.area
                                .filter(|ma| ma.group == a.group)
                                .map(|ma| (m.id, Some(ma.r)))
                        })
                        .collect();
                    ids.sort_by_key(|(_, r)| r.map(layout::reading_key));
                    (ids, usable)
                }
                None => (
                    vec![(id, None)],
                    ((usable + GAP) * w.frac - GAP)
                        .max(w.min_w)
                        .clamp(MIN_TILE.min(usable), usable),
                ),
            };
            out.push(Tile {
                ids,
                off,
                w: width.round(),
            });
            off += width.round() + GAP;
        }
        let total = if out.is_empty() { 0.0 } else { off - GAP };
        (out, total)
    }

    /// Every window in row order (windows of a screen in reading order).
    fn row_ids(&self) -> Vec<u32> {
        self.tiles(self.cur)
            .0
            .into_iter()
            .flat_map(|t| t.ids.into_iter().map(|(id, _)| id))
            .collect()
    }

    fn layout(&mut self) {
        self.layout_needed = false;
        let cur = self.cur;
        let usable = self.usable();
        let (tiles, total) = self.tiles(cur);
        let max_scroll = (total - usable).max(0.0);
        let ctx = &mut self.ctxs[cur];
        ctx.scroll = ctx.scroll.clamp(0.0, max_scroll);
        let origin = if total <= usable {
            self.scr.x + GAP + ((usable - total) / 2.0).round()
        } else {
            self.scr.x + GAP - ctx.scroll
        };
        let top = self.scr.y + GAP;
        let h = self.scr.h - 2.0 * GAP;
        let (left, right) = (self.scr.x, self.scr.x + self.scr.w);

        // A neighbour showing only a thin sliver at the edge is hidden, so the
        // main app is framed on its own. Never the window you're using, and
        // not while you're scrolling (windows need to slide in smoothly).
        let sliver = (self.scr.w * 0.2).max(160.0);
        let focus = self.ctxs[cur].focus;
        // Show the current context first, then hide the rest: less flicker.
        for t in &tiles {
            let x = (origin + t.off).round();
            let visible = (x + t.w).min(right) - x.max(left);
            let is_sliver = !self.scrolling
                && visible < t.w - 1.0
                && visible < sliver
                && !focus.is_some_and(|f| t.has(f));
            let off_screen = x + t.w <= left + 1.0 || x >= right - 1.0 || is_sliver;
            for &(id, r) in &t.ids {
                if off_screen {
                    self.park(id);
                    continue;
                }
                let f = match r {
                    None => Frame {
                        x,
                        y: top,
                        w: t.w,
                        h,
                    },
                    Some(r) => {
                        let (x, y, w, h) = layout::member_frame(x, top, t.w, h, r, GAP);
                        Frame { x, y, w, h }
                    }
                };
                self.place(id, f);
            }
        }
        let others: Vec<u32> = self
            .wins
            .values()
            .filter(|w| w.ctx != cur && w.present && !w.parked)
            .map(|w| w.id)
            .collect();
        for id in others {
            self.park(id);
        }
    }

    fn with_enhanced_ui_off(&self, pid: i32, f: impl FnOnce()) {
        let app = self
            .apps
            .get(&pid)
            .filter(|a| a.enhanced_ui)
            .map(|a| a.el.clone());
        if let Some(a) = &app {
            ax::names(|n| ax::set_bool(a, &n.enhanced_ui, false));
        }
        f();
        if let Some(a) = &app {
            ax::names(|n| ax::set_bool(a, &n.enhanced_ui, true));
        }
    }

    fn place(&mut self, id: u32, f: Frame) {
        let Some(w) = self.wins.get(&id) else { return };
        if !w.parked && w.applied == Some(f) {
            return;
        }
        let size_changed = w.parked
            || w.applied
                .is_none_or(|a| (a.w - f.w).abs() > 0.5 || (a.h - f.h).abs() > 0.5);
        let el = w.el.clone();
        let mut actual = None;
        self.with_enhanced_ui_off(w.pid, || {
            let size = CGSize::new(f.w, f.h);
            if size_changed {
                ax::set_size(&el, size);
            }
            ax::set_point(&el, CGPoint::new(f.x, f.y));
            if size_changed {
                // Again: macOS may have clamped the size at the old position.
                ax::set_size(&el, size);
                actual = ax::get_size(&el);
            }
        });
        let Some(w) = self.wins.get_mut(&id) else {
            return;
        };
        w.applied = Some(f);
        w.parked = false;
        w.touched = Instant::now();
        if let Some(a) = actual {
            w.size = a;
            if a.width > f.w + 2.0 && a.width > w.min_w + 1.0 {
                // The app has a minimum width: make room for it.
                w.min_w = a.width;
                self.layout_needed = true;
            }
        }
    }

    fn park(&mut self, id: u32) {
        let scr = self.scr;
        let Some(w) = self.wins.get_mut(&id) else {
            return;
        };
        if w.parked {
            return;
        }
        let x = if scr.use_left {
            scr.park_left - w.size.width
        } else {
            scr.park_right
        };
        ax::set_point(&w.el, CGPoint::new(x, scr.park_y));
        w.parked = true;
        w.applied = None;
    }

    fn ensure_visible(&mut self, id: u32) {
        let usable = self.usable();
        let (tiles, total) = self.tiles(self.cur);
        if total <= usable {
            return;
        }
        let Some(t) = tiles.iter().find(|t| t.has(id)) else {
            return;
        };
        let ctx = &mut self.ctxs[self.cur];
        let rel = t.off - ctx.scroll;
        if t.w >= usable || rel < 0.0 {
            ctx.scroll = t.off;
        } else if rel + t.w > usable {
            ctx.scroll = t.off + t.w - usable;
        }
        self.layout_needed = true;
    }

    fn center(&mut self, id: u32) {
        let usable = self.usable();
        let (tiles, _) = self.tiles(self.cur);
        let Some(t) = tiles.iter().find(|t| t.has(id)) else {
            return;
        };
        self.ctxs[self.cur].scroll = t.off + t.w / 2.0 - usable / 2.0;
        self.layout_needed = true;
    }

    // ---------- focus ----------

    fn focus(&mut self, id: u32) {
        let Some(w) = self.wins.get(&id) else { return };
        let (pid, el, ctx) = (w.pid, w.el.clone(), w.ctx);
        let frontmost = self
            .apps
            .get(&pid)
            .is_some_and(|app| ax::names(|n| ax::set_bool(&app.el, &n.frontmost, true)));
        if !frontmost
            && let Some(r) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        {
            r.activateWithOptions(NSApplicationActivationOptions::empty());
        }
        ax::names(|n| ax::set_bool(&el, &n.main, true));
        ax::raise(&el);
        self.ctxs[ctx].focus = Some(id);
        self.touch(id);
    }

    /// Remember that `id` was just used (for picking the likely next app).
    fn touch(&mut self, id: u32) {
        self.use_tick += 1;
        self.used.insert(id, self.use_tick);
    }

    fn focus_current_or_self(&mut self) {
        let f = self.ctxs[self.cur]
            .focus
            .filter(|f| self.wins.get(f).is_some_and(|w| w.present))
            .or_else(|| self.row_ids().first().copied());
        match f {
            Some(f) => self.focus(f),
            None => ui::activate_self(self.mtm),
        }
    }

    fn step_focus(&mut self, d: i32) {
        let ids = self.row_ids();
        if ids.is_empty() {
            return;
        }
        let pos = self.ctxs[self.cur]
            .focus
            .and_then(|f| ids.iter().position(|&x| x == f));
        let j = match pos {
            Some(i) => (i as i32 + d).clamp(0, ids.len() as i32 - 1) as usize,
            None if d > 0 => 0,
            None => ids.len() - 1,
        };
        let id = ids[j];
        self.ctxs[self.cur].focus = Some(id);
        self.frame_window(id);
        self.layout();
        self.focus(id);
        self.dots_needed = true;
    }

    fn on_focus_event(&mut self, pid: i32) {
        if pid == self.own_pid || matches!(self.overlay, Overlay::Place { .. }) {
            return;
        }
        if !matches!(self.overlay, Overlay::None) {
            // You clicked into another app: the overview gets out of the way.
            self.close_overlay();
        }
        if !self.apps.contains_key(&pid) {
            self.register_app(pid);
        }
        let front = NSWorkspace::sharedWorkspace()
            .frontmostApplication()
            .map(|a| a.processIdentifier());
        if front != Some(pid) {
            return;
        }
        let Some(app_el) = self.apps.get(&pid).map(|a| a.el.clone()) else {
            return;
        };
        let Some(el) = ax::focused_window(&app_el) else {
            return;
        };
        let Some(id) = ax::window_id(&el) else { return };
        if !self.wins.contains_key(&id) {
            self.refresh_app(pid);
        }
        let Some(w) = self.wins.get(&id) else { return };
        if w.ctx == self.cur {
            self.touch(id);
            if self.ctxs[self.cur].focus != Some(id) {
                self.ctxs[self.cur].focus = Some(id);
                self.ensure_visible(id);
            }
            return;
        }
        // The wall: windows of another context can't be reached from here.
        let is_finder = self.apps.get(&pid).is_some_and(|a| a.is_finder);
        if is_finder && self.focus_is_usable() {
            return;
        }
        // macOS activated another app on its own (you closed, minimised or
        // hid the window you were using, or switched context): fix it quietly.
        if Instant::now() < self.quiet_until || !self.focus_is_usable() {
            self.focus_current_or_self();
            return;
        }
        let other = w.ctx;
        crate::log!("wall: pid {pid} window {id} is in context {other}");
        let app = self
            .apps
            .get(&pid)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        self.focus_current_or_self();
        ui::notice(
            &format!("{app} is in {}", self.ctxs[other].name),
            &format!("Press ⌃⌥{} to switch", other + 1),
        );
    }

    /// Is the current context's focused window still open, visible and alive?
    fn focus_is_usable(&self) -> bool {
        let Some(w) = self.ctxs[self.cur].focus.and_then(|f| self.wins.get(&f)) else {
            return false;
        };
        if !w.present || self.apps.get(&w.pid).is_none_or(|a| a.hidden) || !ax::is_alive(&w.el) {
            return false;
        }
        !ax::names(|n| ax::get_bool(&w.el, &n.minimized)).unwrap_or(false)
    }

    // ---------- contexts ----------

    fn switch_to(&mut self, n: usize) {
        if n >= self.ctxs.len() {
            return;
        }
        if n != self.cur {
            self.cur = n;
            self.layout();
            self.focus_current_or_self();
            self.quiet_until = Instant::now() + Duration::from_millis(700);
            self.save_needed = true;
            let names: Vec<String> = self.ctxs.iter().map(|c| c.name.clone()).collect();
            ui::rebuild_menu(&names, n);
        }
        let count = self.row_ids().len();
        ui::show_context_hud(&self.ctxs[n].name, count, n, self.ctxs.len());
    }

    // ---------- overviews ----------

    fn close_overlay(&mut self) {
        match self.overlay {
            Overlay::Apps { .. } | Overlay::Contexts { .. } => ui::hide_overview(),
            Overlay::Place { .. } => {
                ui::hide_notice();
                ui::hide_overview();
            }
            Overlay::None => {}
        }
        self.overlay = Overlay::None;
        MODE.set(Mode::Normal);
    }

    fn show_apps(&mut self) {
        let ids = self.row_ids();
        if ids.is_empty() {
            self.close_overlay();
            ui::notice(&format!("{} has no windows", self.ctxs[self.cur].name), "");
            return;
        }
        let sel = self.ctxs[self.cur]
            .focus
            .and_then(|f| ids.iter().position(|&x| x == f))
            .unwrap_or(0);
        let tiles: Vec<AppTile> = ids
            .iter()
            .filter_map(|id| self.wins.get(id))
            .map(|w| AppTile {
                pid: w.pid,
                app: self
                    .apps
                    .get(&w.pid)
                    .map(|a| a.name.clone())
                    .unwrap_or_default(),
                title: ax::names(|n| ax::get_string(&w.el, &n.title)).unwrap_or_default(),
                key: None,
                dim: false,
            })
            .collect();
        let cols = ui::show_apps(&self.ctxs[self.cur].name, &tiles, sel);
        self.overlay = Overlay::Apps { sel, ids, cols };
        MODE.set(Mode::Overview);
    }

    fn show_contexts(&mut self, sel: usize) {
        let cards: Vec<ContextCard> = self
            .ctxs
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut pids = Vec::new();
                let mut count = 0;
                for id in &c.order {
                    if let Some(w) = self.wins.get(id).filter(|w| w.present) {
                        count += 1;
                        if !pids.contains(&w.pid) {
                            pids.push(w.pid);
                        }
                    }
                }
                ContextCard {
                    name: c.name.clone(),
                    count,
                    pids,
                    current: i == self.cur,
                }
            })
            .collect();
        let cols = ui::show_contexts(&cards, sel);
        self.overlay = Overlay::Contexts { sel, cols };
        MODE.set(Mode::Overview);
    }

    fn overlay_move(&mut self, dx: i32, dy: i32) {
        let (sel, len, cols) = match &mut self.overlay {
            Overlay::Apps { sel, ids, cols } => (sel, ids.len(), *cols),
            Overlay::Contexts { sel, cols } => (sel, self.ctxs.len(), *cols),
            Overlay::Place {
                stage: PlaceStage::Apps { sel, ids, cols, .. },
                ..
            } => (sel, ids.len(), *cols),
            _ => return,
        };
        let next = *sel as i32 + dx + dy * cols as i32;
        if next >= 0 && (next as usize) < len {
            *sel = next as usize;
            ui::select_tile(*sel);
        }
    }

    fn overlay_enter(&mut self) {
        if let Overlay::Place {
            stage: PlaceStage::Apps { sel, ids, .. },
            ..
        } = &self.overlay
        {
            if let Some(&id) = ids.get(*sel) {
                self.place_pick(id);
            }
            return;
        }
        enum Target {
            Window(u32),
            Context(usize),
        }
        let target = match &self.overlay {
            Overlay::Apps { sel, ids, .. } => ids.get(*sel).copied().map(Target::Window),
            Overlay::Contexts { sel, .. } => Some(Target::Context(*sel)),
            _ => return,
        };
        self.close_overlay();
        match target {
            Some(Target::Window(id)) => {
                self.ctxs[self.cur].focus = Some(id);
                self.ensure_visible(id);
                self.layout();
                self.focus(id);
                self.dots_needed = true;
            }
            Some(Target::Context(n)) => self.switch_to(n),
            None => {}
        }
    }

    // ---------- ⌃⌥Space: pick an area, then an app ----------

    fn app_name_of(&self, id: u32) -> String {
        self.wins
            .get(&id)
            .and_then(|w| self.apps.get(&w.pid))
            .map(|a| a.name.clone())
            .unwrap_or_default()
    }

    fn group_rects(&self, group: Option<u32>) -> Vec<Rect> {
        let Some(g) = group else {
            return Vec::new();
        };
        self.ctxs[self.cur]
            .order
            .iter()
            .filter_map(|id| self.wins.get(id))
            .filter(|w| self.shown(w))
            .filter_map(|w| w.area.filter(|a| a.group == g).map(|a| a.r))
            .collect()
    }

    fn place_start(&mut self) {
        if matches!(self.overlay, Overlay::Place { .. }) {
            self.close_overlay();
            return;
        }
        self.close_overlay();
        let c = &self.ctxs[self.cur];
        let focus = c.focus.filter(|f| self.wins.contains_key(f));
        let group = focus
            .and_then(|f| self.wins.get(&f))
            .and_then(|w| w.area.map(|a| a.group));
        let anchor = focus
            .and_then(|f| c.order.iter().position(|&x| x == f))
            .unwrap_or(c.order.len());
        let snapshot = Snapshot {
            order: c.order.clone(),
            areas: c
                .order
                .iter()
                .filter_map(|id| self.wins.get(id).map(|w| (w.id, w.area)))
                .collect(),
            focus: c.focus,
        };
        self.overlay = Overlay::Place {
            group,
            anchor,
            snapshot,
            stage: PlaceStage::Map,
            // Building a new screen: the window you're in goes first. Adding to
            // an existing screen: you're choosing a new app, so ask.
            first: focus.filter(|f| {
                self.wins
                    .get(f)
                    .is_some_and(|w| self.shown(w) && w.area.is_none())
            }),
        };
        self.show_place_map();
    }

    fn show_place_map(&mut self) {
        self.show_place_map_lit(None);
    }

    /// The area map, with `lit` highlighted while arrows are held.
    fn show_place_map_lit(&mut self, lit: Option<usize>) {
        let Overlay::Place { group, first, .. } = &self.overlay else {
            return;
        };
        let title = match first {
            Some(id) => format!("Place {}", self.app_name_of(*id)),
            None if group.is_some() && layout::covers_all(&self.group_rects(*group)) => {
                "Add to this screen".to_string()
            }
            None if group.is_some() => "Fill the rest".to_string(),
            None => "Pick an area".to_string(),
        };
        let taken = self.group_rects(*group);
        let flags: [bool; 9] =
            std::array::from_fn(|i| taken.iter().any(|r| layout::overlaps(*r, AREAS[i])));
        ui::hide_overview();
        ui::show_area_map(&title, &flags, !taken.is_empty(), lit);
        MODE.set(Mode::PlaceMap);
    }

    fn place_area(&mut self, i: usize) {
        let Overlay::Place { group, first, .. } = &self.overlay else {
            return;
        };
        let (group, first) = (*group, *first);
        if i >= AREAS.len() || i == layout::MIDDLE {
            return;
        }
        // The window you're in goes to the first area: no picking needed.
        if let Some(id) = first.filter(|id| self.wins.contains_key(id)) {
            if let Overlay::Place { stage, first, .. } = &mut self.overlay {
                *first = None;
                *stage = PlaceStage::Apps {
                    area: i,
                    ids: vec![id],
                    keys: Vec::new(),
                    sel: 0,
                    cols: 1,
                };
            }
            self.place_pick(id);
            return;
        }
        let ids = self.row_ids();
        if ids.is_empty() {
            self.close_overlay();
            ui::notice(&format!("{} has no windows", self.ctxs[self.cur].name), "");
            return;
        }
        // Letters come from app names in name order, so they stay the same day to day.
        let mut by_name: Vec<(String, u32)> =
            ids.iter().map(|id| (self.app_name_of(*id), *id)).collect();
        by_name.sort();
        let names: Vec<&str> = by_name.iter().map(|(n, _)| n.as_str()).collect();
        let key_of: HashMap<u32, char> = by_name
            .iter()
            .zip(layout::letters(&names))
            .map(|((_, id), c)| (*id, c))
            .collect();
        let keys: Vec<char> = ids
            .iter()
            .map(|id| key_of.get(id).copied().unwrap_or('?'))
            .collect();
        let in_screen = |id: &u32| {
            group.is_some() && self.wins.get(id).and_then(|w| w.area).map(|a| a.group) == group
        };
        let tiles: Vec<AppTile> = ids
            .iter()
            .zip(&keys)
            .filter_map(|(id, k)| {
                let w = self.wins.get(id)?;
                Some(AppTile {
                    pid: w.pid,
                    app: self.app_name_of(*id),
                    title: ax::names(|n| ax::get_string(&w.el, &n.title)).unwrap_or_default(),
                    key: Some(*k),
                    dim: in_screen(id),
                })
            })
            .collect();
        // Pre-select the app you used most recently that isn't in this screen yet.
        let sel = ids
            .iter()
            .enumerate()
            .filter(|(_, id)| !in_screen(id))
            .max_by_key(|(_, id)| self.used.get(id).copied().unwrap_or(0))
            .map_or(0, |(i, _)| i);
        crate::log!(
            "place picker for {}: {}",
            AREA_NAMES[i],
            tiles
                .iter()
                .map(|t| format!("{}={}", t.key.unwrap_or('?'), t.app))
                .collect::<Vec<_>>()
                .join(" ")
        );
        ui::hide_notice();
        let cols = ui::show_place_apps(AREA_NAMES[i], &tiles, sel);
        if let Overlay::Place { stage, .. } = &mut self.overlay {
            *stage = PlaceStage::Apps {
                area: i,
                ids,
                keys,
                sel,
                cols,
            };
        }
        MODE.set(Mode::PlaceApps);
    }

    fn place_key(&mut self, c: char) {
        let id = match &self.overlay {
            Overlay::Place {
                stage: PlaceStage::Apps { ids, keys, .. },
                ..
            } => keys
                .iter()
                .position(|k| *k == c)
                .and_then(|i| ids.get(i))
                .copied(),
            _ => None,
        };
        if let Some(id) = id {
            self.place_pick(id);
        }
    }

    fn place_pick(&mut self, id: u32) {
        let (group, anchor, area) = match &self.overlay {
            Overlay::Place {
                group,
                anchor,
                stage: PlaceStage::Apps { area, .. },
                ..
            } => (*group, *anchor, *area),
            _ => return,
        };
        let rect = AREAS[area];
        let g = group.unwrap_or_else(|| {
            self.next_group += 1;
            self.next_group
        });
        let cur = self.cur;
        // Windows in the way make room: they shrink to what's left of their
        // area. Only a window that's completely covered leaves the screen.
        let mut moved_out = Vec::new();
        for m in self.ctxs[cur].order.clone() {
            if m == id {
                continue;
            }
            if let Some(w) = self.wins.get_mut(&m)
                && let Some(a) = w.area
                && a.group == g
                && layout::overlaps(a.r, rect)
            {
                w.applied = None;
                match layout::subtract(a.r, rect) {
                    Some(rest) => w.area = Some(Area { group: g, r: rest }),
                    None => {
                        w.area = None;
                        w.frac = 0.5;
                        moved_out.push(m);
                    }
                }
            }
        }
        if let Some(w) = self.wins.get_mut(&id) {
            w.area = Some(Area { group: g, r: rect });
            w.applied = None;
            w.full_prev = None;
        }
        // Keep the screen's windows together, at the screen's spot in the row.
        let wins = &self.wins;
        let order = &mut self.ctxs[cur].order;
        let is_member = |m: &u32| {
            wins.get(m)
                .is_some_and(|w| w.area.is_some_and(|a| a.group == g))
        };
        let spot = order
            .iter()
            .position(|m| *m != id && is_member(m))
            .unwrap_or(anchor.min(order.len()));
        let before = order[..spot].iter().filter(|m| !is_member(m)).count();
        let members: Vec<u32> = order.iter().copied().filter(|m| is_member(m)).collect();
        order.retain(|m| !is_member(m));
        let at = before.min(order.len());
        for (k, m) in members.into_iter().enumerate() {
            order.insert(at + k, m);
        }
        for w in self
            .wins
            .values_mut()
            .filter(|w| w.area.is_some_and(|a| a.group == g))
        {
            w.applied = None;
        }
        // A window pushed out sits right after the screen, not somewhere far away.
        if !moved_out.is_empty() {
            let wins = &self.wins;
            let order = &mut self.ctxs[cur].order;
            order.retain(|m| !moved_out.contains(m));
            let after = order
                .iter()
                .rposition(|m| {
                    wins.get(m)
                        .is_some_and(|w| w.area.is_some_and(|a| a.group == g))
                })
                .map_or(order.len(), |i| i + 1);
            for (k, m) in moved_out.iter().enumerate() {
                order.insert(after + k, *m);
            }
        }

        if let Overlay::Place {
            group,
            stage,
            first,
            ..
        } = &mut self.overlay
        {
            *group = Some(g);
            *stage = PlaceStage::Map;
            *first = None;
        }
        self.ctxs[cur].focus = Some(id);
        self.ensure_visible(id);
        self.layout();
        self.focus(id);
        self.save_needed = true;
        let names: Vec<String> = moved_out.iter().map(|m| self.app_name_of(*m)).collect();
        if layout::covers_all(&self.group_rects(Some(g))) {
            self.close_overlay();
            if names.is_empty() {
                ui::notice("Screen ready", "Press ⌃⌥Space on it to rearrange");
            } else {
                ui::notice(
                    &format!("{} moved next to this screen", names.join(", ")),
                    "It was completely covered",
                );
            }
        } else {
            self.show_place_map();
            if !names.is_empty() {
                crate::log!("place: {} moved out of the screen", names.join(", "));
            }
        }
    }

    /// ⌃⌥Return on a window in a screen: take it out, full width, right after
    /// the screen. ⌃⌥Return again: back to its spot. Returns false when the
    /// window has nothing to do with screens (normal full view applies).
    fn toggle_out_of_screen(&mut self, id: u32) -> bool {
        let cur = self.cur;
        let Some(w) = self.wins.get(&id) else {
            return false;
        };
        if let Some(a) = w.area {
            // Out of the screen, as its own full-width stop next to it.
            if let Some(w) = self.wins.get_mut(&id) {
                w.left_area = Some(a);
                w.area = None;
                w.full_prev = Some(w.frac);
                w.frac = 1.0;
                w.applied = None;
            }
            let wins = &self.wins;
            let order = &mut self.ctxs[cur].order;
            if let Some(i) = order.iter().position(|&x| x == id) {
                order.remove(i);
            }
            let after = order
                .iter()
                .rposition(|m| {
                    wins.get(m)
                        .is_some_and(|w| w.area.is_some_and(|x| x.group == a.group))
                })
                .map_or(order.len(), |i| i + 1);
            order.insert(after, id);
            for w in self
                .wins
                .values_mut()
                .filter(|w| w.area.is_some_and(|x| x.group == a.group))
            {
                w.applied = None;
            }
            self.ensure_visible(id);
            self.layout_needed = true;
            self.save_needed = true;
            ui::notice("Out of the screen, full width", "⌃⌥Return puts it back");
            return true;
        }
        let Some(a) = w.left_area else {
            return false;
        };
        // Back to its spot, if nothing has taken it meanwhile.
        let taken = self
            .group_rects(Some(a.group))
            .iter()
            .any(|r| layout::overlaps(*r, a.r));
        if let Some(w) = self.wins.get_mut(&id) {
            w.left_area = None;
            if taken {
                return false;
            }
            w.area = Some(a);
            w.frac = w.full_prev.take().unwrap_or(0.5);
            w.applied = None;
        }
        let wins = &self.wins;
        let order = &mut self.ctxs[cur].order;
        let was = order.iter().position(|&x| x == id);
        if let Some(i) = was {
            order.remove(i);
        }
        // Rejoin the screen's block of windows; if they're all gone, stay put.
        let at = order
            .iter()
            .position(|m| {
                wins.get(m)
                    .is_some_and(|w| w.area.is_some_and(|x| x.group == a.group))
            })
            .unwrap_or(was.unwrap_or(order.len()).min(order.len()));
        order.insert(at, id);
        for w in self
            .wins
            .values_mut()
            .filter(|w| w.area.is_some_and(|x| x.group == a.group))
        {
            w.applied = None;
        }
        self.ensure_visible(id);
        self.layout_needed = true;
        self.save_needed = true;
        true
    }

    /// ⌃⌥Space, then ⌫: the screen splits back into separate windows in the row.
    fn place_ungroup(&mut self) {
        let Overlay::Place { group: Some(g), .. } = self.overlay else {
            return;
        };
        self.close_overlay();
        let mut n = 0;
        for w in self
            .wins
            .values_mut()
            .filter(|w| w.area.is_some_and(|a| a.group == g))
        {
            w.area = None;
            w.left_area = None;
            w.frac = 0.5;
            w.applied = None;
            n += 1;
        }
        if let Some(f) = self.ctxs[self.cur].focus {
            self.ensure_visible(f);
        }
        self.layout_needed = true;
        self.save_needed = true;
        ui::notice(
            &format!("Screen split into {n} windows"),
            "Each is its own stop in the row again",
        );
    }

    /// Esc: put everything back exactly as it was before ⌃⌥Space.
    fn place_cancel(&mut self) {
        let Overlay::Place { snapshot, .. } = std::mem::replace(&mut self.overlay, Overlay::None)
        else {
            return;
        };
        ui::hide_notice();
        ui::hide_overview();
        MODE.set(Mode::Normal);
        let cur = self.cur;
        // Keep any window that opened meanwhile; drop any that closed.
        let mut order: Vec<u32> = snapshot
            .order
            .iter()
            .copied()
            .filter(|id| self.wins.contains_key(id))
            .collect();
        for id in &self.ctxs[cur].order {
            if !order.contains(id) {
                order.push(*id);
            }
        }
        self.ctxs[cur].order = order;
        for (id, area) in snapshot.areas {
            if let Some(w) = self.wins.get_mut(&id) {
                w.area = area;
            }
        }
        for w in self.wins.values_mut().filter(|w| w.ctx == cur) {
            w.applied = None;
        }
        self.ctxs[cur].focus = snapshot.focus.filter(|f| self.wins.contains_key(f));
        if let Some(f) = self.ctxs[cur].focus {
            self.ensure_visible(f);
        }
        self.layout();
        self.focus_current_or_self();
        self.save_needed = true;
    }

    // ---------- commands ----------

    /// Picks up windows that appeared without an Accessibility event, such as
    /// windows arriving from another desktop. One cheap WindowServer query;
    /// apps are only asked when an unknown window is actually on screen.
    fn scan(&mut self) {
        let mut pids = Vec::new();
        for (id, pid) in crate::spaces::onscreen_windows() {
            if pid == self.own_pid || self.wins.contains_key(&id) || self.ignored.contains(&id) {
                continue;
            }
            if !pids.contains(&pid) {
                pids.push(pid);
            }
            self.ignored.insert(id);
        }
        for pid in pids {
            if self.apps.contains_key(&pid) {
                self.refresh_app(pid);
            } else {
                self.register_app(pid);
            }
        }
        // Forget windows that became managed or went away, so the set stays small.
        let wins = &self.wins;
        self.ignored.retain(|id| !wins.contains_key(id));
        if self.ignored.len() > 4096 {
            self.ignored.clear();
        }
    }

    fn rescan(&mut self) {
        let apps = NSWorkspace::sharedWorkspace().runningApplications();
        for app in apps.iter() {
            if app.activationPolicy() == NSApplicationActivationPolicy::Regular {
                let pid = app.processIdentifier();
                if self.apps.contains_key(&pid) {
                    self.refresh_app(pid);
                } else {
                    self.register_app(pid);
                }
            }
        }
    }

    fn handle(&mut self, c: Cmd) {
        if !matches!(
            c,
            Cmd::Scroll(_)
                | Cmd::ScrollEnd
                | Cmd::AppChanged(_)
                | Cmd::FocusChanged(_)
                | Cmd::Activated(_)
                | Cmd::Resized(_)
                | Cmd::MouseDown(..)
                | Cmd::Scan
                | Cmd::PlaceHover(_)
        ) {
            crate::log!("command: {c:?}");
        }
        // While an overview is open, the row stays still.
        let overlay_open = !matches!(self.overlay, Overlay::None);
        match c {
            Cmd::FocusPrev
            | Cmd::FocusNext
            | Cmd::Wider
            | Cmd::Narrower
            | Cmd::Center
            | Cmd::FullView
            | Cmd::Scroll(_)
                if overlay_open =>
            {
                return;
            }
            _ => {}
        }
        match c {
            Cmd::FocusPrev => self.step_focus(-1),
            Cmd::FocusNext => self.step_focus(1),
            Cmd::Wider | Cmd::Narrower | Cmd::FullView => {
                let Some(f) = self.ctxs[self.cur].focus else {
                    return;
                };
                if c == Cmd::FullView && self.toggle_out_of_screen(f) {
                    return;
                }
                let Some(w) = self.wins.get_mut(&f) else {
                    return;
                };
                if w.area.is_some() {
                    ui::notice(
                        "This window shares a screen",
                        "⌃⌥Return takes it out full width · ⌃⌥Space rearranges",
                    );
                    return;
                }
                w.frac = match c {
                    Cmd::Wider => PRESETS
                        .iter()
                        .copied()
                        .find(|p| *p > w.frac + 0.01)
                        .unwrap_or(1.0),
                    Cmd::Narrower => PRESETS
                        .iter()
                        .rev()
                        .copied()
                        .find(|p| *p < w.frac - 0.01)
                        .unwrap_or(PRESETS[0]),
                    _ => match w.full_prev.take() {
                        Some(p) => p,
                        None => {
                            w.full_prev = Some(w.frac);
                            1.0
                        }
                    },
                };
                if c != Cmd::FullView {
                    w.full_prev = None;
                }
                w.min_w = 0.0;
                self.ensure_visible(f);
                self.layout_needed = true;
                self.save_needed = true;
            }
            Cmd::Center => {
                if let Some(f) = self.ctxs[self.cur].focus {
                    self.center(f);
                }
            }
            Cmd::SnapBack => {
                // Also pick up windows that arrived without telling us
                // (for example from a desktop you just removed).
                self.rescan();
                let cur = self.cur;
                for w in self.wins.values_mut().filter(|w| w.ctx == cur) {
                    w.applied = None;
                }
                self.layout_needed = true;
                check_desktops();
            }
            Cmd::ContextPrev | Cmd::ContextNext => {
                let n = if c == Cmd::ContextPrev {
                    self.cur.checked_sub(1)
                } else {
                    Some(self.cur + 1).filter(|n| *n < self.ctxs.len())
                };
                match n {
                    Some(n) => self.switch_to(n),
                    None => ui::notice(
                        &format!(
                            "{} is the {} context",
                            self.ctxs[self.cur].name,
                            if c == Cmd::ContextPrev {
                                "first"
                            } else {
                                "last"
                            }
                        ),
                        "",
                    ),
                }
            }
            Cmd::GoTo(n) => {
                if overlay_open {
                    self.close_overlay();
                }
                self.switch_to(n);
            }
            Cmd::OvClose => self.close_overlay(),
            Cmd::ShowApps => match self.overlay {
                Overlay::Apps { .. } => self.close_overlay(),
                _ => {
                    self.close_overlay();
                    self.show_apps();
                }
            },
            Cmd::ShowContexts => match self.overlay {
                Overlay::Contexts { .. } => self.close_overlay(),
                _ => {
                    self.close_overlay();
                    self.show_contexts(self.cur);
                }
            },
            Cmd::ZoomOut => match self.overlay {
                Overlay::None => self.show_apps(),
                Overlay::Apps { .. } => {
                    self.close_overlay();
                    self.show_contexts(self.cur);
                }
                _ => {}
            },
            Cmd::ZoomIn => match self.overlay {
                Overlay::Contexts { sel, .. } => {
                    self.close_overlay();
                    self.switch_to(sel);
                    self.show_apps();
                }
                Overlay::Apps { .. } => self.overlay_enter(),
                _ => {}
            },
            Cmd::OvMove(dx, dy) => self.overlay_move(dx, dy),
            Cmd::OvEnter => self.overlay_enter(),
            Cmd::OvPick(i) => {
                match &mut self.overlay {
                    Overlay::Apps { sel, .. }
                    | Overlay::Contexts { sel, .. }
                    | Overlay::Place {
                        stage: PlaceStage::Apps { sel, .. },
                        ..
                    } => *sel = i,
                    _ => return,
                }
                self.overlay_enter();
            }
            Cmd::Scroll(d) => {
                self.scrolling = true;
                self.ctxs[self.cur].scroll += d;
                self.layout_needed = true;
                self.dots_needed = true;
                ui::schedule_scroll_end();
            }
            Cmd::ScrollEnd => {
                self.scrolling = false;
                self.after_scroll();
                self.layout_needed = true;
            }
            Cmd::AppChanged(pid) => {
                if self.apps.contains_key(&pid) {
                    self.refresh_app(pid);
                } else {
                    self.register_app(pid);
                }
            }
            Cmd::FocusChanged(pid) | Cmd::Activated(pid) => self.on_focus_event(pid),
            Cmd::Scan => self.scan(),
            Cmd::PlaceStart => self.place_start(),
            Cmd::PlaceArea(i) => self.place_area(i),
            Cmd::PlaceKey(c) => self.place_key(c),
            Cmd::PlaceHover(lit) => {
                if let Overlay::Place {
                    stage: PlaceStage::Map,
                    ..
                } = self.overlay
                {
                    self.show_place_map_lit(lit);
                }
            }
            Cmd::PlaceBack => {
                if let Overlay::Place { stage, .. } = &mut self.overlay {
                    *stage = PlaceStage::Map;
                    self.show_place_map();
                }
            }
            Cmd::PlaceDone => self.close_overlay(),
            Cmd::PlaceCancel => self.place_cancel(),
            Cmd::PlaceUngroup => self.place_ungroup(),
            Cmd::MouseDown(x, y) => {
                let inside = matches!(
                    self.overlay,
                    Overlay::Apps { .. }
                        | Overlay::Contexts { .. }
                        | Overlay::Place {
                            stage: PlaceStage::Apps { .. },
                            ..
                        }
                ) && ui::overview_contains(x, y);
                if !inside {
                    self.close_overlay();
                }
            }
            Cmd::Resized(id) => self.on_resized(id),
            Cmd::AppLaunched(pid) => {
                self.register_app(pid);
                ui::schedule_refresh(pid);
            }
            Cmd::AppTerminated(pid) => {
                self.apps.remove(&pid);
                let ids: Vec<u32> = self
                    .wins
                    .values()
                    .filter(|w| w.pid == pid)
                    .map(|w| w.id)
                    .collect();
                for id in ids {
                    self.remove_window(id);
                }
                self.layout_needed = true;
                self.save_needed = true;
            }
            Cmd::AppHidden(pid, hidden) => {
                if let Some(a) = self.apps.get_mut(&pid) {
                    a.hidden = hidden;
                    self.layout_needed = true;
                }
            }
            Cmd::ScreensChanged => {
                self.scr = read_screen(self.mtm);
                for w in self.wins.values_mut() {
                    w.applied = None;
                    w.parked = false;
                }
                self.layout_needed = true;
            }
            Cmd::Quit => {
                self.close_overlay();
                RESTORED.set(true);
                self.save();
                self.restore_all();
                QUIT.set(true);
            }
        }
    }

    /// After a ⌃⌥-scroll ends, keep keyboard focus on a window you can see.
    /// When you let go after ⌃⌥-scrolling: the most visible app becomes the
    /// main one, gets focus, and the row slides so it's framed (no slivers).
    fn after_scroll(&mut self) {
        let usable = self.usable();
        let (tiles, _) = self.tiles(self.cur);
        if tiles.is_empty() {
            return;
        }
        let scroll = self.ctxs[self.cur].scroll;
        let focus = self.ctxs[self.cur].focus;
        let main = (0..tiles.len())
            .max_by(|&a, &b| {
                let key = |i: usize| {
                    let t = &tiles[i];
                    (
                        layout::visible(t.off, t.w, scroll, usable).round() as i64,
                        focus.is_some_and(|f| t.has(f)),
                    )
                };
                key(a).cmp(&key(b))
            })
            .unwrap_or(0);
        self.frame(&tiles, main);
        let has_focus = focus.is_some_and(|f| tiles[main].has(f));
        if !has_focus && let Some(&(id, _)) = tiles[main].ids.first() {
            self.focus(id);
        }
    }

    /// Slide the row (never resize) so `tiles[main]` is framed.
    fn frame(&mut self, tiles: &[Tile], main: usize) {
        let usable = self.usable();
        let stops: Vec<(f64, f64)> = tiles.iter().map(|t| (t.off, t.w)).collect();
        let sliver = (self.scr.w * 0.2).max(160.0);
        let ctx = &mut self.ctxs[self.cur];
        let target = layout::frame_scroll(&stops, main, ctx.scroll, usable, sliver);
        if (target - ctx.scroll).abs() > 0.5 {
            ctx.scroll = target;
        }
        self.layout_needed = true;
    }

    /// Bring a window into view and frame it (used by ⌃⌥← / ⌃⌥→).
    fn frame_window(&mut self, id: u32) {
        self.ensure_visible(id);
        let (tiles, _) = self.tiles(self.cur);
        if let Some(i) = tiles.iter().position(|t| t.has(id)) {
            self.frame(&tiles, i);
        }
    }

    fn on_resized(&mut self, id: u32) {
        let usable = self.usable();
        let cur = self.cur;
        let Some(w) = self.wins.get_mut(&id) else {
            return;
        };
        if w.ctx != cur || w.parked || !w.present || w.area.is_some() {
            return;
        }
        let Some(applied) = w.applied else { return };
        // Resize events caused by Lanes itself arrive just after it sets a frame.
        if w.touched.elapsed() < Duration::from_millis(800) {
            return;
        }
        let Some(size) = ax::get_size(&w.el) else {
            return;
        };
        // Apps like Terminal snap to whole characters, so allow a little slack
        // against both the width Lanes asked for and the width the app chose.
        let ours =
            (size.width - applied.w).abs() <= 16.0 || (size.width - w.size.width).abs() <= 2.0;
        w.size = size;
        if ours {
            return;
        }
        crate::log!("resize: window {id} resized by hand to {:.0}", size.width);
        // You resized it by hand: keep that width and make room for it.
        w.frac = ((size.width + GAP) / (usable + GAP)).clamp(0.2, 1.0);
        w.min_w = 0.0;
        w.full_prev = None;
        w.applied = None;
        self.layout_needed = true;
        self.save_needed = true;
    }

    fn flush(&mut self) {
        for _ in 0..3 {
            if self.layout_needed {
                self.layout();
            }
        }
        if self.dots_needed {
            self.dots_needed = false;
            let ids = self.row_ids();
            if ids.len() > 1 {
                let i = self.ctxs[self.cur]
                    .focus
                    .and_then(|f| ids.iter().position(|&x| x == f));
                ui::show_dots(ids.len(), i);
            }
        }
        if self.save_needed {
            self.save_needed = false;
            ui::schedule_save();
        }
    }

    fn save(&self) {
        let mut items = Vec::new();
        for (ci, c) in self.ctxs.iter().enumerate() {
            for (oi, id) in c.order.iter().enumerate() {
                let Some(w) = self.wins.get(id) else { continue };
                items.push((
                    *id,
                    Saved {
                        pid: w.pid,
                        app: self
                            .apps
                            .get(&w.pid)
                            .map(|a| a.name.clone())
                            .unwrap_or_default(),
                        ctx: ci,
                        frac: w.frac,
                        order: oi,
                        area: w.area.map(|a| (a.group, a.r)),
                    },
                ));
            }
        }
        store::save_state(self.cur, &items);
    }

    /// Bring every window fully back on screen (used when Lanes quits).
    fn restore_all(&mut self) {
        let scr = self.scr;
        let mut n = 0.0;
        let ids: Vec<u32> = self
            .ctxs
            .iter()
            .flat_map(|c| c.order.iter().copied())
            .collect();
        for id in ids {
            let Some(w) = self.wins.get_mut(&id) else {
                continue;
            };
            if !w.present {
                continue;
            }
            let size = ax::get_size(&w.el).unwrap_or(w.size);
            let wd = size.width.min(scr.w);
            let ht = size.height.min(scr.h);
            let pos = ax::get_point(&w.el).unwrap_or(CGPoint::new(scr.x, scr.y));
            let on_screen = !w.parked && pos.x >= scr.x - 1.0 && pos.x + wd <= scr.x + scr.w + 1.0;
            if on_screen {
                continue;
            }
            let x = if w.parked {
                let step = (n * 28.0) % 280.0;
                n += 1.0;
                scr.x + ((scr.w - wd) / 2.0).max(0.0) - 140.0 + step
            } else {
                pos.x.clamp(scr.x, scr.x + scr.w - wd)
            };
            let y = if w.parked {
                scr.y + ((scr.h - ht) / 2.0).max(0.0)
            } else {
                pos.y
            };
            ax::set_size(&w.el, CGSize::new(wd, ht));
            ax::set_point(&w.el, CGPoint::new(x.max(scr.x), y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_snap_to_presets() {
        assert_eq!(snap_frac(0.49), 0.5);
        assert_eq!(snap_frac(0.68), 2.0 / 3.0);
        assert_eq!(snap_frac(0.99), 1.0);
        assert_eq!(snap_frac(0.1), 0.25);
        assert_eq!(snap_frac(1.7), 1.0);
        assert!((snap_frac(0.6) - 0.6).abs() < 1e-9);
    }

    #[test]
    fn only_main_display_windows_are_managed() {
        let s = Screen {
            full: (0.0, 0.0, 1440.0, 900.0),
            ..Default::default()
        };
        assert!(s.overlaps(CGPoint::new(100.0, 100.0), CGSize::new(800.0, 600.0)));
        // Parked in the corner by Lanes: 1px still overlaps.
        assert!(s.overlaps(CGPoint::new(1439.0, 899.0), CGSize::new(800.0, 600.0)));
        // A window on a display to the right.
        assert!(!s.overlaps(CGPoint::new(1500.0, 100.0), CGSize::new(800.0, 600.0)));
    }

    #[test]
    fn scroll_commands_merge_in_the_queue() {
        QUEUE.with_borrow_mut(|q| q.clear());
        QUEUE.with_borrow_mut(|q| {
            q.push(Cmd::Scroll(10.0));
            if let Some(Cmd::Scroll(p)) = q.last_mut() {
                *p += 5.0;
            }
        });
        assert_eq!(QUEUE.with_borrow(|q| q.clone()), vec![Cmd::Scroll(15.0)]);
    }
}
