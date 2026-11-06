//! Native macOS interface: menu bar icon, HUDs, position dots and overviews.
//!
//! Everything uses Apple's own controls and system colours, so Lanes follows
//! Light/Dark mode, the accent colour and Reduce Transparency automatically.
//! Panels are created on first use and their contents are rebuilt only when
//! shown, so nothing is drawn while you work.

use std::cell::{Cell, OnceCell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAnimationContext, NSApplication, NSApplicationDidChangeScreenParametersNotification,
    NSApplicationWillTerminateNotification, NSBackingStoreType, NSButton, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSEventModifierFlags, NSFont, NSFontWeightBold,
    NSFontWeightMedium, NSFontWeightRegular, NSFontWeightSemibold, NSImage, NSImageScaling,
    NSImageSymbolConfiguration, NSImageView, NSLineBreakMode, NSMenu, NSMenuItem, NSPanel,
    NSPopUpMenuWindowLevel, NSRunningApplication, NSScreen, NSStatusBar, NSStatusItem,
    NSTextAlignment, NSTextField, NSView, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
    NSVisualEffectState, NSVisualEffectView, NSWindowCollectionBehavior, NSWindowStyleMask,
    NSWorkspace, NSWorkspaceActiveSpaceDidChangeNotification, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification, NSWorkspaceDidHideApplicationNotification,
    NSWorkspaceDidLaunchApplicationNotification, NSWorkspaceDidTerminateApplicationNotification,
    NSWorkspaceDidUnhideApplicationNotification,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_foundation::{
    NSArray, NSDictionary, NSNotification, NSNotificationCenter, NSNumber, NSRunLoopCommonModes,
    NSString, NSURL, NSUserDefaults,
};

use crate::store;
use crate::wm::{self, Cmd};

pub struct AppTile {
    pub pid: i32,
    pub app: String,
    pub title: String,
    /// Letter to press for this app (⌃⌥Space picker), shown as a badge.
    pub key: Option<char>,
    /// Already in this screen: shown faded.
    pub dim: bool,
}

pub struct ContextCard {
    pub name: String,
    pub count: usize,
    pub pids: Vec<i32>,
    pub current: bool,
}

// Menu item tags.
const TAG_APPS: isize = 20;
const TAG_CONTEXTS: isize = 21;
const TAG_SNAP: isize = 22;
const TAG_EDIT_CONTEXTS: isize = 23;
const TAG_ACCESSIBILITY: isize = 30;
const TAG_QUIT: isize = 31;
const TAG_DESKTOPS: isize = 32;
const TAG_LOG: isize = 33;
const TAG_FOLLOW: isize = 34;
const FOLLOW_OFF_KEY: &str = "focusFollowsMouseOff";

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "LanesController"]
    pub struct Controller;

    impl Controller {
        #[unsafe(method(drain:))]
        fn drain(&self, _o: Option<&AnyObject>) {
            DRAIN_SCHEDULED.set(false);
            wm::drain();
        }

        #[unsafe(method(menuAction:))]
        fn menu_action(&self, sender: &NSMenuItem) {
            match sender.tag() {
                t @ 1..=9 => wm::push(Cmd::GoTo(t as usize - 1)),
                TAG_APPS => wm::push(Cmd::ShowApps),
                TAG_CONTEXTS => wm::push(Cmd::ShowContexts),
                TAG_SNAP => wm::push(Cmd::SnapBack),
                TAG_EDIT_CONTEXTS => edit_contexts(),
                TAG_ACCESSIBILITY => open_accessibility_settings(),
                TAG_DESKTOPS => open_mission_control(),
                TAG_LOG => open_log(),
                TAG_FOLLOW => {
                    let on = !follow_mouse();
                    FOLLOW.set(on);
                    let defaults = NSUserDefaults::standardUserDefaults();
                    defaults.setBool_forKey(!on, &NSString::from_str(FOLLOW_OFF_KEY));
                    let (names, cur) = UI.with_borrow(|u| match u {
                        Some(u) => (u.names.clone(), u.cur),
                        None => (Vec::new(), 0),
                    });
                    rebuild_menu(&names, cur);
                }
                TAG_QUIT => {
                    if wm::mode() == wm::Mode::Off {
                        terminate();
                    } else {
                        wm::push(Cmd::Quit);
                    }
                }
                _ => {}
            }
        }

        #[unsafe(method(tileClicked:))]
        fn tile_clicked(&self, sender: &NSButton) {
            wm::push(Cmd::OvPick(sender.tag().max(0) as usize));
        }

        #[unsafe(method(appActivated:))]
        fn app_activated(&self, n: &NSNotification) {
            if let Some(pid) = pid_of(n) {
                wm::push(Cmd::Activated(pid));
            }
        }

        #[unsafe(method(appLaunched:))]
        fn app_launched(&self, n: &NSNotification) {
            if let Some(pid) = pid_of(n) {
                wm::push(Cmd::AppLaunched(pid));
            }
        }

        #[unsafe(method(appTerminated:))]
        fn app_terminated(&self, n: &NSNotification) {
            if let Some(pid) = pid_of(n) {
                wm::push(Cmd::AppTerminated(pid));
            }
        }

        #[unsafe(method(appHidden:))]
        fn app_hidden(&self, n: &NSNotification) {
            if let Some(pid) = pid_of(n) {
                wm::push(Cmd::AppHidden(pid, true));
            }
        }

        #[unsafe(method(appUnhidden:))]
        fn app_unhidden(&self, n: &NSNotification) {
            if let Some(pid) = pid_of(n) {
                wm::push(Cmd::AppHidden(pid, false));
            }
        }

        #[unsafe(method(scanTick:))]
        fn scan_tick(&self, _o: Option<&AnyObject>) {
            wm::push(Cmd::Scan);
            perform(sel!(scanTick:), None, 2.0);
        }

        #[unsafe(method(spaceChanged:))]
        fn space_changed(&self, _n: &NSNotification) {
            wm::push(Cmd::Scan);
        }

        #[unsafe(method(screensChanged:))]
        fn screens_changed(&self, _n: &NSNotification) {
            wm::push(Cmd::ScreensChanged);
        }

        #[unsafe(method(willTerminate:))]
        fn will_terminate(&self, _n: &NSNotification) {
            wm::restore_now();
        }

        #[unsafe(method(refreshPid:))]
        fn refresh_pid(&self, n: &NSNumber) {
            wm::push(Cmd::AppChanged(n.intValue()));
        }

        #[unsafe(method(activateFocus:))]
        fn activate_focus(&self, _o: Option<&AnyObject>) {
            wm::push(Cmd::Activate);
        }

        #[unsafe(method(hoverCommit:))]
        fn hover_commit(&self, _o: Option<&AnyObject>) {
            let (x, y) = HOVER_AT.get();
            wm::push(Cmd::HoverCommit(x, y));
        }

        #[unsafe(method(glideTick:))]
        fn glide_tick(&self, _o: Option<&AnyObject>) {
            wm::push(Cmd::GlideTick);
        }

        #[unsafe(method(scrollEnded:))]
        fn scroll_ended(&self, _o: Option<&AnyObject>) {
            wm::push(Cmd::ScrollEnd);
        }

        #[unsafe(method(saveState:))]
        fn save_state(&self, _o: Option<&AnyObject>) {
            wm::save_now();
        }

        #[unsafe(method(trustPoll:))]
        fn trust_poll(&self, _o: Option<&AnyObject>) {
            wm::trust_poll(self.mtm());
        }

        #[unsafe(method(hideHud:))]
        fn hide_hud(&self, _o: Option<&AnyObject>) {
            UI.with_borrow(|u| {
                if let Some(u) = u
                    && let Some(p) = &u.hud {
                        fade_out(p, sel!(hudGone:));
                    }
            });
        }

        #[unsafe(method(hudGone:))]
        fn hud_gone(&self, _o: Option<&AnyObject>) {
            UI.with_borrow(|u| {
                if let Some(p) = u.as_ref().and_then(|u| u.hud.as_ref()) {
                    p.orderOut(None);
                }
            });
        }

        #[unsafe(method(hideDots:))]
        fn hide_dots(&self, _o: Option<&AnyObject>) {
            UI.with_borrow(|u| {
                if let Some(p) = u.as_ref().and_then(|u| u.dots.as_ref()) {
                    fade_out(p, sel!(dotsGone:));
                }
            });
        }

        #[unsafe(method(dotsGone:))]
        fn dots_gone(&self, _o: Option<&AnyObject>) {
            UI.with_borrow(|u| {
                if let Some(p) = u.as_ref().and_then(|u| u.dots.as_ref()) {
                    p.orderOut(None);
                }
            });
        }
    }
);

