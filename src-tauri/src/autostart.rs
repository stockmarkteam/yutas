//! Start at login: a LaunchAgent in ~/Library/LaunchAgents.
//! No admin rights needed, and it shows up under System Settings >
//! General > Login Items.

use std::path::{Path, PathBuf};

const LABEL: &str = "dev.yutas.app";

fn agent_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn plist(exe: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#,
        xml_escape(&exe.display().to_string())
    )
}

fn write(path: &Path, exe: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("ログイン項目のフォルダを作成できません: {e}"))?;
    }
    std::fs::write(path, plist(exe)).map_err(|e| format!("ログイン項目を登録できません: {e}"))
}

fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("ログイン項目を解除できません: {e}")),
    }
}

pub fn is_enabled() -> bool {
    agent_path().is_some_and(|p| p.exists())
}

pub fn set(enabled: bool) -> Result<(), String> {
    let path = agent_path().ok_or("ホームフォルダがわかりません")?;
    if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("実行ファイルの場所がわかりません: {e}"))?;
        write(&path, &exe)
    } else {
        remove(&path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_round_trip() {
        let dir = std::env::temp_dir().join(format!("yutas-autostart-test-{}", std::process::id()));
        let path = dir.join("LaunchAgents").join("test.plist");
        remove(&path).unwrap(); // removing a missing agent is fine
        write(&path, Path::new("/Applications/A & B/yutas.app/Contents/MacOS/yutas")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("<string>/Applications/A &amp; B/yutas.app/Contents/MacOS/yutas</string>"));
        assert!(text.contains("<key>RunAtLoad</key>"));
        remove(&path).unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
