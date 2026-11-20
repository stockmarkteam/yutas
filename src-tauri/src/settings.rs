//! User settings, stored as JSON in
//! ~/Library/Application Support/yutas/config.json.
//! Read on every operation, so a save takes effect at once. The gateway
//! session is deliberately not kept here: see secrets.rs.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The one Claude model in use: the gateway maps it to Haiku 5.5 on Bedrock
/// in Japan (`jp.anthropic.claude-haiku-5-5`). Not a setting.
pub const CLAUDE_MODEL: &str = "claude-haiku-5-5";
pub const DEFAULT_LOCAL_URL: &str = "http://localhost:11434/v1";
pub const DEFAULT_TONE: &str =
    "です・ます調の、簡潔で丁寧なビジネスチャットの文体。文意は変えず、語尾や言い回しだけを整える。";

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct DictEntry {
    /// What the user types: a reading in kana, or a shorthand.
    pub reading: String,
    /// What it must become.
    pub word: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct AppRule {
    /// App name as keys::foreground_app reports it: the bundle name,
    /// lower-cased ("slack", "google chrome").
    pub app: String,
    pub tone: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub trigger: String,
    /// "gateway" (Claude through the company's Claude apps gateway) or
    /// "local" (an OpenAI-compatible server on this machine or network,
    /// such as Ollama or LM Studio).
    pub provider: String,
    pub local_url: String,
    pub local_model: String,
    pub tone: String,
    pub extra_instructions: String,
    pub dictionary: Vec<DictEntry>,
    pub app_rules: Vec<AppRule>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            trigger: "shift".into(),
            provider: "gateway".into(),
            local_url: DEFAULT_LOCAL_URL.into(),
            local_model: String::new(),
            tone: DEFAULT_TONE.into(),
            extra_instructions: String::new(),
            dictionary: Vec::new(),
            app_rules: Vec::new(),
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        crate::config_dir().join("config.json")
    }

    pub fn load() -> Settings {
        let Ok(text) = std::fs::read_to_string(Self::path()) else {
            return Settings::default();
        };
        match serde_json::from_str::<Settings>(&text) {
            Ok(settings) => settings.cleaned(),
            Err(e) => {
                crate::log(&format!("config.json is invalid, using defaults: {e}"));
                Settings::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("設定フォルダを作成できません: {e}"))?;
        }
        let json = serde_json::to_string_pretty(&self.clone().cleaned()).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("設定を保存できません: {e}"))
    }

    /// Trim, drop empty rows, and fall back to defaults for blank essentials.
    fn cleaned(mut self) -> Settings {
        // Anything else (including "anthropic" from older versions, which
        // called the Claude API directly) becomes the gateway.
        if self.provider != "local" {
            self.provider = "gateway".into();
        }
        self.local_url = self.local_url.trim().trim_end_matches('/').to_string();
        if self.local_url.is_empty() {
            self.local_url = DEFAULT_LOCAL_URL.into();
        }
        self.local_model = self.local_model.trim().to_string();
        if self.tone.trim().is_empty() {
            self.tone = DEFAULT_TONE.into();
        }
        for entry in &mut self.dictionary {
            entry.reading = entry.reading.trim().to_string();
            entry.word = entry.word.trim().to_string();
        }
        self.dictionary.retain(|e| !e.reading.is_empty() && !e.word.is_empty());
        for rule in &mut self.app_rules {
            rule.app = rule.app.trim().to_lowercase();
        }
        self.app_rules.retain(|r| !r.app.is_empty() && !r.tone.trim().is_empty());
        self
    }

    /// The model the active provider will use.
    pub fn active_model(&self) -> &str {
        if self.provider == "local" {
            &self.local_model
        } else {
            CLAUDE_MODEL
        }
    }

    /// The tone for the given executable, and the rule's app name if one matched.
    pub fn tone_for(&self, exe: Option<&str>) -> (String, Option<String>) {
        if let Some(exe) = exe {
            let exe = exe.to_lowercase();
            if let Some(rule) = self.app_rules.iter().find(|r| r.app.to_lowercase() == exe) {
                return (rule.tone.clone(), Some(rule.app.clone()));
            }
        }
        (self.tone.clone(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_rule_overrides_default_tone() {
        let settings = Settings {
            tone: "default".into(),
            app_rules: vec![AppRule { app: "slack".into(), tone: "casual".into() }],
            ..Settings::default()
        };
        assert_eq!(settings.tone_for(Some("Slack")), ("casual".into(), Some("slack".into())));
        assert_eq!(settings.tone_for(Some("notes")), ("default".into(), None));
        assert_eq!(settings.tone_for(None), ("default".into(), None));
    }

    #[test]
    fn partial_json_and_empty_rows() {
        let json = r#"{ "tone": " ", "provider": "openai", "localUrl": "http://localhost:1234/v1/",
            "dictionary": [ {"reading": "mtg", "word": "ミーティング"}, {"reading": "", "word": "x"} ] }"#;
        let settings = serde_json::from_str::<Settings>(json).unwrap().cleaned();
        assert_eq!(settings.tone, DEFAULT_TONE);
        assert_eq!(settings.dictionary.len(), 1);
        assert_eq!(settings.provider, "gateway");
        assert_eq!(settings.local_url, "http://localhost:1234/v1");
    }

    /// A config.json written by a version that called the Claude API directly.
    #[test]
    fn old_config_moves_to_the_gateway() {
        let json = r#"{ "provider": "anthropic", "model": "claude-opus-5-5", "workspaceId": "wrkspc_1",
            "gatewayUrl": "https://elsewhere.example.com" }"#;
        let settings = serde_json::from_str::<Settings>(json).unwrap().cleaned();
        assert_eq!(settings.provider, "gateway");
        // The model and the gateway are fixed: old choices are dropped, not obeyed.
        assert_eq!(settings.active_model(), CLAUDE_MODEL);
        let saved = serde_json::to_string(&settings).unwrap();
        assert!(!saved.contains("workspace") && !saved.contains("opus") && !saved.contains("elsewhere"));
    }

    #[test]
    fn active_model_follows_provider() {
        let mut settings = Settings { local_model: "gemma3".into(), ..Settings::default() };
        assert_eq!(settings.active_model(), CLAUDE_MODEL);
        settings.provider = "local".into();
        assert_eq!(settings.active_model(), "gemma3");
    }
}
