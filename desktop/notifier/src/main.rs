//! `wingman-notify` — the desktop popup stack.
//!
//! A frameless always-on-top window in the bottom-right corner that renders
//! whatever lands in `~/.wingman/notifications.jsonl` and writes the answers
//! back. It talks to no daemon and holds no socket: any wingman process — a
//! detached `pilot run`, a worker, the TUI, a `serve` turn — reaches it by
//! appending a line to a file, which is the one channel all of them share.
//!
//! The window is *hidden* whenever the stack is empty rather than made
//! click-through. `set_ignore_cursor_events` behaves differently on each
//! platform and still leaves an invisible always-on-top window painting over
//! the user's screen; a hidden window is click-through by definition and is one
//! line.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod tail;
mod wire;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};

use wire::Notification;

/// How often the inbox is re-read.
///
/// A `metadata().len()` call on one small file, four times a second. The
/// `notify` crate would replace only that check — the byte-offset read still
/// has to happen — and `wingman-rag` already pairs it with a 500ms debouncer
/// because the raw events are too noisy. Debouncing a prompt somebody is
/// blocked on is exactly backwards, and sub-300ms is the difference between
/// "instant" and "broken".
const POLL: Duration = Duration::from_millis(250);

/// How often the liveness marker is re-stamped. `ask_user` treats a marker
/// older than 30s as gone, so this leaves room for a slow tick.
const ALIVE_EVERY: Duration = Duration::from_secs(10);

/// Card width. Fixed: a notification that reflows as it grows is harder to read
/// at a glance than one that always looks the same.
const WIDTH: u32 = 380;

/// Breathing room between the stack and the corner of the work area.
const GAP: u32 = 12;

/// Fraction of the work area the stack may occupy before it scrolls internally.
const MAX_HEIGHT_FRACTION: f64 = 0.6;

struct Inbox {
    dir: PathBuf,
    /// Cards raised and not yet answered, oldest first.
    ///
    /// The webview sends back only an id and what the user typed or clicked;
    /// the run directory and the control command are looked up here. Keeping
    /// them out of JavaScript means the path this process writes to can never
    /// be influenced by the page.
    ///
    /// It is also what the page reads on mount. Emitting is fire-and-forget, so
    /// a card raised before the webview finished loading — which is exactly
    /// when the startup replay happens — reaches no listener at all. The page
    /// pulls this list first and treats the event stream as the update channel
    /// rather than the source of truth.
    open: Mutex<Vec<Notification>>,
}

/// Bottom-right of the *work area*, in physical pixels.
///
/// Taking the work area rather than the monitor size is the whole point: on
/// Windows the taskbar, on macOS the Dock and menu bar, and on Linux any panel
/// are excluded, so the stack never sits underneath them. Kept as arithmetic
/// over plain numbers so it is testable without a monitor.
fn corner(area: (i32, i32, u32, u32), window: (u32, u32), gap: u32) -> (i32, i32) {
    let (ax, ay, aw, ah) = area;
    let (ww, wh) = window;
    // `saturating_sub` on the unsigned side first: a window wider or taller
    // than the work area pins to the top-left corner instead of wrapping
    // around to a huge positive offset.
    let x = ax + aw.saturating_sub(ww.saturating_add(gap)) as i32;
    let y = ay + ah.saturating_sub(wh.saturating_add(gap)) as i32;
    (x, y)
}

/// The primary monitor's work area, in physical pixels.
///
/// Primary rather than the monitor under the cursor (the popup would chase the
/// mouse) or under the focused window (needs per-platform foreground APIs Tauri
/// does not surface). Predictable beats clever for something that appears
/// without being asked.
fn work_area(window: &WebviewWindow) -> Option<(i32, i32, u32, u32)> {
    let monitor = window.primary_monitor().ok().flatten()?;
    let area = monitor.work_area();
    Some((
        area.position.x,
        area.position.y,
        area.size.width,
        area.size.height,
    ))
}

/// Move the window to the corner and size it to its content.
fn place(window: &WebviewWindow, height: u32) -> tauri::Result<()> {
    let area = work_area(window).unwrap_or((0, 0, 1920, 1040));
    let cap = (area.3 as f64 * MAX_HEIGHT_FRACTION) as u32;
    let height = height.clamp(1, cap.max(1));
    window.set_size(tauri::PhysicalSize::new(WIDTH, height))?;
    let (x, y) = corner(area, (WIDTH, height), GAP);
    window.set_position(tauri::PhysicalPosition::new(x, y))
}

/// Show a card.
fn push(app: &AppHandle, n: Notification) {
    if let Some(state) = app.try_state::<Inbox>() {
        if let Ok(mut open) = state.open.lock() {
            if open.iter().any(|c| c.id == n.id) {
                return;
            }
            open.push(n.clone());
        }
    }
    if let Some(window) = app.get_webview_window("stack") {
        // Fire-and-forget: a card raised before the page has mounted reaches
        // nobody, which is why `open` exists for it to read on startup.
        //
        // Showing the window is deliberately not done here. The page knows how
        // many cards it is drawing and reports it through `resize`; a `show`
        // here would race the empty first layout that follows it and lose.
        let _ = window.emit("notification", &n);
    }
}

