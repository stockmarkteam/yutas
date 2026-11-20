//! What the event tap (keys.rs) feeds: double-tap detection on the trigger key,
//! composition tracking, and holding back type-ahead while an operation runs.

use std::sync::mpsc::Sender;
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::keys::{self, QKey};
use crate::romaji::Composer;
use crate::vk;

/// Where a key event came from.
#[derive(Clone, Copy, PartialEq)]
pub enum Origin {
    /// The user's keyboard.
    User,
    /// Injected by us as part of an operation: ignored.
    Own,
    /// Type-ahead we held back and are now replaying: tracked, never queued again.
    Replay,
}

/// One key press or release, as the event tap saw it.
pub struct KeyEvent {
    /// See vk.rs.
    pub vk: u16,
    pub up: bool,
    pub origin: Origin,
    pub shift: bool,
    /// Control, Option or Command is held: this is a shortcut, not text.
    pub chord: bool,
    /// What to send to deliver this key later.
    pub raw: QKey,
}

const DOUBLE_TAP: Duration = Duration::from_millis(350);

/// What the user double-taps: a modifier. It is never held back: a tap is a
/// press and release with no other key in between.
#[derive(Clone, Copy)]
pub struct Trigger {
    name: &'static str,
    vks: &'static [u16],
}

impl Trigger {
    /// shift (default) | lshift | rshift | ctrl | lctrl | rctrl
    fn from_setting(choice: &str) -> Trigger {
        let (name, vks): (&'static str, &'static [u16]) = match choice.trim().to_lowercase().as_str() {
            "lshift" => ("左Shift", &[vk::LSHIFT]),
            "rshift" => ("右Shift", &[vk::RSHIFT]),
            "ctrl" => ("Control", &[vk::LCONTROL, vk::RCONTROL]),
            "lctrl" => ("左Control", &[vk::LCONTROL]),
            "rctrl" => ("右Control", &[vk::RCONTROL]),
            _ => ("Shift", &[vk::LSHIFT, vk::RSHIFT]),
        };
        Trigger { name, vks }
    }
}

static TRIGGER: LazyLock<RwLock<Trigger>> = LazyLock::new(|| RwLock::new(Trigger::from_setting("shift")));

fn trigger() -> Trigger {
    *TRIGGER.read().unwrap_or_else(|e| e.into_inner())
}

/// Switch the trigger at runtime (called when settings are loaded or saved).
pub fn set_trigger(choice: &str) {
    let new = Trigger::from_setting(choice);
    *TRIGGER.write().unwrap_or_else(|e| e.into_inner()) = new;
    let mut st = state();
    st.tap_time = None;
    st.trigger_down = false;
    crate::log(&format!("trigger: {} x2", new.name));
}

pub enum Job {
    /// The trigger was double-tapped.
    Trigger,
}

#[derive(Clone)]
pub struct Last {
    pub original: String,
    pub result: String,
}

#[derive(Default)]
pub struct State {
    pub comp: Composer,
    /// The caret moved or something else happened inside the composition
    /// that we do not mirror; the tracked text can no longer be trusted.
    pub dirty: bool,
    /// The IME is showing conversion candidates.
    pub converting: bool,
    fg: isize,

    /// An operation is running: real keys are queued instead of delivered.
    pub busy: bool,
    pub cancel: bool,
    pub queue: Vec<QKey>,

    /// Time of the first tap, while waiting for the second.
    tap_time: Option<Instant>,
    trigger_down: bool,
    /// No other key since the trigger went down.
    trigger_clean: bool,

    /// The last result is still right before the caret, untouched.
    pub undo_armed: bool,
    pub last: Option<Last>,
}

impl State {
    pub fn reset_composition(&mut self) {
        self.comp.clear();
        self.dirty = false;
        self.converting = false;
    }

    fn on_convert_key(&mut self) {
        if !self.comp.is_empty() {
            self.converting = true;
        }
    }