impl Controller {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

struct Ui {
    mtm: MainThreadMarker,
    item: Retained<NSStatusItem>,
    hud: Option<Retained<NSPanel>>,
    dots: Option<Retained<NSPanel>>,
    outline: Option<Retained<NSPanel>>,
    overview: Option<Retained<NSPanel>>,
    tiles: Vec<Retained<NSView>>,
    dots_key: Option<(usize, Option<usize>)>,
    trusted: bool,
    names: Vec<String>,
    /// Desktops on the main display, when there is more than one.
    desktops: Option<(usize, usize)>,
    cur: usize,
}

thread_local! {
    static CTRL: OnceCell<Retained<Controller>> = const { OnceCell::new() };
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
    static DRAIN_SCHEDULED: Cell<bool> = const { Cell::new(false) };
    static FOLLOW: Cell<bool> = const { Cell::new(true) };
    static HOVER_AT: Cell<(f64, f64)> = const { Cell::new((0.0, 0.0)) };
}

fn ctrl() -> Option<Retained<Controller>> {
    CTRL.with(|c| c.get().cloned())
}

fn pid_of(n: &NSNotification) -> Option<i32> {
    let info: Retained<NSDictionary> = n.userInfo()?;
    let key: &NSString = unsafe { NSWorkspaceApplicationKey };
    let app: Option<Retained<AnyObject>> = unsafe { msg_send![&*info, objectForKey: key] };
    let app = app?;
    let app: &NSRunningApplication = app.downcast_ref()?;
    Some(app.processIdentifier())
}

/// Run `sel` on the controller after `delay`, also while menus are open.
fn perform(sel: Sel, obj: Option<&AnyObject>, delay: f64) {
    let Some(c) = ctrl() else { return };
    let mode: &NSString = unsafe { NSRunLoopCommonModes };
    let modes = NSArray::from_slice(&[mode]);
    unsafe {
        let _: () = msg_send![&*c, performSelector: sel, withObject: obj, afterDelay: delay, inModes: &*modes];
    }
}

fn cancel(sel: Sel) {
    let Some(c) = ctrl() else { return };
    unsafe {
        let _: () = msg_send![
            NSObject::class(),
            cancelPreviousPerformRequestsWithTarget: &*c,
            selector: sel,
            object: Option::<&AnyObject>::None
        ];
    }
}

fn debounce(sel: Sel, delay: f64) {
    cancel(sel);
    perform(sel, None, delay);
}

pub fn schedule_drain() {
    if !DRAIN_SCHEDULED.replace(true) {
        perform(sel!(drain:), None, 0.0);
    }
}

pub fn schedule_save() {
    debounce(sel!(saveState:), 1.0);
}

pub fn schedule_trust_poll() {
    debounce(sel!(trustPoll:), 1.0);
}

/// Next frame of a glide, about 60 times a second.
/// Focus follows the mouse (on unless turned off in the menu).
pub fn follow_mouse() -> bool {
    FOLLOW.get()
}

/// The pointer moved: check what's under it once it has rested 0.15 s.
pub fn schedule_hover_commit(x: f64, y: f64, delay: f64) {
    HOVER_AT.set((x, y));
    debounce(sel!(hoverCommit:), delay);
}

/// Activate the focused window's app shortly, once the pointer settles.
pub fn schedule_activate(delay: f64) {
    debounce(sel!(activateFocus:), delay);
}

pub fn cancel_hover_commit() {
    cancel(sel!(hoverCommit:));
}

pub fn schedule_glide_tick() {
    perform(sel!(glideTick:), None, 1.0 / 60.0);
}

pub fn schedule_scroll_end() {
    debounce(sel!(scrollEnded:), 0.25);
}

pub fn schedule_refresh(pid: i32) {
    let n = NSNumber::new_i32(pid);
    perform(sel!(refreshPid:), Some(&n), 0.8);
}

pub fn terminate() {
    if let Some(mtm) = MainThreadMarker::new() {
        NSApplication::sharedApplication(mtm).terminate(None);
    }
}

pub fn activate_self(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    unsafe {
        let _: () = msg_send![&*app, activateIgnoringOtherApps: true];
    }
}

fn open_url(s: &str) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(s)) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

