mod autostart;
mod engine;
mod gateway;
mod hook;
mod keys;
mod llm;
mod pill;
mod romaji;
mod secrets;
mod settings;
mod vk;

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use settings::Settings;

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Where config.json lives: ~/Library/Application Support/yutas.
pub fn config_dir() -> PathBuf {
    home().map(|h| h.join("Library/Application Support")).unwrap_or_else(std::env::temp_dir).join("yutas")
}

/// Where yutas.log lives: ~/Library/Logs/yutas.
fn log_dir() -> Option<PathBuf> {
    Some(home()?.join("Library/Logs/yutas"))
}

/// Timings and outcomes go to yutas.log.
/// Typed text is only written when YUTAS_DEBUG is set.
pub fn log(message: &str) {
    if cfg!(test) {
        return; // unit tests must not write to the user's log
    }
    let Some(dir) = log_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("yutas.log")) {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(file, "[{secs}] {message}");
    }
}

pub fn debug() -> bool {
    std::env::var("YUTAS_DEBUG").is_ok()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    /// Gateway session for the saved gateway URL: "signed-in", "expired" or "none"
    session: &'static str,
    /// The signed-in account, when the gateway told us.
    email: String,
    /// The event tap is in place. False until the user grants the
    /// Accessibility permission.
    hook_active: bool,
    autostart: bool,
    config_path: String,
    mock: bool,
}

#[tauri::command]
fn get_settings() -> Settings {
    Settings::load()
}

#[tauri::command]
fn save_settings(settings: Settings) -> Result<(), String> {
    settings.save()?;
    hook::set_trigger(&settings.trigger);
    Ok(())
}

#[tauri::command]
fn get_status() -> Status {
    let (session, email) = gateway::status(gateway::URL);
    Status {
        session,
        email,
        hook_active: keys::hook_active(),
        autostart: autostart::is_enabled(),
        config_path: Settings::path().display().to_string(),
        mock: std::env::var("YUTAS_PROVIDER").is_ok_and(|v| v.trim() == "mock"),
    }
}

/// Run blocking work (network calls driven with block_on, sleeps) for an
/// async command. Tauri runs commands inside its async runtime, where
/// calling block_on directly panics and the command never answers; a
/// thread of the blocking pool is allowed to.
async fn off_runtime<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    match tauri::async_runtime::spawn_blocking(work).await {
        Ok(result) => result,
        Err(e) => {
            log(&format!("command failed: {e}"));
            Err("内部エラーが発生しました".into())
        }
    }
}

/// Bumped to abandon a sign-in that is waiting for the browser.
static LOGIN_ATTEMPT: AtomicU64 = AtomicU64::new(0);

fn open_in_browser(url: &str) -> Result<(), String> {
    // Only ever a web page: this string comes from the network.
    gateway::check_url(url)?;
    let spawned = std::process::Command::new("/usr/bin/open").arg(url).spawn();
    spawned.map(|_| ()).map_err(|e| format!("ブラウザを開けません: {e}"))
}

/// Start signing in to the gateway: opens the sign-in page in the browser,
/// and returns what gateway_login_wait needs.
#[tauri::command]
async fn gateway_login_start() -> Result<gateway::Login, String> {
    off_runtime(|| {
        let login = tauri::async_runtime::block_on(gateway::start_login(gateway::URL)).inspect_err(|e| {
            log(&format!("gateway sign-in could not start: {e}"));
        })?;
        LOGIN_ATTEMPT.fetch_add(1, Ordering::SeqCst);
        if let Err(e) = open_in_browser(&login.verification_uri) {
            log(&format!("gateway sign-in: {e}"));
        }
        Ok(login)
    })
    .await
}

/// Wait until the user has signed in in the browser. Returns the account's e-mail address.
#[tauri::command]
async fn gateway_login_wait(login: gateway::Login) -> Result<String, String> {
    off_runtime(move || {
        let attempt = LOGIN_ATTEMPT.load(Ordering::SeqCst);
        let result = tauri::async_runtime::block_on(gateway::finish_login(gateway::URL, &login, &|| {
            LOGIN_ATTEMPT.load(Ordering::SeqCst) != attempt
        }));
        match &result {
            Ok(_) => log("gateway sign-in: ok"),
            Err(e) => log(&format!("gateway sign-in failed: {e}")),
        }
        result
    })
    .await
}

#[tauri::command]
fn gateway_login_cancel() {
    LOGIN_ATTEMPT.fetch_add(1, Ordering::SeqCst);
}

#[tauri::command]
fn gateway_sign_out() -> Result<(), String> {
    LOGIN_ATTEMPT.fetch_add(1, Ordering::SeqCst);
    gateway::sign_out()
}

