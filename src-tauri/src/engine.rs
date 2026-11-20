//! The operation behind a double tap. Runs on the worker thread, never on
//! the hook. Three cases, in order:
//!   1. kana is being composed  -> commit it, read it back, polish, replace
//!   2. the last result is still untouched before the caret -> undo it
//!   3. otherwise -> polish the current selection, if there is one

use std::thread::sleep;
use std::time::{Duration, Instant};

use tauri::AppHandle;

use crate::hook::{self, Job, Last};
use crate::llm::Mode;
use crate::settings::Settings;
use crate::vk::RIGHT as VK_RIGHT;
use crate::{keys, llm, log, pill, romaji};

/// Selections longer than this are refused rather than sent off by accident.
const MAX_SELECTION_CHARS: usize = 4000;

pub fn handle(app: &AppHandle, job: Job) {
    match job {
        Job::Trigger => trigger(app),
    }
    hook::finish_operation();
}

fn trigger(app: &AppHandle) {
    // The trigger fires from inside the event tap, on the release that
    // completes the double tap. Let that release land before sending keys,
    // or it would cut our own Shift+Left selection short.
    sleep(Duration::from_millis(40));

    let (expected, untracked, undo) = {
        let st = hook::state();
        let undo = if st.undo_armed { st.last.clone() } else { None };
        (st.comp.finalized(), st.dirty || st.converting, undo)
    };

    if expected.is_empty() {
        match undo {
            Some(last) => undo_last(app, last),
            None => polish_selection(app),
        }
        return;
    }
    if untracked {
        log("abort: composition not tracked (caret moved or converting)");
        pill::flash(app, "error", "入力を追跡できませんでした", "変換・カーソル移動の後は未対応", 2200);
        return;
    }
    if keys::ime_open() == Some(false) && std::env::var("YUTAS_SKIP_IME_CHECK").is_err() {
        log("abort: IME reported off");
        hook::state().reset_composition();
        pill::flash(app, "info", "IMEがオフです", "", 1400);
        return;
    }

    let started = Instant::now();
    pill::show(app, "working", "清書中…", "");

    // Commit the kana as-is, then select it again and read it back.
    keys::commit_composition();
    sleep(Duration::from_millis(70));
    keys::select_left(expected.len());
    sleep(Duration::from_millis(50));
    let saved_clipboard = keys::clipboard_text();
    let copied = keys::copy_selection();

    match copied {
        Some(text) if romaji::matches(&expected, &text) => {
            polish_and_replace(app, Mode::Kana, text, saved_clipboard, started);
        }
        other => {
            let got = other.as_ref().map(|t| t.chars().count());
            log(&format!("abort: read-back mismatch, expected {} chars, got {:?}", expected.len(), got));
            if crate::debug() {
                log(&format!("  expected={:?} got={:?}", expected.iter().collect::<String>(), other));
            }
            keys::tap(VK_RIGHT);
            restore_clipboard(saved_clipboard);
            hook::state().reset_composition();
            pill::flash(app, "error", "入力を読み取れませんでした", "ひらがなは確定済みです", 2200);
        }
    }
}

/// Nothing is being composed: polish whatever the user has selected.
fn polish_selection(app: &AppHandle) {
    let started = Instant::now();
    let saved_clipboard = keys::clipboard_text();
    let selected = keys::copy_selection().map(|t| t.replace('\r', ""));
    match selected {
        Some(text) if text.trim().is_empty() => {
            restore_clipboard(saved_clipboard);
            pill::flash(app, "info", "清書する入力がありません", "", 1400);
        }
        Some(text) if text.chars().count() > MAX_SELECTION_CHARS => {
            restore_clipboard(saved_clipboard);
            log(&format!("abort: selection too long ({} chars)", text.chars().count()));
            pill::flash(app, "error", "選択範囲が長すぎます", "4000文字まで", 2200);
        }
        Some(text) => {
            pill::show(app, "working", "選択範囲を清書中…", "");
            polish_and_replace(app, Mode::Selection, text, saved_clipboard, started);
        }
        None => pill::flash(app, "info", "清書する入力がありません", "", 1400),
    }
}