fn open_accessibility_settings() {
    open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility");
}

fn edit_contexts() {
    let path = store::contexts_path();
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(
            &path,
            "# One context per line (up to 9). Quit and reopen Lanes after saving.\nWork\nPersonal\n",
        );
    }
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    NSWorkspace::sharedWorkspace().openURL(&url);
    notice(
        "Edit the list, then quit and reopen Lanes",
        "One context name per line",
    );
}

// ---------------------------------------------------------------- setup

pub fn init(mtm: MainThreadMarker) {
    let off =
        NSUserDefaults::standardUserDefaults().boolForKey(&NSString::from_str(FOLLOW_OFF_KEY));
    FOLLOW.set(!off);
    let c = Controller::new(mtm);
    CTRL.with(|cell| {
        let _ = cell.set(c);
    });

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(-1.0);
    if let Some(button) = item.button(mtm) {
        let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("rectangle.split.3x1"),
            Some(&NSString::from_str("Lanes")),
        );
        if let Some(img) = img {
            img.setTemplate(true);
            button.setImage(Some(&img));
        }
        button.setToolTip(Some(&NSString::from_str("Lanes")));
    }
    UI.with_borrow_mut(|u| {
        *u = Some(Ui {
            mtm,
            item,
            hud: None,
            dots: None,
            outline: None,
            overview: None,
            tiles: Vec::new(),
            dots_key: None,
            trusted: true,
            names: Vec::new(),
            desktops: None,
            cur: 0,
        })
    });
    rebuild_menu(&[], 0);

    // Put windows back if the Mac logs out or Lanes is quit from Activity Monitor.
    let Some(c) = ctrl() else { return };
    let center = NSNotificationCenter::defaultCenter();
    unsafe {
        center.addObserver_selector_name_object(
            &c,
            sel!(willTerminate:),
            Some(NSApplicationWillTerminateNotification),
            None,
        );
        center.addObserver_selector_name_object(
            &c,
            sel!(screensChanged:),
            Some(NSApplicationDidChangeScreenParametersNotification),
            None,
        );
    }
}

pub fn observe_workspace() {
    let Some(c) = ctrl() else { return };
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    unsafe {
        center.addObserver_selector_name_object(
            &c,
            sel!(appActivated:),
            Some(NSWorkspaceDidActivateApplicationNotification),
            None,
        );
        center.addObserver_selector_name_object(
            &c,
            sel!(appLaunched:),
            Some(NSWorkspaceDidLaunchApplicationNotification),
            None,
        );
        center.addObserver_selector_name_object(
            &c,
            sel!(appTerminated:),
            Some(NSWorkspaceDidTerminateApplicationNotification),
            None,
        );
        center.addObserver_selector_name_object(
            &c,
            sel!(appHidden:),
            Some(NSWorkspaceDidHideApplicationNotification),
            None,
        );
        center.addObserver_selector_name_object(
            &c,
            sel!(appUnhidden:),
            Some(NSWorkspaceDidUnhideApplicationNotification),
            None,
        );
        center.addObserver_selector_name_object(
            &c,
            sel!(spaceChanged:),
            Some(NSWorkspaceActiveSpaceDidChangeNotification),
            None,
        );
    }
    perform(sel!(scanTick:), None, 2.0);
}

pub fn set_trusted(trusted: bool) {
    let (names, cur) = UI.with_borrow_mut(|u| match u {
        Some(u) => {
            u.trusted = trusted;
            (u.names.clone(), u.cur)
        }
        None => (Vec::new(), 0),
    });
    rebuild_menu(&names, cur);
}

// ---------------------------------------------------------------- menu

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    tag: isize,
    key: &str,
    mods: NSEventModifierFlags,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(sel!(menuAction:)),
            &NSString::from_str(key),
        )
    };
    item.setKeyEquivalentModifierMask(mods);
    item.setTag(tag);
    if let Some(c) = ctrl() {
        unsafe { item.setTarget(Some(&c)) };
    }
    item
}

