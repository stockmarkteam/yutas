//! The floating status pill. The window is always present, transparent,
//! click-through and never activated; the page fades the pill in and out.

use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition};

static GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize, Clone)]
struct Payload<'a> {
    state: &'a str,
    text: &'a str,
    hint: &'a str,
}

pub fn init(app: &AppHandle) {
    let Some(window) = app.get_webview_window("pill") else { return };
    let _ = window.set_ignore_cursor_events(true);
    // Stay visible over full-screen apps and on every Space.
    let _ = window.set_visible_on_all_workspaces(true);
    place(app);
}

/// Bottom centre of the screen the pointer is on, clear of the Dock.
fn place(app: &AppHandle) {
    let Some(window) = app.get_webview_window("pill") else { return };
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten())
        .or_else(|| window.current_monitor().ok().flatten());
    let (Some(monitor), Ok(size)) = (monitor, window.outer_size()) else { return };
    let (origin, screen) = (monitor.position(), monitor.size());
    let margin = (110.0 * monitor.scale_factor()) as i32;
    let x = origin.x + (screen.width as i32 - size.width as i32) / 2;
    let y = origin.y + screen.height as i32 - size.height as i32 - margin;
    let _ = window.set_position(PhysicalPosition::new(x, y));
}

fn emit(app: &AppHandle, state: &str, text: &str, hint: &str) {
    let _ = app.emit("pill", Payload { state, text, hint });
}

/// Show a state that stays until replaced.
pub fn show(app: &AppHandle, state: &str, text: &str, hint: &str) {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    place(app);
    emit(app, state, text, hint);
}

/// Update the text of the current state without moving the window.
pub fn update(app: &AppHandle, state: &str, text: &str, hint: &str) {
    emit(app, state, text, hint);
}

/// Show a final state, then fade out.
pub fn flash(app: &AppHandle, state: &str, text: &str, hint: &str, ms: u64) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    place(app);
    emit(app, state, text, hint);
    let app = app.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(ms));
        if GENERATION.load(Ordering::SeqCst) == generation {
            emit(&app, "hide", "", "");
        }
    });
}