/// `source` is selected in the document. Polish it and paste over it.
fn polish_and_replace(app: &AppHandle, mode: Mode, source: String, saved_clipboard: Option<String>, started: Instant) {
    let settings = Settings::load();
    let (tone, rule_app) = settings.tone_for(keys::foreground_app().as_deref());
    // Tell the user when an app-specific tone is in effect.
    let hint = rule_app.as_deref().map(|app| format!("{app} の口調")).unwrap_or_default();
    if !hint.is_empty() {
        pill::update(app, "working", "清書中…", &hint);
    }
    let t_read = started.elapsed();
    let mut first_text: Option<Duration> = None;
    let result = tauri::async_runtime::block_on(llm::polish(
        &settings,
        &tone,
        mode,
        &source,
        &mut |text| {
            first_text.get_or_insert_with(|| started.elapsed());
            pill::update(app, "working", text, &hint);
        },
        &|| hook::state().cancel,
    ));
    let t_llm = started.elapsed();
    let cancelled = hook::state().cancel;
    // A selection made by the user stays as it was on failure; our own
    // selection of the committed kana is dropped.
    let drop_selection = || {
        if mode == Mode::Kana {
            keys::tap(VK_RIGHT);
        }
    };
    let kept = if mode == Mode::Kana { "ひらがなは確定済みです" } else { "選択範囲はそのままです" };

    match result {
        Ok(text) if !cancelled => {
            let pasted = keys::paste(&text);
            restore_clipboard(saved_clipboard);
            {
                let mut st = hook::state();
                st.reset_composition();
                st.last = Some(Last { original: source.clone(), result: text.clone() });
                st.undo_armed = pasted;
            }
            log(&format!(
                "ok: mode={} provider={} model={} tone={} dict={} in={} out={} chars | read={}ms first_text={}ms llm_done={}ms total={}ms",
                if mode == Mode::Kana { "kana" } else { "selection" },
                llm::provider(&settings),
                settings.active_model(),
                rule_app.as_deref().unwrap_or("default"),
                settings.dictionary.len(),
                source.chars().count(),
                text.chars().count(),
                t_read.as_millis(),
                first_text.unwrap_or_default().as_millis(),
                t_llm.as_millis(),
                started.elapsed().as_millis(),
            ));
            if crate::debug() {
                log(&format!("  {source:?} -> {text:?}"));
            }
            pill::flash(app, "done", &text, "もう一度2回押しで戻す", 1600);
        }
        Ok(_) => {
            drop_selection();
            restore_clipboard(saved_clipboard);
            hook::state().reset_composition();
            log("cancelled by user");
            pill::flash(app, "info", "キャンセルしました", "", 1200);
        }
        Err(message) => {
            drop_selection();
            restore_clipboard(saved_clipboard);
            hook::state().reset_composition();
            log(&format!("error after {}ms: {message}", t_llm.as_millis()));
            pill::flash(app, "error", &message, kept, 2600);
        }
    }
}

/// Put the original text back in place of the last result.
fn undo_last(app: &AppHandle, last: Last) {
    let saved_clipboard = keys::clipboard_text();
    // A line break is one caret step, however the document stores it.
    keys::select_left(last.result.chars().filter(|c| *c != '\r').count());
    sleep(Duration::from_millis(50));
    let copied = keys::copy_selection().map(|t| t.replace('\r', ""));
    if copied.as_deref() == Some(last.result.as_str()) {
        keys::paste(&last.original);
        log("undo: restored original");
        pill::flash(app, "info", "元に戻しました", "", 1200);
    } else {
        keys::tap(VK_RIGHT);
        log("undo: text before caret is no longer the last result");
        pill::flash(app, "error", "元に戻せませんでした", "", 1800);
    }
    restore_clipboard(saved_clipboard);
    let mut st = hook::state();
    st.last = None;
    st.undo_armed = false;
}

fn restore_clipboard(saved: Option<String>) {
    if let Some(text) = saved {
        keys::set_clipboard_text(&text);
    }
}