/// Open the Accessibility pane of System Settings (and let macOS add yutas
/// to its list). With `reset`, first forget what macOS has recorded for
/// yutas: the list can show yutas as allowed while the entry really belongs
/// to a differently signed build, and only removing it helps.
#[tauri::command]
fn request_accessibility(reset: bool) -> Result<(), String> {
    if reset {
        let done = std::process::Command::new("/usr/bin/tccutil")
            .args(["reset", "Accessibility", BUNDLE_ID])
            .status()
            .map_err(|e| format!("許可をリセットできません: {e}"))?;
        log(&format!("accessibility reset: {done}"));
        if !done.success() {
            return Err("許可をリセットできません。一覧から yutas を「−」で削除してください".into());
        }
    }
    keys::request_accessibility();
    let _ = std::process::Command::new("/usr/bin/open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
        .spawn();
    Ok(())
}

/// Must match `identifier` in tauri.conf.json.
const BUNDLE_ID: &str = "dev.yutas.app";

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<(), String> {
    autostart::set(enabled)
}

#[tauri::command]
fn list_apps() -> Vec<String> {
    keys::visible_apps()
}

#[tauri::command]
async fn list_local_models(url: String) -> Result<Vec<String>, String> {
    off_runtime(move || tauri::async_runtime::block_on(llm::local_models(&url))).await
}

/// Try the given (possibly unsaved) settings on a sample, without touching any document.
#[tauri::command]
async fn preview(settings: Settings, input: String, app: Option<String>) -> Result<String, String> {
    off_runtime(move || {
        let input = input.trim();
        if input.is_empty() {
            return Err("試す文章を入力してください".into());
        }
        let (tone, _) = settings.tone_for(app.as_deref());
        let has_kanji = input.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c));
        let mode = if has_kanji || input.contains('\n') { llm::Mode::Selection } else { llm::Mode::Kana };
        tauri::async_runtime::block_on(llm::polish(&settings, &tone, mode, input, &mut |_| {}, &|| false))
    })
    .await
}

/// `tab` opens the window on that tab: "tone", "dict" or "general".
fn open_settings(app: &AppHandle, tab: Option<&str>) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }
    let page = match tab {
        Some(tab) if matches!(tab, "tone" | "dict" | "general") => format!("settings.html#{tab}"),
        _ => "settings.html".to_string(),
    };
    let built = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App(page.into()))
        .title("yutas 設定")
        .inner_size(860.0, 720.0)
        .min_inner_size(640.0, 480.0)
        .build();
    if let Err(e) = built {
        log(&format!("could not open settings window: {e}"));
    }
}

/// Two instances would mean two event taps fighting over every key.
fn already_running() -> bool {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("yutas.lock"))
    else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            std::mem::forget(file); // held until the process exits
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

fn main() {
    if already_running() {
        return;
    }
    // --settings opens the settings window at startup; --settings=general picks a tab.
    let settings_arg = std::env::args().find(|a| a == "--settings" || a.starts_with("--settings="));

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            get_status,
            gateway_login_start,
            gateway_login_wait,
            gateway_login_cancel,
            gateway_sign_out,
            request_accessibility,
            set_autostart,
            list_apps,
            list_local_models,
            preview
        ])
        .setup(move |app| {
            // A background tool: no Dock icon, never the frontmost app.
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            pill::init(&handle);

            let open = MenuItem::with_id(app, "settings", "設定…", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "yutas を終了", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;
            // A one-colour template image, which the menu bar tints like
            // every other status item (light, dark, highlighted).
            let tray = TrayIconBuilder::new()
                .icon(tauri::include_image!("icons/tray-template.png"))
                .icon_as_template(true)
                .tooltip("yutas")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "settings" => open_settings(app, None),
                    "quit" => app.exit(0),
                    _ => {}
                });
            tray.build(app)?;

            let settings = Settings::load();
            log(&format!(
                "started: provider={} model={} session={} dict={} app_rules={}",
                llm::provider(&settings),
                settings.active_model(),
                gateway::status(gateway::URL).0,
                settings.dictionary.len(),
                settings.app_rules.len()
            ));
            hook::set_trigger(&settings.trigger);

            let (tx, rx) = mpsc::channel();
            hook::start(tx);
            let worker = handle.clone();
            std::thread::spawn(move || {
                for job in rx {
                    engine::handle(&worker, job);
                }
            });

            if let Some(arg) = &settings_arg {
                open_settings(&handle, arg.strip_prefix("--settings="));
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run yutas");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::async_runtime::{block_on, spawn};

    /// Tauri runs every command as a task on its async runtime. This is why
    /// the commands go through off_runtime: block_on straight from such a
    /// task panics, and the caller in the settings window waits forever.
    #[test]
    fn block_on_inside_a_command_task_panics() {
        let direct = block_on(spawn(async { block_on(async { 1 }) }));
        assert!(direct.is_err(), "block_on on a runtime thread is expected to panic");
    }

    #[test]
    fn commands_run_blocking_work_off_the_runtime() {
        let answered = block_on(spawn(async { off_runtime(|| Ok(block_on(async { 1 }))).await }));
        assert_eq!(answered.unwrap(), Ok(1));
        // The commands themselves, driven the way Tauri drives them.
        let models = block_on(spawn(list_local_models("http://127.0.0.1:9/v1".into()))).unwrap();
        assert!(models.unwrap_err().contains("接続できません"));
        std::env::set_var("YUTAS_PROVIDER", "mock");
        let preview = block_on(spawn(preview(Settings::default(), "あした".into(), None))).unwrap();
        std::env::remove_var("YUTAS_PROVIDER");
        assert_eq!(preview.unwrap(), "【清書】あした");
    }
}