pub fn rebuild_menu(names: &[String], cur: usize) {
    UI.with_borrow_mut(|u| {
        let Some(u) = u else { return };
        if !names.is_empty() {
            u.names = names.to_vec();
            u.cur = cur;
        }
        let mtm = u.mtm;
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let chord = NSEventModifierFlags::Control | NSEventModifierFlags::Option;

        if let Some((d, f)) = u.desktops {
            let text = if d > 1 {
                format!("{d} desktops found — Lanes uses only this one…")
            } else {
                format!(
                    "{f} app{} in full screen — Lanes can't place {}…",
                    if f == 1 { "" } else { "s" },
                    if f == 1 { "it" } else { "them" }
                )
            };
            menu.addItem(&menu_item(
                mtm,
                &text,
                TAG_DESKTOPS,
                "",
                NSEventModifierFlags::empty(),
            ));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        if !u.trusted {
            let info = menu_item(
                mtm,
                "Lanes needs Accessibility access to manage windows",
                0,
                "",
                chord,
            );
            info.setEnabled(false);
            menu.addItem(&info);
            menu.addItem(&menu_item(
                mtm,
                "Open Accessibility Settings…",
                TAG_ACCESSIBILITY,
                "",
                chord,
            ));
            menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        for (i, name) in u.names.iter().enumerate() {
            let it = menu_item(mtm, name, i as isize + 1, &(i + 1).to_string(), chord);
            it.setState(if i == u.cur {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            it.setEnabled(u.trusted);
            menu.addItem(&it);
        }
        if !u.names.is_empty() {
            menu.addItem(&NSMenuItem::separatorItem(mtm));
        }
        for (title, tag, key) in [
            ("Show All Apps", TAG_APPS, "o"),
            ("Show All Contexts", TAG_CONTEXTS, "p"),
            ("Snap Windows Back", TAG_SNAP, "y"),
        ] {
            let it = menu_item(mtm, title, tag, key, chord);
            it.setEnabled(u.trusted);
            menu.addItem(&it);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&menu_item(
            mtm,
            "Edit Contexts…",
            TAG_EDIT_CONTEXTS,
            "",
            NSEventModifierFlags::empty(),
        ));
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        let follow = menu_item(
            mtm,
            "Focus Follows Mouse",
            TAG_FOLLOW,
            "",
            NSEventModifierFlags::empty(),
        );
        follow.setState(if follow_mouse() {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
        menu.addItem(&follow);
        menu.addItem(&menu_item(
            mtm,
            "Show Log",
            TAG_LOG,
            "",
            NSEventModifierFlags::empty(),
        ));
        menu.addItem(&menu_item(
            mtm,
            "Quit Lanes",
            TAG_QUIT,
            "q",
            NSEventModifierFlags::Command,
        ));
        u.item.setMenu(Some(&menu));
    });
}

// ---------------------------------------------------------------- building blocks

fn primary_visible_frame(mtm: MainThreadMarker) -> CGRect {
    NSScreen::screens(mtm)
        .firstObject()
        .map(|s| s.visibleFrame())
        .unwrap_or(CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(1440.0, 900.0),
        ))
}

fn make_panel(mtm: MainThreadMarker, ignores_mouse: bool) -> Retained<NSPanel> {
    let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
    let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(100.0, 100.0)),
        style,
        NSBackingStoreType::Buffered,
        false,
    );
    unsafe { panel.setReleasedWhenClosed(false) };
    panel.setOpaque(false);
    panel.setBackgroundColor(Some(&NSColor::clearColor()));
    panel.setHasShadow(true);
    panel.setLevel(NSPopUpMenuWindowLevel);
    panel.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    panel.setIgnoresMouseEvents(ignores_mouse);
    panel.setFloatingPanel(true);
    panel.setBecomesKeyOnlyIfNeeded(true);
    panel.setHidesOnDeactivate(false);
    panel
}

/// A frosted, rounded background that follows Light/Dark mode.
fn glass(mtm: MainThreadMarker, size: CGSize, radius: CGFloat) -> Retained<NSVisualEffectView> {
    let v = NSVisualEffectView::initWithFrame(
        NSVisualEffectView::alloc(mtm),
        CGRect::new(CGPoint::new(0.0, 0.0), size),
    );
    v.setMaterial(NSVisualEffectMaterial::Popover);
    v.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
    v.setState(NSVisualEffectState::Active);
    v.setWantsLayer(true);
    if let Some(layer) = v.layer() {
        layer.setCornerRadius(radius);
        layer.setMasksToBounds(true);
    }
    v
}

fn label(
    mtm: MainThreadMarker,
    text: &str,
    size: CGFloat,
    weight: CGFloat,
    secondary: bool,
    align: NSTextAlignment,
) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFont(Some(&NSFont::systemFontOfSize_weight(size, weight)));
    let color = if secondary {
        NSColor::secondaryLabelColor()
    } else {
        NSColor::labelColor()
    };
    l.setTextColor(Some(&color));
    l.setAlignment(align);
    l.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    l
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

fn app_icon(mtm: MainThreadMarker, pid: i32, size: f64) -> Retained<NSImageView> {
    let iv = NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(0.0, 0.0, size, size));
    iv.setImageScaling(NSImageScaling::ScaleProportionallyUpOrDown);
    if let Some(icon) =
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid).and_then(|a| a.icon())
    {
        iv.setImage(Some(&icon));
    }
    iv
}

fn symbol(mtm: MainThreadMarker, name: &str, points: f64) -> Option<Retained<NSImageView>> {
    let img = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        None,
    )?;
    let iv = NSImageView::imageViewWithImage(&img, mtm);
    iv.setSymbolConfiguration(Some(
        &NSImageSymbolConfiguration::configurationWithPointSize_weight(points, unsafe {
            NSFontWeightRegular
        }),
    ));
    iv.setContentTintColor(Some(&NSColor::labelColor()));
    Some(iv)
}

fn set_bg(view: &NSView, color: Option<&NSColor>) {
    if let Some(layer) = view.layer() {
        let c = color
            .map(|c| c.CGColor())
            .unwrap_or_else(|| NSColor::clearColor().CGColor());
        layer.setBackgroundColor(Some(&c));
    }
}

fn show_panel(panel: &NSPanel, frame: CGRect) {
    cancel(sel!(hudGone:));
    cancel(sel!(dotsGone:));
    panel.setFrame_display(frame, true);
    NSAnimationContext::beginGrouping();
    NSAnimationContext::currentContext().setDuration(0.08);
    let anim: Retained<NSPanel> = unsafe { msg_send![panel, animator] };
    anim.setAlphaValue(1.0);
    NSAnimationContext::endGrouping();
    panel.orderFrontRegardless();
    panel.invalidateShadow();
}

fn set_alpha(panel: &NSPanel, alpha: f64, seconds: f64) {
    NSAnimationContext::beginGrouping();
    NSAnimationContext::currentContext().setDuration(seconds);
    let anim: Retained<NSPanel> = unsafe { msg_send![panel, animator] };
    anim.setAlphaValue(alpha);
    NSAnimationContext::endGrouping();
}

fn fade_out(panel: &NSPanel, gone: Sel) {
    NSAnimationContext::beginGrouping();
    NSAnimationContext::currentContext().setDuration(0.22);
    let anim: Retained<NSPanel> = unsafe { msg_send![panel, animator] };
    anim.setAlphaValue(0.0);
    NSAnimationContext::endGrouping();
    perform(gone, None, 0.25);
}

// ---------------------------------------------------------------- HUDs

fn show_hud(
    build: impl FnOnce(MainThreadMarker) -> (Retained<NSView>, CGSize),
    center_y: f64,
    seconds: Option<f64>,
) {
    UI.with_borrow_mut(|u| {
        let Some(u) = u else { return };
        let mtm = u.mtm;
        let panel = u.hud.get_or_insert_with(|| make_panel(mtm, true)).clone();
        let (content, size) = build(mtm);
        panel.setContentView(Some(&content));
        let vf = primary_visible_frame(mtm);
        let frame = rect(
            (vf.origin.x + (vf.size.width - size.width) / 2.0).round(),
            (vf.origin.y + vf.size.height * center_y - size.height / 2.0).round(),
            size.width,
            size.height,
        );
        show_panel(&panel, frame);
    });
    cancel(sel!(hideHud:));
    if let Some(s) = seconds {
        perform(sel!(hideHud:), None, s);
    }
}

