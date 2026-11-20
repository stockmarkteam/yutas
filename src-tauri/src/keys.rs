//! The event tap, synthetic key input, clipboard round-trips and
//! input-source state.
//!
//! Everything here needs the Accessibility permission (System Settings >
//! Privacy & Security > Accessibility): without it the event tap cannot be
//! created and posted keys are dropped.

use std::ffi::{c_char, c_void, CStr};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};

use crate::hook::{self, KeyEvent, Origin};
use crate::vk;

/// Marks input we injected as part of an operation: the hook ignores it.
const MAGIC: i64 = 0x5459_4231;
/// Marks replayed type-ahead: the hook tracks it but never queues it again.
const MAGIC_REPLAY: i64 = 0x5459_4232;

// Hardware key codes (positions on an ANSI/JIS keyboard).
const KC_C: u16 = 8;
const KC_V: u16 = 9;
const KC_J: u16 = 38;
const KC_RETURN: u16 = 36;
const KC_SPACE: u16 = 49;
const KC_DELETE: u16 = 51;
const KC_ESCAPE: u16 = 53;
const KC_LEFT: u16 = 123;
const KC_RIGHT: u16 = 124;

// CGEventFlags
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
/// Arrow keys carry these two (numeric pad + function).
const FLAG_ARROW: u64 = 0x0080_0000 | 0x0020_0000;

// CGEventType
const EV_LEFT_MOUSE_DOWN: u32 = 1;
const EV_RIGHT_MOUSE_DOWN: u32 = 3;
const EV_KEY_DOWN: u32 = 10;
const EV_KEY_UP: u32 = 11;
const EV_FLAGS_CHANGED: u32 = 12;
const EV_OTHER_MOUSE_DOWN: u32 = 25;
const EV_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
const EV_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

// CGEventField
const FIELD_KEYCODE: u32 = 9;
const FIELD_USER_DATA: u32 = 42;

type CGEventRef = *mut c_void;
type CFTypeRef = *const c_void;
type TapCallback = extern "C" fn(*mut c_void, u32, CGEventRef, *mut c_void) -> CGEventRef;

#[repr(C)]
struct Opaque {
    _private: [u8; 0],
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        mask: u64,
        callback: TapCallback,
        user: *mut c_void,
    ) -> *mut c_void;
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventGetFlags(event: CGEventRef) -> u64;
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventCreateKeyboardEvent(source: *const c_void, keycode: u16, down: bool) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopCommonModes: CFTypeRef;
    static kCFBooleanTrue: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: Opaque;
    static kCFTypeDictionaryValueCallBacks: Opaque;
    fn CFMachPortCreateRunLoopSource(allocator: *const c_void, port: *mut c_void, order: isize) -> *mut c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopAddSource(run_loop: *mut c_void, source: *mut c_void, mode: CFTypeRef);
    fn CFRunLoopRun();
    fn CFRelease(object: CFTypeRef);
    fn CFDictionaryCreate(
        allocator: *const c_void,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const Opaque,
        value_callbacks: *const Opaque,
    ) -> CFTypeRef;
    fn CFStringGetCString(string: CFTypeRef, buffer: *mut c_char, size: isize, encoding: u32) -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    static kAXTrustedCheckOptionPrompt: CFTypeRef;
    fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    static kTISPropertyInputSourceID: CFTypeRef;
    fn TISCopyCurrentKeyboardInputSource() -> CFTypeRef;
    fn TISGetInputSourceProperty(source: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
}

