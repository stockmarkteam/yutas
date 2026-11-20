//! The gateway session (bearer and refresh token). It is kept in the
//! Keychain, never in config.json.

const SERVICE: &str = "yutas";
#[cfg_attr(test, allow(dead_code))]
const ACCOUNT: &str = "gateway-session";

fn entry_for(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, account).map_err(|e| format!("キーチェーンを開けません: {e}"))
}

/// The stored session as the JSON text gateway.rs wrote, if any.
#[cfg(not(test))]
pub fn load() -> Option<String> {
    entry_for(ACCOUNT).ok()?.get_password().ok().filter(|k| !k.trim().is_empty())
}

#[cfg(not(test))]
pub fn store(session: &str) -> Result<(), String> {
    entry_for(ACCOUNT)?.set_password(session).map_err(|e| format!("サインイン情報を保存できません: {e}"))
}

#[cfg(not(test))]
pub fn delete() -> Result<(), String> {
    match entry_for(ACCOUNT)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("サインイン情報を削除できません: {e}")),
    }
}

/// Unit tests never touch the real credential store: they share this slot.
#[cfg(test)]
pub use in_memory::{delete, load, store};

#[cfg(test)]
mod in_memory {
    use std::sync::Mutex;

    static SLOT: Mutex<Option<String>> = Mutex::new(None);

    pub fn load() -> Option<String> {
        SLOT.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn store(session: &str) -> Result<(), String> {
        *SLOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.to_string());
        Ok(())
    }

    pub fn delete() -> Result<(), String> {
        *SLOT.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches the real credential store (under a throwaway account), so it
    /// only runs on request: cargo test -- --ignored
    #[test]
    #[ignore]
    fn credential_store_round_trip() {
        let entry = entry_for("selftest").unwrap();
        entry.set_password("sk-test-123").unwrap();
        assert_eq!(entry_for("selftest").unwrap().get_password().unwrap(), "sk-test-123");
        entry.delete_credential().unwrap();
        assert!(matches!(entry_for("selftest").unwrap().get_password(), Err(keyring::Error::NoEntry)));
    }
}