/// The panel shown when you switch context: name, window count, position.
pub fn show_context_hud(name: &str, count: usize, index: usize, total: usize) {
    show_hud(
        |mtm| {
            let size = CGSize::new(240.0, 184.0);
            let g = glass(mtm, size, 26.0);
            if let Some(icon) = symbol(mtm, "rectangle.split.3x1", 34.0) {
                icon.setFrame(rect(0.0, 112.0, size.width, 46.0));
                g.addSubview(&icon);
            }
            let title = label(
                mtm,
                name,
                22.0,
                unsafe { NSFontWeightSemibold },
                false,
                NSTextAlignment::Center,
            );
            title.setFrame(rect(12.0, 72.0, size.width - 24.0, 30.0));
            g.addSubview(&title);
            let sub = format!("{count} window{}", if count == 1 { "" } else { "s" });
            let sub = label(
                mtm,
                &sub,
                13.0,
                unsafe { NSFontWeightRegular },
                true,
                NSTextAlignment::Center,
            );
            sub.setFrame(rect(12.0, 50.0, size.width - 24.0, 18.0));
            g.addSubview(&sub);
            let dot = 7.0;
            let gap = 7.0;
            let row = total as f64 * dot + (total.saturating_sub(1)) as f64 * gap;
            for i in 0..total {
                let d = NSView::initWithFrame(
                    NSView::alloc(mtm),
                    rect(
                        (size.width - row) / 2.0 + i as f64 * (dot + gap),
                        22.0,
                        dot,
                        dot,
                    ),
                );
                d.setWantsLayer(true);
                if let Some(l) = d.layer() {
                    l.setCornerRadius(dot / 2.0);
                }
                let c = if i == index {
                    NSColor::labelColor()
                } else {
                    NSColor::tertiaryLabelColor()
                };
                set_bg(&d, Some(&c));
                g.addSubview(&d);
            }
            (Retained::into_super(g), size)
        },
        0.5,
        Some(0.9),
    );
}

fn notice_view(mtm: MainThreadMarker, title: &str, sub: &str) -> (Retained<NSView>, CGSize) {
    let t = label(
        mtm,
        title,
        15.0,
        unsafe { NSFontWeightSemibold },
        false,
        NSTextAlignment::Center,
    );
    let s = label(
        mtm,
        sub,
        13.0,
        unsafe { NSFontWeightRegular },
        true,
        NSTextAlignment::Center,
    );
    let tw = t.fittingSize().width;
    let sw = if sub.is_empty() {
        0.0
    } else {
        s.fittingSize().width
    };
    let width = (tw.max(sw) + 56.0).clamp(220.0, 640.0);
    let height = if sub.is_empty() { 52.0 } else { 72.0 };
    let size = CGSize::new(width, height);
    let g = glass(mtm, size, 18.0);
    if sub.is_empty() {
        t.setFrame(rect(16.0, 15.0, width - 32.0, 22.0));
    } else {
        t.setFrame(rect(16.0, 37.0, width - 32.0, 22.0));
        s.setFrame(rect(16.0, 15.0, width - 32.0, 18.0));
        g.addSubview(&s);
    }
    g.addSubview(&t);
    (Retained::into_super(g), size)
}

/// A short message that fades on its own.
pub fn notice(title: &str, sub: &str) {
    notice_for(title, sub, 1.8);
}

pub fn notice_for(title: &str, sub: &str, seconds: f64) {
    show_hud(|mtm| notice_view(mtm, title, sub), 0.22, Some(seconds));
}

pub fn set_desktop_warning(desktops: usize, fullscreen: usize) {
    let (names, cur) = UI.with_borrow_mut(|u| match u {
        Some(u) => {
            u.desktops = (desktops > 1 || fullscreen > 0).then_some((desktops, fullscreen));
            (u.names.clone(), u.cur)
        }
        None => (Vec::new(), 0),
    });
    rebuild_menu(&names, cur);
}

fn open_mission_control() {
    let _ = std::process::Command::new("/usr/bin/open")
        .args(["-a", "Mission Control"])
        .spawn();
    notice_for(
        "Hover the top of the screen and click ✕ on extra desktops",
        "Their windows move to this desktop. Then press ⌃⌥Y so Lanes picks them up.",
        9.0,
    );
}

fn open_log() {
    let path = crate::log::path();
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    NSWorkspace::sharedWorkspace().openURL(&url);
}

/// Restart Lanes (from its .app) so macOS applies a newly granted permission.
pub fn relaunch() -> bool {
    let path = objc2_foundation::NSBundle::mainBundle()
        .bundlePath()
        .to_string();
    if !path.ends_with(".app") {
        return false;
    }
    let spawned = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 0.8; /usr/bin/open -n \"$0\"", &path])
        .spawn()
        .is_ok();
    if spawned {
        terminate();
    }
    spawned
}

/// Is another Lanes already running?
pub fn another_copy_running() -> bool {
    let me = std::process::id() as i32;
    let id = NSString::from_str("app.lanes.Lanes");
    NSRunningApplication::runningApplicationsWithBundleIdentifier(&id)
        .iter()
        .any(|a| a.processIdentifier() != me)
}

/// Small dots at the bottom showing your place in the row; fade after a second.
pub fn show_dots(n: usize, active: Option<usize>) {
    let n = n.min(24);
    UI.with_borrow_mut(|u| {
        let Some(u) = u else { return };
        let mtm = u.mtm;
        let panel = u.dots.get_or_insert_with(|| make_panel(mtm, true)).clone();
        // Scrolling sends many updates; rebuild only when the dots change.
        if u.dots_key == Some((n, active)) && panel.isVisible() {
            cancel(sel!(dotsGone:));
            set_alpha(&panel, 1.0, 0.08);
            return;
        }
        u.dots_key = Some((n, active));
        let (dot, wide, gap, pad, h) = (6.0, 18.0, 6.0, 10.0, 22.0);
        let row =
            (n.saturating_sub(1)) as f64 * (dot + gap) + if active.is_some() { wide } else { dot };
        let size = CGSize::new(row + 2.0 * pad, h);
        let g = glass(mtm, size, h / 2.0);
        let mut x = pad;
        for i in 0..n {
            let on = Some(i) == active;
            let w = if on { wide } else { dot };
            let d = NSView::initWithFrame(NSView::alloc(mtm), rect(x, (h - dot) / 2.0, w, dot));
            d.setWantsLayer(true);
            if let Some(l) = d.layer() {
                l.setCornerRadius(dot / 2.0);
            }
            let c = if on {
                NSColor::labelColor()
            } else {
                NSColor::tertiaryLabelColor()
            };
            set_bg(&d, Some(&c));
            g.addSubview(&d);
            x += w + gap;
        }
        panel.setContentView(Some(&g));
        let vf = primary_visible_frame(mtm);
        let frame = rect(
            (vf.origin.x + (vf.size.width - size.width) / 2.0).round(),
            vf.origin.y + 16.0,
            size.width,
            size.height,
        );
        show_panel(&panel, frame);
    });
    debounce(sel!(hideDots:), 1.1);
}

