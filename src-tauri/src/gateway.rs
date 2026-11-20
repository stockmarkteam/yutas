//! Signing in to a Claude apps gateway and keeping the session fresh.
//!
//! The gateway is its own OAuth authorization server: a client starts a
//! device-code grant (RFC 8628), the user signs in with the company IdP in
//! the browser, and the gateway hands out a short-lived bearer plus a
//! refresh token. Clients send no client ID. Inference then goes to
//! `<gateway>/v1/messages` with that bearer (see llm.rs).

use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::secrets;

/// The company's Claude apps gateway. Fixed: it is the only place Claude
/// requests and the sign-in session may go.
pub const URL: &str = "https://ax.stockmark.co.jp";

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Refresh this long before the bearer expires.
const REFRESH_MARGIN_SECS: u64 = 120;

/// The gateway's sign-in endpoints must never be reached through a redirect.
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("http client")
});

/// Serialises refreshes: a refresh token may be single-use.
static REFRESH: LazyLock<refresh_lock::Lock> = LazyLock::new(refresh_lock::Lock::default);

/// A lock that can be held across an await. Callers are worker threads
/// blocking on the request, so waiting by sleeping is fine.
mod refresh_lock {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[derive(Default)]
    pub struct Lock(AtomicBool);
    pub struct Guard<'a>(&'a Lock);

    impl Lock {
        pub fn lock(&self) -> Guard<'_> {
            while self.0.swap(true, Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(20));
            }
            Guard(self)
        }
    }
    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            self.0 .0.store(false, Ordering::Release);
        }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Bearers must not travel in the clear: https only, except to this machine.