extern "C" {
    static _dispatch_main_q: Opaque;
    fn dispatch_sync_f(queue: *const Opaque, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

/// What to post to deliver a held-back key later.
pub struct QKey {
    keycode: u16,
    flags: u64,
    up: bool,
}

// ---- the event tap ----

static TAP: AtomicPtr<c_void> = AtomicPtr::new(null_mut());
static HOOKED: AtomicBool = AtomicBool::new(false);

/// Whether the event tap is in place (it is not until the user grants the
/// Accessibility permission).
pub fn hook_active() -> bool {
    HOOKED.load(Ordering::SeqCst)
}

/// Ask macOS to show its "grant Accessibility access" prompt (it only shows
/// when access has not been granted yet).
pub fn request_accessibility() -> bool {
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        let trusted = AXIsProcessTrustedWithOptions(options);
        if !options.is_null() {
            CFRelease(options);
        }
        trusted
    }
}

/// Install the event tap on a thread of its own. Without the Accessibility
/// permission the tap cannot be created; prompt once and keep trying, so the
/// app starts working the moment the user grants it.
pub fn install_hooks() {
    std::thread::spawn(|| {
        if !request_accessibility() {
            crate::log("accessibility permission missing: waiting for the user to grant it");
        }
        let mask: u64 = (1 << EV_KEY_DOWN)
            | (1 << EV_KEY_UP)
            | (1 << EV_FLAGS_CHANGED)
            | (1 << EV_LEFT_MOUSE_DOWN)
            | (1 << EV_RIGHT_MOUSE_DOWN)
            | (1 << EV_OTHER_MOUSE_DOWN);
        let tap = loop {
            // session tap, inserted at the head, able to swallow events
            let tap = unsafe { CGEventTapCreate(1, 0, 0, mask, tap_callback, null_mut()) };
            if !tap.is_null() {
                break tap;
            }
            sleep(Duration::from_secs(2));
        };
        TAP.store(tap, Ordering::SeqCst);
        unsafe {
            let source = CFMachPortCreateRunLoopSource(null(), tap, 0);
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
            CGEventTapEnable(tap, true);
        }
        HOOKED.store(true, Ordering::SeqCst);
        crate::log("hooks installed: event tap");
        unsafe { CFRunLoopRun() };
    });
}

/// Hardware key code -> vk.rs code. Positions, so a QWERTY layout is assumed.
fn vk_of(keycode: u16) -> u16 {
    const LETTERS: [(u16, u8); 26] = [
        (0, b'A'), (11, b'B'), (8, b'C'), (2, b'D'), (14, b'E'), (3, b'F'), (5, b'G'), (4, b'H'), (34, b'I'),
        (38, b'J'), (40, b'K'), (37, b'L'), (46, b'M'), (45, b'N'), (31, b'O'), (35, b'P'), (12, b'Q'),
        (15, b'R'), (1, b'S'), (17, b'T'), (32, b'U'), (9, b'V'), (13, b'W'), (7, b'X'), (16, b'Y'), (6, b'Z'),
    ];
    const DIGITS: [u16; 10] = [29, 18, 19, 20, 21, 23, 22, 26, 28, 25];
    if let Some((_, letter)) = LETTERS.iter().find(|(code, _)| *code == keycode) {
        return *letter as u16;
    }
    if let Some(digit) = DIGITS.iter().position(|code| *code == keycode) {
        return 0x30 + digit as u16;
    }
    match keycode {
        KC_RETURN | 76 => vk::RETURN, // 76: keypad Enter
        KC_SPACE => vk::SPACE,
        KC_DELETE => vk::BACK,
        KC_ESCAPE => vk::ESCAPE,
        // - = [ ] ' ; \ , / . ` ¥ _ and the keypad
        24 | 27 | 30 | 33 | 39 | 41 | 42 | 43 | 44 | 47 | 50 | 93 | 94 => vk::SYMBOL,
        65 | 67 | 69 | 75 | 78 | 81..=89 | 91 | 92 | 95 => vk::SYMBOL,
        56 => vk::LSHIFT,
        60 => vk::RSHIFT,
        59 => vk::LCONTROL,
        62 => vk::RCONTROL,
        58 => vk::LOPTION,
        61 => vk::ROPTION,
        55 => vk::LCOMMAND,
        54 => vk::RCOMMAND,
        _ => vk::OTHER,
    }
}

/// Whether the modifier with this key code is down, given the flags of its
/// flags-changed event.
fn modifier_down(keycode: u16, flags: u64) -> bool {
    // Left/right are told apart by the device-dependent bits.
    let (device, generic) = match keycode {
        56 => (0x02, FLAG_SHIFT),
        60 => (0x04, FLAG_SHIFT),
        59 => (0x01, FLAG_CONTROL),
        62 => (0x2000, FLAG_CONTROL),
        58 => (0x20, FLAG_OPTION),
        61 => (0x40, FLAG_OPTION),
        55 => (0x08, FLAG_COMMAND),
        54 => (0x10, FLAG_COMMAND),
        _ => return false,
    };
    if flags & 0x207F != 0 {
        flags & device != 0
    } else {
        flags & generic != 0 // no device bits on this event: best effort
    }
}

extern "C" fn tap_callback(_proxy: *mut c_void, etype: u32, event: CGEventRef, _user: *mut c_void) -> CGEventRef {
    match etype {
        EV_TAP_DISABLED_BY_TIMEOUT | EV_TAP_DISABLED_BY_USER_INPUT => {
            // macOS switches a slow tap off; switch it back on.
            let tap = TAP.load(Ordering::SeqCst);
            if !tap.is_null() {
                unsafe { CGEventTapEnable(tap, true) };
            }
            return event;
        }
        EV_LEFT_MOUSE_DOWN | EV_RIGHT_MOUSE_DOWN | EV_OTHER_MOUSE_DOWN => {
            hook::on_mouse_down();
            return event;
        }
        EV_KEY_DOWN | EV_KEY_UP | EV_FLAGS_CHANGED => {}
        _ => return event,
    }

    let (keycode, flags, user_data) = unsafe {
        (
            CGEventGetIntegerValueField(event, FIELD_KEYCODE) as u16,
            CGEventGetFlags(event),
            CGEventGetIntegerValueField(event, FIELD_USER_DATA),
        )
    };
    let vk = vk_of(keycode);
    let modifier = etype == EV_FLAGS_CHANGED;
    if modifier && !vk::is_modifier(vk) {
        return event; // Caps Lock, Fn
    }
    let up = if modifier { !modifier_down(keycode, flags) } else { etype == EV_KEY_UP };
    let swallow = hook::handle_key(KeyEvent {
        vk,
        up,
        origin: match user_data {
            MAGIC => Origin::Own,
            MAGIC_REPLAY => Origin::Replay,
            _ => Origin::User,
        },
        shift: flags & FLAG_SHIFT != 0,
        chord: flags & (FLAG_CONTROL | FLAG_OPTION | FLAG_COMMAND) != 0,
        raw: QKey { keycode, flags, up },
    });
    // Modifier changes always go through: swallowing them would leave the
    // system with a stale idea of what is held. Replayed keys carry their
    // own flags instead.
    if swallow && !modifier {
        null_mut()
    } else {
        event
    }
}

// ---- synthetic input ----

fn post(keycode: u16, down: bool, flags: u64, mark: i64) {
    unsafe {
        let event = CGEventCreateKeyboardEvent(null(), keycode, down);
        if event.is_null() {
            return;
        }
        CGEventSetFlags(event, flags);
        CGEventSetIntegerValueField(event, FIELD_USER_DATA, mark);
        CGEventPost(0, event); // HID level: passes through our own tap, marked
        CFRelease(event);
    }
}

fn stroke(keycode: u16, flags: u64) {
    post(keycode, true, flags, MAGIC);
    post(keycode, false, flags, MAGIC);
}

/// Press and release a key given by its vk.rs code (only the ones the engine uses).
pub fn tap(vk: u16) {
    match vk {
        vk::RETURN => stroke(KC_RETURN, 0),
        vk::RIGHT => stroke(KC_RIGHT, FLAG_ARROW),
        _ => {}
    }
}

/// Commit the composition as the kana it shows. Control+J turns it back
/// into hiragana first: with live conversion on (the default of the Apple
/// Japanese input method) the composition already shows kanji.
pub fn commit_composition() {
    stroke(KC_J, FLAG_CONTROL);
    sleep(Duration::from_millis(40));
    stroke(KC_RETURN, 0);
}

/// Extend the selection `n` characters to the left of the caret.
pub fn select_left(n: usize) {
    for _ in 0..n {
        stroke(KC_LEFT, FLAG_SHIFT | FLAG_ARROW);
        sleep(Duration::from_millis(1));
    }
}

/// Replay keys that were held back while an operation was running.
pub fn replay(keys: &[QKey]) {
    for key in keys {
        post(key.keycode, !key.up, key.flags, MAGIC_REPLAY);
        sleep(Duration::from_millis(1));
    }
}

// ---- clipboard ----

pub fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

pub fn set_clipboard_text(text: &str) -> bool {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text.to_string()))
        .is_ok()
}