// ---------------------------------------------------------------- overviews

#[allow(clippy::too_many_arguments)]
fn show_overview(
    title: &str,
    detail: &str,
    hint: &str,
    count: usize,
    max_cols: usize,
    tile: CGSize,
    sel: usize,
    fill: impl Fn(MainThreadMarker, usize, &NSView),
) -> usize {
    let cols = count.clamp(1, max_cols);
    let rows = count.div_ceil(cols);
    let (gap, pad, head) = (12.0, 22.0, 54.0);
    let width = (2.0 * pad + cols as f64 * tile.width + (cols - 1) as f64 * gap).max(420.0);
    let height = pad + head + rows as f64 * tile.height + (rows - 1) as f64 * gap + pad;
    UI.with_borrow_mut(|u| {
        let Some(u) = u else { return };
        let mtm = u.mtm;
        let panel = u
            .overview
            .get_or_insert_with(|| make_panel(mtm, false))
            .clone();
        let size = CGSize::new(width, height);
        let g = glass(mtm, size, 22.0);

        let t = label(
            mtm,
            title,
            20.0,
            unsafe { NSFontWeightBold },
            false,
            NSTextAlignment::Left,
        );
        let tw = t.fittingSize().width.min(width * 0.5);
        t.setFrame(rect(pad, height - pad - 28.0, tw + 4.0, 28.0));
        g.addSubview(&t);
        let d = label(
            mtm,
            detail,
            13.0,
            unsafe { NSFontWeightRegular },
            true,
            NSTextAlignment::Left,
        );
        d.setFrame(rect(pad + tw + 12.0, height - pad - 25.0, 160.0, 18.0));
        g.addSubview(&d);
        let h = label(
            mtm,
            hint,
            11.0,
            unsafe { NSFontWeightMedium },
            true,
            NSTextAlignment::Right,
        );
        h.setFrame(rect(width - pad - 300.0, height - pad - 24.0, 300.0, 16.0));
        g.addSubview(&h);

        let ctrl = ctrl();
        u.tiles.clear();
        let grid_w = cols as f64 * tile.width + (cols - 1) as f64 * gap;
        let x0 = (width - grid_w) / 2.0;
        for i in 0..count {
            let (col, row) = ((i % cols) as f64, (i / cols) as f64);
            let x = x0 + col * (tile.width + gap);
            let y = height - pad - head - (row + 1.0) * tile.height - row * gap;
            let v = NSView::initWithFrame(NSView::alloc(mtm), rect(x, y, tile.width, tile.height));
            v.setWantsLayer(true);
            if let Some(l) = v.layer() {
                l.setCornerRadius(14.0);
            }
            fill(mtm, i, &v);
            // A transparent button over the tile makes it clickable.
            let b = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(""),
                    ctrl.as_deref().map(|c| c as &AnyObject),
                    Some(sel!(tileClicked:)),
                    mtm,
                )
            };
            b.setFrame(rect(0.0, 0.0, tile.width, tile.height));
            b.setTransparent(true);
            b.setTag(i as isize);
            v.addSubview(&b);
            g.addSubview(&v);
            u.tiles.push(v);
        }
        panel.setContentView(Some(&g));
        let vf = primary_visible_frame(mtm);
        let frame = rect(
            (vf.origin.x + (vf.size.width - width) / 2.0).round(),
            (vf.origin.y + (vf.size.height - height) / 2.0 + vf.size.height * 0.04).round(),
            width,
            height,
        );
        show_panel(&panel, frame);
    });
    select_tile(sel);
    cols
}

/// ⌃⌥O: every window in the current context.
pub fn show_apps(context: &str, tiles: &[AppTile], sel: usize) -> usize {
    let n = tiles.len();
    show_overview(
        context,
        &format!("{n} window{}", if n == 1 { "" } else { "s" }),
        "← → choose   ↩ open   ⌃⌥↑ all contexts   esc close",
        n,
        6,
        CGSize::new(164.0, 150.0),
        sel,
        |mtm, i, v| fill_app_tile(mtm, &tiles[i], v),
    )
}

/// ⌃⌥Space, then an area: the same panel as ⌃⌥O, with a letter on each app.
pub fn show_place_apps(area: &str, tiles: &[AppTile], sel: usize) -> usize {
    show_overview(
        area,
        "Pick an app",
        "type its letter   ← → ↩   ⌫ back   esc undo",
        tiles.len(),
        6,
        CGSize::new(164.0, 150.0),
        sel,
        |mtm, i, v| fill_app_tile(mtm, &tiles[i], v),
    )
}

fn fill_app_tile(mtm: MainThreadMarker, t: &AppTile, v: &NSView) {
    let icon = app_icon(mtm, t.pid, 64.0);
    icon.setFrame(rect(50.0, 66.0, 64.0, 64.0));
    v.addSubview(&icon);
    let a = label(
        mtm,
        &t.app,
        13.0,
        unsafe { NSFontWeightSemibold },
        false,
        NSTextAlignment::Center,
    );
    a.setFrame(rect(8.0, 38.0, 148.0, 18.0));
    v.addSubview(&a);
    let title = if t.title.is_empty() || t.title == t.app {
        ""
    } else {
        &t.title
    };
    let s = label(
        mtm,
        title,
        11.0,
        unsafe { NSFontWeightRegular },
        true,
        NSTextAlignment::Center,
    );
    s.setFrame(rect(8.0, 18.0, 148.0, 16.0));
    v.setToolTip(Some(&NSString::from_str(&t.title)));
    v.addSubview(&s);
    if let Some(k) = t.key {
        // Keycap-style letter badge on the icon's corner.
        let badge = NSView::initWithFrame(NSView::alloc(mtm), rect(104.0, 108.0, 26.0, 24.0));
        badge.setWantsLayer(true);
        if let Some(l) = badge.layer() {
            l.setCornerRadius(7.0);
        }
        set_bg(&badge, Some(&NSColor::controlAccentColor()));
        let kl = label(
            mtm,
            &k.to_string(),
            14.0,
            unsafe { NSFontWeightBold },
            false,
            NSTextAlignment::Center,
        );
        kl.setTextColor(Some(&NSColor::whiteColor()));
        kl.setFrame(rect(0.0, 3.0, 26.0, 18.0));
        badge.addSubview(&kl);
        v.addSubview(&badge);
    }
    if t.dim {
        v.setAlphaValue(0.45);
    }
}