pub fn check_url(url: &str) -> Result<(), String> {
    let rest = url.strip_prefix("https://").map(|r| (r, true)).or_else(|| url.strip_prefix("http://").map(|r| (r, false)));
    let Some((rest, tls)) = rest else {
        return Err("ゲートウェイのURLは https:// で始めてください".into());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace) {
        return Err("ゲートウェイのURLが正しくありません".into());
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    if !tls && !matches!(host, "localhost" | "127.0.0.1" | "::1") {
        return Err("ゲートウェイのURLは https:// で始めてください（http は localhost のみ）".into());
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Session {
    /// The gateway this session belongs to: it is never sent anywhere else.
    gateway: String,
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    /// Unix seconds.
    expires_at: u64,
    #[serde(default)]
    email: String,
}

fn load_session(gateway: &str) -> Option<Session> {
    let session: Session = serde_json::from_str(&secrets::load()?).ok()?;
    (session.gateway == gateway && !session.access_token.is_empty()).then_some(session)
}

fn save_session(session: &Session) -> Result<(), String> {
    secrets::store(&serde_json::to_string(session).map_err(|e| e.to_string())?)
}

/// "signed-in", "expired" (signed in before, but a new sign-in is needed)
/// or "none"; and the account's e-mail address when the gateway told us.
pub fn status(gateway: &str) -> (&'static str, String) {
    match load_session(gateway) {
        Some(s) if s.expires_at > now() || !s.refresh_token.is_empty() => ("signed-in", s.email),
        Some(s) => ("expired", s.email),
        None => ("none", String::new()),
    }
}

pub fn sign_out() -> Result<(), String> {
    secrets::delete()
}

fn transport_error(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "ゲートウェイの応答がタイムアウトしました".to_string()
    } else if e.is_connect() {
        "ゲートウェイに接続できません（URL・ネットワークを確認）".to_string()
    } else {
        format!("通信エラー: {e}")
    }
}

/// What the user needs to finish signing in, and what we need to wait for it.
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Login {
    pub user_code: String,
    /// The page to open: the code is already filled in when the gateway offers that.
    pub verification_uri: String,
    pub device_code: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// Start a device-code sign-in.
pub async fn start_login(gateway: &str) -> Result<Login, String> {
    check_url(gateway)?;
    let resp = HTTP
        .post(format!("{gateway}/oauth/device_authorization"))
        .header("content-length", "0")
        .send()
        .await
        .map_err(transport_error)?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(match status.as_u16() {
            429 => "サインインの試行が多すぎます。数分待ってください".to_string(),
            403 => "ゲートウェイに拒否されました（接続元が許可されていない可能性）".to_string(),
            404 => "このURLは Claude apps gateway ではないようです".to_string(),
            code => format!("サインインを開始できません (HTTP {code})"),
        });
    }
    let v: Value = serde_json::from_str(&body).map_err(|_| "ゲートウェイの応答を読み取れません".to_string())?;
    let text = |key: &str| v[key].as_str().unwrap_or("").to_string();
    let login = Login {
        user_code: text("user_code"),
        verification_uri: Some(text("verification_uri_complete")).filter(|u| !u.is_empty()).unwrap_or_else(|| text("verification_uri")),
        device_code: text("device_code"),
        interval: v["interval"].as_u64().unwrap_or(5).clamp(1, 60),
        expires_in: v["expires_in"].as_u64().unwrap_or(600).clamp(30, 3600),
    };
    if login.device_code.is_empty() || login.user_code.is_empty() {
        return Err("ゲートウェイの応答を読み取れません".into());
    }
    // The page is opened in the user's browser: it has to be a web page.
    check_url(&login.verification_uri).map_err(|_| "ゲートウェイが示したサインイン先が正しくありません".to_string())?;
    Ok(login)
}

enum Grant {
    Session(Session),
    /// An OAuth error code such as "authorization_pending".
    Refused(String),
}

/// One call to the token endpoint.
async fn token_request(gateway: &str, form: &[(&str, &str)]) -> Result<Grant, String> {
    let resp = HTTP.post(format!("{gateway}/oauth/token")).form(form).send().await.map_err(transport_error)?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if status.is_success() {
        let access_token = v["access_token"].as_str().unwrap_or("").to_string();
        if access_token.is_empty() {
            return Err("ゲートウェイの応答にトークンがありません".into());
        }
        return Ok(Grant::Session(Session {
            gateway: gateway.to_string(),
            access_token,
            refresh_token: v["refresh_token"].as_str().unwrap_or("").to_string(),
            expires_at: now() + v["expires_in"].as_u64().unwrap_or(3600),
            email: v["email"].as_str().unwrap_or("").to_string(),
        }));
    }
    match v["error"].as_str() {
        Some(code) => Ok(Grant::Refused(code.to_string())),
        None => Err(format!("サインインに失敗しました (HTTP {})", status.as_u16())),
    }
}

/// Wait for the user to finish signing in in the browser, then store the session.
/// Returns the account's e-mail address (empty when the gateway does not say).
pub async fn finish_login(gateway: &str, login: &Login, cancelled: &dyn Fn() -> bool) -> Result<String, String> {
    check_url(gateway)?;
    let deadline = now() + login.expires_in;
    let mut interval = login.interval;
    loop {
        for _ in 0..interval * 4 {
            if cancelled() {
                return Err("サインインをやめました".into());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if now() >= deadline {
            return Err("時間切れです。もう一度サインインしてください".into());
        }
        let form = [("grant_type", DEVICE_GRANT), ("device_code", login.device_code.as_str())];
        match token_request(gateway, &form).await? {
            Grant::Session(session) => {
                save_session(&session)?;
                return Ok(session.email);
            }
            Grant::Refused(code) => match code.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval += 5,
                "expired_token" => return Err("時間切れです。もう一度サインインしてください".into()),
                "access_denied" => return Err("サインインが拒否されました（このアカウントは許可されていません）".into()),
                other => return Err(format!("サインインに失敗しました ({other})")),
            },
        }
    }
}

const SIGN_IN_AGAIN: &str = "ゲートウェイへのサインインが必要です（設定画面でサインイン）";

/// Trade the refresh token for a new session. `stale` is the bearer that is
/// known to be old: if another caller already replaced it, use theirs.
async fn refresh(gateway: &str, stale: &str) -> Result<String, String> {
    let _guard = REFRESH.lock();
    let session = load_session(gateway).ok_or(SIGN_IN_AGAIN)?;
    if session.access_token != stale && session.expires_at > now() + REFRESH_MARGIN_SECS {
        return Ok(session.access_token);
    }
    if session.refresh_token.is_empty() {
        return Err(SIGN_IN_AGAIN.into());
    }
    let form = [("grant_type", "refresh_token"), ("refresh_token", session.refresh_token.as_str())];
    match token_request(gateway, &form).await? {
        Grant::Session(mut fresh) => {
            // A gateway may keep the refresh token and not repeat the e-mail.
            if fresh.refresh_token.is_empty() {
                fresh.refresh_token = session.refresh_token;
            }
            if fresh.email.is_empty() {
                fresh.email = session.email;
            }
            save_session(&fresh)?;
            Ok(fresh.access_token)
        }
        Grant::Refused(code) => {
            // The account was removed or the session revoked: stop offering it.
            crate::log(&format!("gateway refresh refused: {code}"));
            let _ = save_session(&Session { refresh_token: String::new(), expires_at: 0, ..session });
            Err(SIGN_IN_AGAIN.into())
        }
    }
}

/// A bearer for `gateway` that is good for at least the next couple of minutes.
pub async fn bearer(gateway: &str) -> Result<String, String> {
    check_url(gateway)?;
    let session = load_session(gateway).ok_or(SIGN_IN_AGAIN)?;
    if session.expires_at > now() + REFRESH_MARGIN_SECS {
        return Ok(session.access_token);
    }
    match refresh(gateway, &session.access_token).await {
        Ok(token) => Ok(token),
        // Offline for a moment, but the bearer has not run out yet: use it.
        Err(e) if e != SIGN_IN_AGAIN && session.expires_at > now() => Ok(session.access_token),
        Err(e) => Err(e),
    }
}

/// The gateway rejected `stale` although it had not expired by our clock.
pub async fn bearer_after_rejection(gateway: &str, stale: &str) -> Result<String, String> {
    refresh(gateway, stale).await
}

/// A throwaway HTTP server for tests: one request per connection.
#[cfg(test)]
pub mod testserver {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex, MutexGuard};

    /// Tests that use the (shared, in-memory) session store take this first.
    pub fn session_lock() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[derive(Clone, Debug)]
    pub struct Request {
        pub method: String,
        pub path: String,
        /// Header lines, names lower-cased.
        pub headers: Vec<String>,
        pub body: String,
    }

    impl Request {
        pub fn header(&self, name: &str) -> Option<String> {
            let prefix = format!("{name}:");
            self.headers.iter().find_map(|h| h.strip_prefix(&prefix).map(|v| v.trim().to_string()))
        }
    }

    pub struct Server {
        pub url: String,
        pub requests: Arc<Mutex<Vec<Request>>>,
    }

    /// `respond` gets each request and how many came before it, and returns
    /// (status, content type, body).
    pub fn serve(respond: impl Fn(&Request, usize) -> (u16, &'static str, String) + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                let (head_end, length) = loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break (raw.len(), 0);
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&raw[..pos]).to_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                            .unwrap_or(0);
                        break (pos + 4, length);
                    }
                };
                while raw.len() < head_end + length {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                }
                let head = String::from_utf8_lossy(&raw[..head_end.min(raw.len())]).to_string();
                let mut lines = head.lines();
                let mut first = lines.next().unwrap_or("").split(' ');
                let request = Request {
                    method: first.next().unwrap_or("").to_string(),
                    path: first.next().unwrap_or("").to_string(),
                    headers: lines
                        .filter_map(|l| l.split_once(':').map(|(name, value)| format!("{}:{value}", name.to_lowercase())))
                        .collect(),
                    body: String::from_utf8_lossy(&raw[head_end.min(raw.len())..]).to_string(),
                };
                let index = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(request.clone());
                    seen.len() - 1
                };
                let (status, content_type, body) = respond(&request, index);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Server { url, requests }
    }

    /// Put a session for `gateway` in the store.
    pub fn sign_in(gateway: &str, access: &str, refresh: &str, expires_in: i64) {
        let expires_at = (super::now() as i64 + expires_in).max(0) as u64;
        super::save_session(&super::Session {
            gateway: gateway.to_string(),
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            expires_at,
            email: "taro@example.com".to_string(),
        })
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::testserver::{serve, session_lock, sign_in};
    use super::*;
    use tauri::async_runtime::block_on;

    const JSON: &str = "application/json";

    #[test]
    fn device_code_sign_in() {
        let _lock = session_lock();
        sign_out().unwrap();
        let server = serve(|req, _| match req.path.as_str() {
            "/oauth/device_authorization" => (
                200,
                JSON,
                r#"{"device_code":"dev-1","user_code":"WDJB-MJHT","verification_uri":"http://127.0.0.1:1/device",
                    "verification_uri_complete":"http://127.0.0.1:1/device?user_code=WDJB-MJHT","expires_in":600,"interval":1}"#
                    .to_string(),
            ),
            // Only the device code we were given is good.
            "/oauth/token" if !req.body.contains("device_code=dev-1") => (400, JSON, r#"{"error":"invalid_grant"}"#.into()),
            "/oauth/token" => (
                200,
                JSON,
                r#"{"access_token":"at-1","refresh_token":"rt-1","token_type":"Bearer","expires_in":3600,"email":"taro@example.com"}"#.into(),
            ),
            _ => (404, JSON, "{}".into()),
        });
        let gateway = server.url.clone();
        let login = block_on(start_login(&gateway)).unwrap();
        assert_eq!(login.user_code, "WDJB-MJHT");
        assert_eq!(login.verification_uri, "http://127.0.0.1:1/device?user_code=WDJB-MJHT");
        assert_eq!(status(&gateway).0, "none");

        let email = block_on(finish_login(&gateway, &login, &|| false)).unwrap();
        assert_eq!(email, "taro@example.com");
        assert_eq!(status(&gateway), ("signed-in", "taro@example.com".to_string()));
        assert_eq!(block_on(bearer(&gateway)).unwrap(), "at-1");

        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].body, "", "the device authorization request carries no client id");
        assert_eq!(
            requests[1].body,
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=dev-1"
        );
        assert_eq!(requests[1].header("content-type").as_deref(), Some("application/x-www-form-urlencoded"));
    }

    #[test]
    fn sign_in_waits_then_reports_refusal_and_cancel() {
        let _lock = session_lock();
        sign_out().unwrap();
        let server = serve(|_, index| match index {
            0 | 1 => (400, JSON, r#"{"error":"authorization_pending"}"#.into()),
            _ => (400, JSON, r#"{"error":"access_denied"}"#.into()),
        });
        let login = Login {
            user_code: "A".into(),
            verification_uri: "http://127.0.0.1:1/device".into(),
            device_code: "d".into(),
            interval: 1,
            expires_in: 600,
        };
        let err = block_on(finish_login(&server.url, &login, &|| false)).unwrap_err();
        assert!(err.contains("拒否"), "{err}");
        assert_eq!(server.requests.lock().unwrap().len(), 3, "kept polling while pending");
        assert_eq!(status(&server.url).0, "none");

        let err = block_on(finish_login(&server.url, &login, &|| true)).unwrap_err();
        assert_eq!(err, "サインインをやめました");
        assert_eq!(server.requests.lock().unwrap().len(), 3, "a cancelled sign-in stops polling");
    }

    #[test]
    fn expired_bearer_is_refreshed() {
        let _lock = session_lock();
        // The gateway keeps the refresh token and does not repeat the e-mail.
        let server = serve(|_, _| (200, JSON, r#"{"access_token":"at-2","expires_in":3600}"#.into()));
        sign_in(&server.url, "at-1", "rt-1", -10);
        assert_eq!(block_on(bearer(&server.url)).unwrap(), "at-2");
        assert_eq!(server.requests.lock().unwrap()[0].body, "grant_type=refresh_token&refresh_token=rt-1");
        // ... and both survive for the next refresh.
        let session = load_session(&server.url).unwrap();
        assert_eq!((session.refresh_token.as_str(), session.email.as_str()), ("rt-1", "taro@example.com"));
        // A fresh bearer is used as it is.
        assert_eq!(block_on(bearer(&server.url)).unwrap(), "at-2");
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn refused_refresh_asks_for_a_new_sign_in() {
        let _lock = session_lock();
        let server = serve(|_, _| (400, JSON, r#"{"error":"invalid_grant"}"#.into()));
        sign_in(&server.url, "at-1", "rt-1", -10);
        assert_eq!(block_on(bearer(&server.url)).unwrap_err(), SIGN_IN_AGAIN);
        assert_eq!(status(&server.url).0, "expired");
        // The dead refresh token is not offered again.
        assert_eq!(block_on(bearer(&server.url)).unwrap_err(), SIGN_IN_AGAIN);
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn unreachable_gateway_keeps_a_still_valid_bearer() {
        let _lock = session_lock();
        // Nothing listens here; the bearer has a minute left (inside the refresh margin).
        let gateway = "http://127.0.0.1:9";
        sign_in(gateway, "at-1", "rt-1", 60);
        assert_eq!(block_on(bearer(gateway)).unwrap(), "at-1");
        sign_in(gateway, "at-1", "rt-1", -10);
        assert!(block_on(bearer(gateway)).unwrap_err().contains("接続できません"));
    }

    #[test]
    fn session_is_only_used_for_its_own_gateway() {
        let _lock = session_lock();
        let other = serve(|_, _| (200, JSON, r#"{"access_token":"stolen"}"#.into()));
        sign_in("https://gw.example.com", "at-1", "rt-1", 3600);
        assert_eq!(block_on(bearer(&other.url)).unwrap_err(), SIGN_IN_AGAIN);
        assert_eq!(status(&other.url).0, "none");
        assert!(other.requests.lock().unwrap().is_empty(), "nothing may be sent to another gateway");
    }

    #[test]
    fn concurrent_refreshes_use_the_refresh_token_once() {
        let _lock = session_lock();
        let server = serve(|req, _| {
            std::thread::sleep(Duration::from_millis(150));
            if req.body.ends_with("refresh_token=rt-1") {
                (200, JSON, r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":3600}"#.into())
            } else {
                (400, JSON, r#"{"error":"invalid_grant"}"#.into())
            }
        });
        sign_in(&server.url, "at-1", "rt-1", -10);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let url = server.url.clone();
                std::thread::spawn(move || block_on(bearer(&url)))
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap().unwrap(), "at-2");
        }
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn sign_in_start_failures() {
        let html = serve(|_, _| (200, "text/html", "<html>not a gateway</html>".into()));
        assert_eq!(block_on(start_login(&html.url)).err().unwrap(), "ゲートウェイの応答を読み取れません");
        let missing = serve(|_, _| (404, "text/html", "nope".into()));
        assert!(block_on(start_login(&missing.url)).err().unwrap().contains("ではないようです"));
        // A gateway must not be able to make us open something that is not a web page.
        let evil = serve(|_, _| (200, JSON, r#"{"device_code":"d","user_code":"u","verification_uri":"file:///Applications/Calculator.app"}"#.into()));
        assert!(block_on(start_login(&evil.url)).err().unwrap().contains("サインイン先"));
        assert!(block_on(start_login("http://example.com")).is_err());
    }


    #[test]
    fn urls_are_normalised_and_checked() {
        assert!(check_url(URL).is_ok());
        assert!(check_url("https://gw.example.com").is_ok());
        assert!(check_url("http://localhost:8088").is_ok());
        assert!(check_url("http://127.0.0.1:8088").is_ok());
        assert!(check_url("http://[::1]:8088").is_ok());
        assert!(check_url("").is_err());
        assert!(check_url("gw.example.com").is_err());
        assert!(check_url("http://gw.example.com").is_err());
        // a look-alike of localhost must not get the plain-http exemption
        assert!(check_url("http://localhost.example.com").is_err());
        assert!(check_url("http://localhost@evil.example.com").is_err());
        assert!(check_url("https://").is_err());
        assert!(check_url("file:///etc/passwd").is_err());
    }
}
