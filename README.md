# Lanes

A lightweight scrolling window manager for macOS, written in Rust.

- **Contexts.** Work and Personal (or up to 9 of your own) are separate. You can only reach the
  windows of the context you're in; switching context swaps every window instantly.
- **One row per context.** Windows sit side by side at full height. Slide through them with the
  keyboard, or hold ⌃⌥ and scroll.
- **Native and light.** Apple's own controls and colours (follows Light/Dark mode and your accent
  colour), no web view, about a 0.5 MB binary. Shortcuts and scrolling are read on their own
  thread and each app's windows are moved on another, so a busy app never slows the pointer.

## Install

```sh
./scripts/setup-signing.sh   # once
./scripts/build-app.sh          # builds dist/Lanes.app
open dist/Lanes.app
```

On first launch macOS asks for **Accessibility** access (System Settings › Privacy & Security ›
Accessibility). Lanes starts by itself once it's allowed.

Run `./scripts/setup-signing.sh` once before the first build. It creates a personal signing
certificate in your keychain, so every rebuild keeps the same identity and macOS remembers the
Accessibility permission. Without it, each rebuild needs the permission granted again.

To start Lanes at login: System Settings › General › Login Items › add `Lanes.app`.

## Shortcuts

Every shortcut is ⌃⌥ (Control + Option) plus one key on the right side of the keyboard.

| Keys | Action |
|---|---|
| ⌃⌥← / ⌃⌥→ (or H / L) | Previous / next window |
| hold ⌃⌥ + scroll | Slide the row (stops where you stop) |
| ⌃⌥[ / ⌃⌥] | Context on the left / right |
| ⌃⌥1 … ⌃⌥9 | Go to a context |
| ⌃⌥O | Show all apps in this context |
| ⇧← / ⇧→ (in ⌃⌥O) | Move the selected app left / right in the row (the order is saved) |
| ⌃⌥P | Show all contexts. Drag an app's icon onto another context to move that window there |
| ⌃⌥↑ / ⌃⌥↓ | Zoom out / in one level |
| ⌃⌥= / ⌃⌥- | Wider / narrower (⅓ · ½ · ⅔ · full) |
| ⌃⌥M | Centre the window |
| ⌃⌥Return | Full view; press again to go back. On a window in a screen: takes it out full width, and back |
| ⌃⌥Y | Snap windows back into the row |
| ⌃⌥Space | Arrange apps on one screen: arrows pick the area (one arrow = half, two = corner, ← → = full), then the app's letter. The window you're in goes first. ⌫ splits a screen back into separate windows |

In an overview: arrows to choose, Return to open, Esc, any other key or a click outside to close.

The row glides to its place. With the system's Reduce Motion setting on it jumps instead, unless
you turn on **Smooth Gliding** in the menu bar menu.

## Contexts

Menu bar icon › **Edit Contexts…** opens `~/.config/lanes/contexts.txt`: one name per line, up to 9.
Quit and reopen Lanes after saving. Which window belongs to which context is remembered in
`~/Library/Application Support/Lanes/state.tsv`.

## Good to know

- Lanes manages the main display. Windows on other displays are left alone.
- Use one desktop (Space) per display; Lanes replaces Spaces with contexts.
- Windows that can't be resized (Calculator, About boxes) sit in the row at their own width.
- New windows join the context you're in, right after the focused window.
- To move a window to another context, open ⌃⌥P and drag its icon onto that context. It joins
  the end of that row. An app with several windows shows one icon per window (hover for its title).
- Focus follows the pointer (turn it off in the menu). It waits while a menu is open or while
  you're in a dialog Lanes doesn't manage, so pointing elsewhere never closes or buries them.
- Quitting Lanes (menu, ⌘Q in its menu, logout, or `kill`) brings every hidden window back on screen.

## Layout

```
src/main.rs    startup, clean shutdown on signals
src/wm.rs      contexts, the row, focus, overviews (the core)
src/layout.rs  geometry: where the row rests, areas of a screen (pure, unit-tested)
src/mover.rs   one worker per app that moves its windows off the main thread
src/input.rs   one event tap, on its own thread, for shortcuts and ⌃⌥-scroll
src/ax.rs      Accessibility API helpers
src/spaces.rs  desktops and on-screen windows, read from the window server
src/ui.rs      menu bar, HUDs, position dots, overview panels
src/store.rs   saved contexts and state
src/log.rs     ~/Library/Logs/Lanes.log
```