/// A small rounded key with an arrow on it.
fn keycap(mtm: MainThreadMarker, arrow: &str, cx: f64, cy: f64, lit: bool) -> Retained<NSView> {
    let (w, h) = (24.0, 22.0);
    let cap = NSView::initWithFrame(NSView::alloc(mtm), rect(cx - w / 2.0, cy - h / 2.0, w, h));
    cap.setWantsLayer(true);
    if let Some(l) = cap.layer() {
        l.setCornerRadius(6.0);
        l.setBorderWidth(if lit { 0.0 } else { 0.75 });
        l.setBorderColor(Some(
            &NSColor::labelColor()
                .colorWithAlphaComponent(0.16)
                .CGColor(),
        ));
    }
    let bg = if lit {
        NSColor::controlAccentColor()
    } else {
        NSColor::labelColor().colorWithAlphaComponent(0.07)
    };
    set_bg(&cap, Some(&bg));
    let t = label(
        mtm,
        arrow,
        12.0,
        unsafe { NSFontWeightSemibold },
        !lit,
        NSTextAlignment::Center,
    );
    if lit {
        t.setTextColor(Some(&NSColor::whiteColor()));
    }
    t.setFrame(rect(0.0, 3.0, w, 15.0));
    cap.addSubview(&t);
    cap
}

/// ⌃⌥Space: arrow keys arranged around a miniature screen, each where it
/// points. Held arrows light up and the area they'd pick fills in softly;
/// areas already in use are shaded. `lit` is the area the held arrows pick.
pub fn show_area_map(title: &str, taken: &[bool; 9], filling: bool, lit: Option<usize>) {
    show_hud(
        |mtm| {
            let (width, height) = (340.0, 286.0);
            let size = CGSize::new(width, height);
            let g = glass(mtm, size, 24.0);

            let t = label(
                mtm,
                title,
                14.0,
                unsafe { NSFontWeightSemibold },
                false,
                NSTextAlignment::Center,
            );
            t.setFrame(rect(20.0, height - 40.0, width - 40.0, 20.0));
            g.addSubview(&t);

            // The miniature screen, 16:10, with a hint of a menu bar.
            let (sw, sh) = (196.0, 122.0);
            let (x0, y0) = ((width - sw) / 2.0, 82.0);
            let screen = NSView::initWithFrame(NSView::alloc(mtm), rect(x0, y0, sw, sh));
            screen.setWantsLayer(true);
            if let Some(l) = screen.layer() {
                l.setCornerRadius(9.0);
                l.setBorderWidth(1.0);
                l.setBorderColor(Some(
                    &NSColor::labelColor()
                        .colorWithAlphaComponent(0.22)
                        .CGColor(),
                ));
                l.setMasksToBounds(true);
            }
            set_bg(
                &screen,
                Some(&NSColor::labelColor().colorWithAlphaComponent(0.04)),
            );
            let bar = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, sh - 7.0, sw, 7.0));
            bar.setWantsLayer(true);
            set_bg(
                &bar,
                Some(&NSColor::labelColor().colorWithAlphaComponent(0.08)),
            );
            screen.addSubview(&bar);
            // Faint centre lines hint at halves and quarters.
            let inner_h = sh - 7.0;
            for (fx, fy, fw, fh) in [
                (sw / 2.0 - 0.25, 0.0, 0.5, inner_h),
                (0.0, inner_h / 2.0 - 0.25, sw, 0.5),
            ] {
                let line = NSView::initWithFrame(NSView::alloc(mtm), rect(fx, fy, fw, fh));
                line.setWantsLayer(true);
                set_bg(
                    &line,
                    Some(&NSColor::labelColor().colorWithAlphaComponent(0.10)),
                );
                screen.addSubview(&line);
            }
            // Areas in use (shaded) and the one the held arrows pick (accent).
            let area_view = |r: [f64; 4], color: &NSColor| {
                let inset = 3.0;
                let v = NSView::initWithFrame(
                    NSView::alloc(mtm),
                    rect(
                        inset + r[0] * (sw - 2.0 * inset),
                        inset + (1.0 - r[1] - r[3]) * (inner_h - 2.0 * inset),
                        r[2] * (sw - 2.0 * inset) - 2.0,
                        r[3] * (inner_h - 2.0 * inset) - 2.0,
                    ),
                );
                v.setWantsLayer(true);
                if let Some(l) = v.layer() {
                    l.setCornerRadius(5.0);
                }
                set_bg(&v, Some(color));
                v
            };
            let used = NSColor::labelColor().colorWithAlphaComponent(0.12);
            for (i, &is_taken) in taken.iter().enumerate() {
                if is_taken && i != crate::layout::MIDDLE {
                    screen.addSubview(&area_view(crate::layout::AREAS[i], &used));
                }
            }
            if let Some(i) = lit {
                let soft = NSColor::controlAccentColor().colorWithAlphaComponent(0.55);
                screen.addSubview(&area_view(crate::layout::AREAS[i], &soft));
            }
            g.addSubview(&screen);

            // Keys along the screen's edges, each where it points.
            let top = y0 + sh + 18.0;
            let bottom = y0 - 18.0;
            let mid = y0 + sh / 2.0;
            let spots: [(usize, f64, f64); 8] = [
                (0, x0 + 26.0, top),
                (1, width / 2.0, top),
                (2, x0 + sw - 26.0, top),
                (3, x0 - 22.0, mid),
                (5, x0 + sw + 22.0, mid),
                (6, x0 + 26.0, bottom),
                (7, width / 2.0, bottom),
                (8, x0 + sw - 26.0, bottom),
            ];
            for (i, cx, cy) in spots {
                let arrows: Vec<&str> = crate::layout::AREA_ARROWS[i].split_whitespace().collect();
                let on = lit == Some(i);
                let n = arrows.len() as f64;
                for (k, a) in arrows.iter().enumerate() {
                    let x = cx + (k as f64 - (n - 1.0) / 2.0) * 27.0;
                    let cap = keycap(mtm, a, x, cy, on);
                    if taken[i] && !on {
                        cap.setAlphaValue(0.35);
                    }
                    g.addSubview(&cap);
                }
            }

            let hint = label(
                mtm,
                if filling {
                    "⌫ separate   ·   ↩ done   ·   esc undo"
                } else {
                    "Hold two arrows for a corner   ·   esc"
                },
                11.0,
                unsafe { NSFontWeightRegular },
                true,
                NSTextAlignment::Center,
            );
            hint.setFrame(rect(20.0, 18.0, width - 40.0, 16.0));
            g.addSubview(&hint);
            (Retained::into_super(g), size)
        },
        0.5,
        None,
    );
}