fn clipboard_change_count() -> isize {
    autoreleasepool(|_| unsafe {
        let pasteboard: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
        if pasteboard.is_null() {
            return 0;
        }
        msg_send![pasteboard, changeCount]
    })
}

/// Command+C the current selection and return it, or None if nothing was copied.
pub fn copy_selection() -> Option<String> {
    let before = clipboard_change_count();
    stroke(KC_C, FLAG_COMMAND);
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        sleep(Duration::from_millis(10));
        if clipboard_change_count() != before {
            sleep(Duration::from_millis(15));
            return clipboard_text();
        }
    }
    None
}

/// Put `text` on the clipboard and Command+V it over the current selection.
pub fn paste(text: &str) -> bool {
    if !set_clipboard_text(text) {
        return false;
    }
    sleep(Duration::from_millis(30));
    stroke(KC_V, FLAG_COMMAND);
    sleep(Duration::from_millis(180));
    true
}

// ---- the app in front ----

unsafe fn string_of(ns_string: *mut AnyObject) -> Option<String> {
    if ns_string.is_null() {
        return None;
    }
    let utf8: *const c_char = msg_send![ns_string, UTF8String];
    if utf8.is_null() {
        return None;
    }
    Some(CStr::from_ptr(utf8).to_string_lossy().into_owned())
}