    fn within_double_tap(&self) -> bool {
        self.tap_time.is_some_and(|t| t.elapsed() < DOUBLE_TAP)
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
static JOBS: OnceLock<Sender<Job>> = OnceLock::new();

pub fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn send_job(job: Job) {
    if let Some(tx) = JOBS.get() {
        let _ = tx.send(job);
    }
}

pub fn start(jobs: Sender<Job>) {
    let _ = JOBS.set(jobs);
    keys::install_hooks();
}

/// Deliver queued type-ahead, then leave the busy state.
pub fn finish_operation() {
    loop {
        let batch = {
            let mut st = state();
            if st.queue.is_empty() {
                st.busy = false;
                st.cancel = false;
                return;
            }
            std::mem::take(&mut st.queue)
        };
        keys::replay(&batch);
    }
}

/// A mouse button went down: the caret may have moved anywhere.
pub fn on_mouse_down() {
    let mut st = state();
    st.reset_composition();
    st.undo_armed = false;
    st.tap_time = None;
    if st.busy {
        st.cancel = true;
    }
}

/// Returns true when the key must not reach the application.
pub fn handle_key(ev: KeyEvent) -> bool {
    if ev.origin == Origin::Own {
        return false;
    }
    let replay = ev.origin == Origin::Replay;
    let up = ev.up;
    let vk = ev.vk;
    let current = trigger();
    let mut st = state();

    if !replay && modifier_trigger(&mut st, current.vks.contains(&vk), up) {
        st.busy = true;
        st.cancel = false;
        drop(st);
        send_job(Job::Trigger);
        return false; // the release itself still reaches the app
    }

    if st.busy && !replay {
        if vk == vk::ESCAPE {
            if !up {
                st.cancel = true;
            }
        } else {
            st.queue.push(ev.raw);
        }
        return true;
    }

    if up {
        return false;
    }

    track(&mut st, &ev);
    false
}

/// Follows the trigger. Returns true on the release that completes a
/// double tap. State is followed even while busy so it never goes stale.
fn modifier_trigger(st: &mut State, is_trigger: bool, up: bool) -> bool {
    if !is_trigger {
        if !up {
            st.trigger_clean = false;
            st.tap_time = None;
        }
        return false;
    }
    if !up {
        if !st.trigger_down {
            st.trigger_down = true;
            st.trigger_clean = true;
        }
        return false;
    }
    st.trigger_down = false;
    if !st.trigger_clean || st.busy {
        st.tap_time = None;
        return false;
    }
    if st.within_double_tap() {
        st.tap_time = None;
        true
    } else {
        st.tap_time = Some(Instant::now());
        false
    }
}

/// Mirror what this key does to the IME composition.
fn track(st: &mut State, ev: &KeyEvent) {
    let vk = ev.vk;
    if vk::is_modifier(vk) {
        return;
    }
    let fg = keys::foreground_id();
    if fg != st.fg {
        st.fg = fg;
        st.reset_composition();
    }
    st.undo_armed = false;

    if ev.chord {
        st.reset_composition();
        return;
    }

    let composing = !st.comp.is_empty();
    match vk {
        0x41..=0x5A => {
            if st.converting {
                st.reset_composition(); // typing on commits the candidates
            }
            st.comp.push_letter((vk as u8 as char).to_ascii_lowercase(), ev.shift);
        }
        0x30..=0x39 | vk::SYMBOL => {
            if st.converting {
                st.reset_composition();
            }
            st.comp.push_symbol();
        }
        vk::BACK => {
            if st.converting {
                st.dirty = true;
            } else {
                st.comp.backspace();
                if st.comp.is_empty() {
                    st.reset_composition();
                }
            }
        }
        vk::SPACE => st.on_convert_key(),
        vk::RETURN => st.reset_composition(),
        vk::ESCAPE => {
            if st.converting {
                st.converting = false;
            } else {
                st.reset_composition();
            }
        }
        _ => {
            if composing {
                st.dirty = true; // arrows, Tab, 英数, かな, Delete...
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed presses and releases as (is the trigger, goes down).
    fn taps(st: &mut State, events: &[(bool, bool)]) -> Vec<bool> {
        events.iter().map(|&(is_trigger, down)| modifier_trigger(st, is_trigger, !down)).collect()
    }

    #[test]
    fn two_clean_taps_fire_on_the_second_release() {
        let mut st = State::default();
        assert_eq!(taps(&mut st, &[(true, true), (true, false), (true, true), (true, false)]), [false, false, false, true]);
        // ... and a third tap starts over instead of firing again.
        assert_eq!(taps(&mut st, &[(true, true), (true, false)]), [false, false]);
    }

    #[test]
    fn shift_used_for_typing_is_not_a_tap() {
        let mut st = State::default();
        // Shift+A, then a lone Shift tap: only one clean tap so far.
        let fired = taps(&mut st, &[(true, true), (false, true), (true, false), (true, true), (true, false)]);
        assert!(!fired.contains(&true));
        // A key between the two taps breaks the pair.
        let mut st = State::default();
        let fired = taps(&mut st, &[(true, true), (true, false), (false, true), (true, true), (true, false)]);
        assert!(!fired.contains(&true));
    }

    #[test]
    fn slow_taps_and_taps_while_busy_do_not_fire() {
        let mut st = State::default();
        taps(&mut st, &[(true, true), (true, false)]);
        st.tap_time = Some(Instant::now() - DOUBLE_TAP - Duration::from_millis(1));
        assert_eq!(taps(&mut st, &[(true, true), (true, false)]), [false, false]);

        let mut st = State { busy: true, ..State::default() };
        let fired = taps(&mut st, &[(true, true), (true, false), (true, true), (true, false)]);
        assert!(!fired.contains(&true));
    }

    #[test]
    fn trigger_settings() {
        assert_eq!(Trigger::from_setting("rshift").vks, [vk::RSHIFT]);
        assert_eq!(Trigger::from_setting(" LCtrl ").vks, [vk::LCONTROL]);
        // An unknown choice (such as "convert" from an older config) means Shift.
        assert_eq!(Trigger::from_setting("convert").vks, [vk::LSHIFT, vk::RSHIFT]);
    }
}