/// Everything currently unanswered, oldest first.
///
/// Called once when the page mounts. Without it the startup replay is lost:
/// the backend raises those cards while the webview is still loading, and an
/// unanswered approval silently failing to come back is the one thing this
/// whole channel exists to prevent.
#[tauri::command]
fn open(state: State<Inbox>) -> Vec<Notification> {
    state.open.lock().map(|o| o.clone()).unwrap_or_default()
}

/// Record the user's answer. `action` is the button pressed, `text` the box.
///
/// Both `None` means dismissed, which still writes a reply: that is what stops
/// the card coming back the next time the app starts.
#[tauri::command]
fn reply(
    state: State<Inbox>,
    id: String,
    action: Option<String>,
    text: Option<String>,
) -> Result<(), String> {
    let card = {
        let mut open = state
            .open
            .lock()
            .map_err(|_| "inbox lock poisoned".to_string())?;
        let at = open
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| format!("no open notification {id}"))?;
        open.remove(at)
    };

    tail::answer(&state.dir, &card, action.as_deref(), text.as_deref())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Forget a card without answering it — used when it expires on screen.
///
/// Deliberately writes nothing: the process that asked has already stopped
/// listening, so a reply would go nowhere, and recording one would claim the
/// user answered when they did not.
#[tauri::command]
fn forget(state: State<Inbox>, id: String) {
    if let Ok(mut open) = state.open.lock() {
        open.retain(|c| c.id != id);
    }
}

/// The stack changed size. Height `0` means it emptied.
///
/// The single owner of whether the window is on screen. The page is the only
/// thing that knows whether it has anything to draw, and routing both the size
/// and the visibility through one report is what stops them disagreeing — an
/// earlier version showed the window when a card arrived and then let the
/// page's empty first layout hide it again, leaving a correctly positioned,
/// correctly sized, invisible stack.
#[tauri::command]
fn resize(window: WebviewWindow, height: u32) -> Result<(), String> {
    if height == 0 {
        return window.hide().map_err(|e| e.to_string());
    }
    place(&window, height).map_err(|e| e.to_string())?;
    window.show().map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![open, reply, forget, resize])
        .setup(|app| {
            let dir = wire::global_dir().ok_or("no home directory")?;
            std::fs::create_dir_all(&dir)?;
            app.manage(Inbox {
                dir: dir.clone(),
                open: Mutex::new(Vec::new()),
            });

            // Quit needs a menu: a `skipTaskbar` frameless window has no other
            // way out.
            let quit = MenuItem::with_id(app, "quit", "Quit Wingman Notify", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&quit])?;
            let mut tray = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("Wingman")
                .on_menu_event(|app, event| {
                    if event.id() == "quit" {
                        app.exit(0);
                    }
                });
            if let Some(icon) = app.default_window_icon().cloned() {
                tray = tray.icon(icon);
            }
            tray.build(app)?;

            let handle = app.handle().clone();
            std::thread::spawn(move || {
                // Start at the end of the file, then put back only what still
                // genuinely needs an answer. Reading from the top would open
                // the app onto every card ever raised.
                let mut reader = tail::Reader::at_end(&dir);
                for card in tail::replay(&dir, wire::now_secs()) {
                    push(&handle, card);
                }

                let mut stamped = Instant::now() - ALIVE_EVERY;
                loop {
                    if stamped.elapsed() >= ALIVE_EVERY {
                        let _ = tail::touch_alive(&dir);
                        stamped = Instant::now();
                    }
                    for card in reader.poll() {
                        push(&handle, card);
                    }
                    std::thread::sleep(POLL);
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("wingman-notify failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corner_is_bottom_right_of_the_work_area_not_the_screen() {
        // A 1920x1080 monitor with a 40px taskbar: the work area stops at 1040,
        // and the stack has to stop above it. Using the monitor height here
        // would put the card under the taskbar, which is the bug this guards.
        let area = (0, 0, 1920, 1040);
        assert_eq!(corner(area, (380, 200), 12), (1528, 828));

        // A monitor to the left of the primary one keeps its negative origin.
        assert_eq!(corner((-1920, 0, 1920, 1040), (380, 200), 12), (-392, 828));

        // A macOS-style 25px menu bar shifts the work area's origin down, and
        // the stack has to shift with it rather than assume a zero origin.
        assert_eq!(corner((0, 25, 1920, 1015), (380, 200), 12), (1528, 828));
    }

    #[test]
    fn a_stack_bigger_than_the_screen_pins_to_the_corner_instead_of_wrapping() {
        // Unsigned arithmetic underflows into an enormous positive offset if
        // this is written the obvious way, and the window vanishes off-screen.
        assert_eq!(corner((0, 0, 300, 200), (380, 400), 12), (0, 0));
    }
}