/// The app's bundle name without ".app", lowercased: "slack", "google chrome".
unsafe fn name_of(app: *mut AnyObject) -> Option<String> {
    if app.is_null() {
        return None;
    }
    let url: *mut AnyObject = msg_send![app, bundleURL];
    let mut name = None;
    if !url.is_null() {
        let last: *mut AnyObject = msg_send![url, lastPathComponent];
        name = string_of(last);
    }
    if name.is_none() {
        let localized: *mut AnyObject = msg_send![app, localizedName];
        name = string_of(localized);
    }
    let name = name?.to_lowercase();
    let name = name.strip_suffix(".app").unwrap_or(&name).trim().to_string();
    (!name.is_empty()).then_some(name)
}

unsafe fn frontmost() -> *mut AnyObject {
    let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
    if workspace.is_null() {
        return null_mut();
    }
    msg_send![workspace, frontmostApplication]
}

/// Identifies the app the user is typing in.
pub fn foreground_id() -> isize {
    autoreleasepool(|_| unsafe {
        let app = frontmost();
        if app.is_null() {
            return 0;
        }
        let pid: i32 = msg_send![app, processIdentifier];
        pid as isize
    })
}

/// Name of the app the user is typing in, e.g. "slack".
pub fn foreground_app() -> Option<String> {
    autoreleasepool(|_| unsafe { name_of(frontmost()) })
}

/// Names of the apps that are running with a Dock icon.
pub fn visible_apps() -> Vec<String> {
    let mut found: Vec<String> = autoreleasepool(|_| unsafe {
        let mut found = Vec::new();
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        if workspace.is_null() {
            return found;
        }
        let apps: *mut AnyObject = msg_send![workspace, runningApplications];
        if apps.is_null() {
            return found;
        }
        let count: usize = msg_send![apps, count];
        for i in 0..count {
            let app: *mut AnyObject = msg_send![apps, objectAtIndex: i];
            if app.is_null() {
                continue;
            }
            let policy: isize = msg_send![app, activationPolicy];
            if policy != 0 {
                continue; // not a regular app
            }
            if let Some(name) = name_of(app) {
                found.push(name);
            }
        }
        found
    });
    found.retain(|name| name != "yutas");
    found.sort();
    found.dedup();
    found
}