/// A thin, soft, see-through accent ring around a window (frame in AppKit
/// coordinates): a quiet hint of where the shortcuts go, not a bright frame.
pub fn show_outline(x: f64, y: f64, w: f64, h: f64) {
    UI.with_borrow_mut(|u| {
        let Some(u) = u else { return };
        let mtm = u.mtm;
        let pad = 2.0;
        let frame = rect(x - pad, y - pad, w + 2.0 * pad, h + 2.0 * pad);
        let panel = u
            .outline
            .get_or_insert_with(|| {
                let p = make_panel(mtm, true);
                p.setHasShadow(false);
                // Above app windows, below Lanes' own panels.
                p.setLevel(3);
                let ring = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 10.0, 10.0));
                ring.setWantsLayer(true);
                if let Some(l) = ring.layer() {
                    l.setBorderWidth(3.0);
                    l.setCornerRadius(13.0);
                }
                p.setContentView(Some(&ring));
                p
            })
            .clone();
        // The accent colour can change in System Settings, so set it each time.
        if let Some(l) = panel.contentView().and_then(|v| v.layer()) {
            let soft = NSColor::controlAccentColor().colorWithAlphaComponent(0.45);
            l.setBorderColor(Some(&soft.CGColor()));
        }
        // Jump straight there: no animation.
        if panel.frame() != frame {
            panel.setFrame_display(frame, true);
        }
        if !panel.isVisible() {
            panel.setAlphaValue(1.0);
            panel.orderFrontRegardless();
        }
    });
}

pub fn hide_outline() {
    UI.with_borrow(|u| {
        if let Some(p) = u.as_ref().and_then(|u| u.outline.as_ref()) {
            p.orderOut(None);
        }
    });
}

pub fn hide_notice() {
    cancel(sel!(hideHud:));
    UI.with_borrow(|u| {
        if let Some(p) = u.as_ref().and_then(|u| u.hud.as_ref()) {
            p.orderOut(None);
        }
    });
}

/// ⌃⌥P: every context side by side.
pub fn show_contexts(cards: &[ContextCard], sel: usize) -> usize {
    let n = cards.len();
    show_overview(
        "All contexts",
        &format!("{n} contexts"),
        "← → choose   ↩ switch   ⌃⌥↓ see its apps   esc close",
        n,
        4,
        CGSize::new(248.0, 146.0),
        sel,
        |mtm, i, v| {
            let c = &cards[i];
            let name = label(
                mtm,
                &c.name,
                17.0,
                unsafe { NSFontWeightBold },
                false,
                NSTextAlignment::Left,
            );
            name.setFrame(rect(16.0, 108.0, 150.0, 24.0));
            v.addSubview(&name);
            let key = label(
                mtm,
                &format!("⌃⌥{}", i + 1),
                12.0,
                unsafe { NSFontWeightMedium },
                true,
                NSTextAlignment::Right,
            );
            key.setFrame(rect(170.0, 111.0, 62.0, 16.0));
            v.addSubview(&key);
            let mut detail = format!("{} window{}", c.count, if c.count == 1 { "" } else { "s" });
            if c.current {
                detail.push_str(" · current");
            }
            let d = label(
                mtm,
                &detail,
                12.0,
                unsafe { NSFontWeightRegular },
                true,
                NSTextAlignment::Left,
            );
            d.setFrame(rect(16.0, 88.0, 216.0, 16.0));
            v.addSubview(&d);
            if c.pids.is_empty() {
                let e = label(
                    mtm,
                    "No windows yet",
                    12.0,
                    unsafe { NSFontWeightRegular },
                    true,
                    NSTextAlignment::Left,
                );
                e.setFrame(rect(16.0, 26.0, 216.0, 16.0));
                v.addSubview(&e);
            }
            for (k, pid) in c.pids.iter().take(6).enumerate() {
                let icon = app_icon(mtm, *pid, 32.0);
                icon.setFrame(rect(14.0 + k as f64 * 37.0, 18.0, 32.0, 32.0));
                v.addSubview(&icon);
            }
        },
    )
}

/// Is this point (global, top-left origin) inside the open overview?
pub fn overview_contains(x: f64, y: f64) -> bool {
    UI.with_borrow(|u| {
        let Some(u) = u else { return false };
        let Some(p) = u.overview.as_ref().filter(|p| p.isVisible()) else {
            return false;
        };
        let Some(primary) = NSScreen::screens(u.mtm).firstObject() else {
            return false;
        };
        let ph = primary.frame().size.height;
        let f = p.frame();
        let top = ph - (f.origin.y + f.size.height);
        x >= f.origin.x && x <= f.origin.x + f.size.width && y >= top && y <= top + f.size.height
    })
}

pub fn select_tile(sel: usize) {
    UI.with_borrow(|u| {
        let Some(u) = u else { return };
        let on = NSColor::controlAccentColor().colorWithAlphaComponent(0.26);
        for (i, v) in u.tiles.iter().enumerate() {
            set_bg(v, (i == sel).then_some(&*on));
        }
    });
}

pub fn hide_overview() {
    UI.with_borrow_mut(|u| {
        let Some(u) = u else { return };
        if let Some(p) = &u.overview {
            p.orderOut(None);
            p.setContentView(None);
        }
        u.tiles.clear();
    });
}