// ---- input source ----

/// Decide from an input source ID whether it types Japanese.
fn japanese_input(source_id: &str) -> Option<bool> {
    if source_id.contains(".keylayout.") || source_id.ends_with("Roman") {
        Some(false) // a plain keyboard layout, or the input method's 英字 mode
    } else if source_id.contains("Japanese") {
        Some(true)
    } else {
        None
    }
}

extern "C" fn read_input_source(context: *mut c_void) {
    let out = unsafe { &mut *(context as *mut Option<String>) };
    unsafe {
        let source = TISCopyCurrentKeyboardInputSource();
        if source.is_null() {
            return;
        }
        let id = TISGetInputSourceProperty(source, kTISPropertyInputSourceID);
        if !id.is_null() {
            let mut buffer = [0 as c_char; 256];
            const UTF8: u32 = 0x0800_0100;
            if CFStringGetCString(id, buffer.as_mut_ptr(), buffer.len() as isize, UTF8) {
                *out = Some(CStr::from_ptr(buffer.as_ptr()).to_string_lossy().into_owned());
            }
        }
        CFRelease(source);
    }
}

/// ID of the current keyboard input source, e.g.
/// "com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese".
pub fn input_source_id() -> Option<String> {
    let mut out: Option<String> = None;
    // The Text Input Sources API must be called on the main thread. This is
    // only ever called from the worker thread, so waiting cannot deadlock.
    unsafe { dispatch_sync_f(&_dispatch_main_q, &mut out as *mut Option<String> as *mut c_void, read_input_source) };
    out
}

/// Whether a Japanese input mode is selected. None when it cannot be told.
pub fn ime_open() -> Option<bool> {
    japanese_input(&input_source_id()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_codes_map_to_shared_codes() {
        assert_eq!(vk_of(0), b'A' as u16);
        assert_eq!(vk_of(6), b'Z' as u16);
        assert_eq!(vk_of(29), b'0' as u16);
        assert_eq!(vk_of(25), b'9' as u16);
        assert_eq!(vk_of(27), vk::SYMBOL); // the long-vowel mark is typed with '-'
        assert_eq!(vk_of(KC_DELETE), vk::BACK);
        assert_eq!(vk_of(56), vk::LSHIFT);
        assert_eq!(vk_of(60), vk::RSHIFT);
        assert_eq!(vk_of(KC_LEFT), vk::OTHER);
        assert_eq!(vk_of(102), vk::OTHER); // 英数
    }

    #[test]
    fn modifier_state_from_flags() {
        // left Shift down, then right Shift down as well, then left up
        assert!(modifier_down(56, FLAG_SHIFT | 0x02));
        assert!(modifier_down(60, FLAG_SHIFT | 0x02 | 0x04));
        assert!(!modifier_down(56, FLAG_SHIFT | 0x04));
        assert!(!modifier_down(56, 0x100)); // all released
        // events without device bits fall back to the generic mask
        assert!(modifier_down(59, FLAG_CONTROL));
        assert!(!modifier_down(59, 0));
    }

    #[test]
    fn input_sources() {
        assert_eq!(japanese_input("com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese"), Some(true));
        assert_eq!(japanese_input("com.apple.inputmethod.Kotoeri.RomajiTyping.Roman"), Some(false));
        assert_eq!(japanese_input("com.google.inputmethod.Japanese.base"), Some(true));
        assert_eq!(japanese_input("com.google.inputmethod.Japanese.Roman"), Some(false));
        assert_eq!(japanese_input("com.apple.keylayout.ABC"), Some(false));
        assert_eq!(japanese_input("com.example.something"), None);
    }

    #[test]
    fn frontmost_app_is_readable() {
        // Whatever is in front, these must not crash and must agree.
        let id = foreground_id();
        let name = foreground_app();
        assert_eq!(id != 0, name.is_some(), "id={id} name={name:?}");
        assert!(!visible_apps().iter().any(|a| a.ends_with(".app")));
    }
}
