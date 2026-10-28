//! SoundCloud OAuth uses the system browser and a loopback callback only.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::watch,
};

use super::{Error, Result};

pub const REDIRECT_URI: &str = "http://127.0.0.1:8899/soundcloud/callback";
const CALLBACK_PATH: &str = "/soundcloud/callback";
pub(super) const TOKEN_URL: &str = "https://secure.soundcloud.com/oauth/token";

/// Deliberately has no Debug implementation because it contains a secret.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppCredentials {
    pub client_id: String,
    pub client_secret: String,
}

impl AppCredentials {
    pub fn new(client_id: String, client_secret: String) -> Result<Self> {
        let app = Self {
            client_id: client_id.trim().into(),
            client_secret: client_secret.trim().into(),
        };
        if app.valid() {
            Ok(app)
        } else {
            Err(Error::InvalidCredentials)
        }
    }

    pub fn valid(&self) -> bool {
        !self.client_id.trim().is_empty()
            && !self.client_secret.trim().is_empty()
            && !self.client_id.chars().any(char::is_control)
            && !self.client_secret.chars().any(char::is_control)
    }
}

/// One atomic protected record prevents a new refresh token being paired
/// with a previous application's secret. Never serialize this into state JSON.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGrant {
    pub app: AppCredentials,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

impl StoredGrant {
    pub fn valid(&self) -> bool {
        self.app.valid() && !self.access_token.is_empty() && !self.refresh_token.is_empty()
    }

    pub(super) fn expires_soon(&self) -> bool {
        self.expires_at <= now().saturating_add(90)
    }
}

#[derive(Deserialize)]
pub(super) struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

impl TokenResponse {
    pub(super) fn into_grant(self, app: AppCredentials) -> Result<StoredGrant> {
        let grant = StoredGrant {
            app,
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: now().saturating_add(self.expires_in),
        };
        if grant.valid() && self.expires_in > 0 {
            Ok(grant)
        } else {
            Err(Error::InvalidResponse)
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The verifier and state are one-use authorization secrets.
pub(super) struct Flow {
    pub url: String,
    pub verifier: String,
    pub state: String,
}

pub(super) fn begin(app: &AppCredentials) -> Result<Flow> {
    if !app.valid() {
        return Err(Error::InvalidCredentials);
    }
    fn random(bytes: usize) -> String {
        let mut buffer = vec![0; bytes];
        rand::rng().fill_bytes(&mut buffer);
        URL_SAFE_NO_PAD.encode(buffer)
    }
    let verifier = random(48);
    let state = random(24);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut url = reqwest::Url::parse("https://secure.soundcloud.com/authorize")
        .map_err(|_| Error::InvalidResponse)?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", app.client_id.as_str()),
        ("redirect_uri", REDIRECT_URI),
        ("response_type", "code"),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
    ]);
    Ok(Flow {
        url: url.into(),
        verifier,
        state,
    })
}

pub(super) fn listen() -> Result<TcpListener> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 8899))
        .map_err(|_| Error::CallbackPort)?;
    listener
        .set_nonblocking(true)
        .map_err(|_| Error::CallbackPort)?;
    TcpListener::from_std(listener).map_err(|_| Error::CallbackPort)
}

pub(super) async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

pub(super) async fn wait_for_code(
    listener: TcpListener,
    state: &str,
    mut cancel: watch::Receiver<bool>,
) -> Result<String> {
    let deadline = tokio::time::sleep(Duration::from_secs(600));
    tokio::pin!(deadline);
    loop {
        let (mut stream, peer) = tokio::select! {
            _ = cancelled(&mut cancel) => return Err(Error::Cancelled),
            _ = &mut deadline => return Err(Error::LoginTimeout),
            accepted = listener.accept() => accepted.map_err(|_| Error::CallbackPort)?,
        };
        if !peer.ip().is_loopback() {
            continue;
        }
        let mut line = String::new();
        let read = async {
            let mut reader = BufReader::new(&mut stream).take(8192);
            reader.read_line(&mut line).await
        };
        let read = tokio::select! {
            _ = cancelled(&mut cancel) => return Err(Error::Cancelled),
            read = tokio::time::timeout(Duration::from_secs(3), read) => read,
        };
        if !matches!(read, Ok(Ok(_))) || !line.ends_with('\n') {
            continue;
        }
        let outcome = parse_callback(&line, state);
        let (status, message) = match &outcome {
            Ok(Some(_)) => (
                "200 OK",
                "SoundCloud authorization received. Return to Spotifast Mix.",
            ),
            Err(Error::AccessDenied) => (
                "200 OK",
                "SoundCloud sign-in was declined. You can close this tab.",
            ),
            _ => (
                "400 Bad Request",
                "This is not the current SoundCloud sign-in request.",
            ),
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}",
            message.len()
        );
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            stream.write_all(response.as_bytes()),
        )
        .await;
        match outcome {
            Ok(Some(code)) => return Ok(code),
            Err(Error::AccessDenied) => return Err(Error::AccessDenied),
            _ => {}
        }
    }
}

fn parse_callback(line: &str, expected_state: &str) -> Result<Option<String>> {
    let mut fields = line.split_whitespace();
    if fields.next() != Some("GET") {
        return Ok(None);
    }
    let Some(target) = fields.next() else {
        return Ok(None);
    };
    if !target.starts_with('/') || target.starts_with("//") {
        return Ok(None);
    }
    let url = reqwest::Url::parse(&format!("http://127.0.0.1:8899{target}"))
        .map_err(|_| Error::InvalidResponse)?;
    if url.path() != CALLBACK_PATH {
        return Ok(None);
    }
    let mut code = None;
    let mut state = None;
    let mut denied = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" if state.is_none() => state = Some(value.into_owned()),
            "code" if code.is_none() => code = Some(value.into_owned()),
            "error" => denied = true,
            "state" | "code" => return Ok(None),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Ok(None);
    }
    if denied {
        return Err(Error::AccessDenied);
    }
    Ok(code.filter(|code| !code.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_checks_state_before_refusal_and_rejects_duplicate_parameters() {
        assert!(matches!(
            parse_callback(
                "GET /soundcloud/callback?error=denied&state=wrong HTTP/1.1\r\n",
                "expected"
            ),
            Ok(None)
        ));
        assert!(matches!(
            parse_callback(
                "GET /soundcloud/callback?error=denied&state=expected HTTP/1.1\r\n",
                "expected"
            ),
            Err(Error::AccessDenied)
        ));
        assert!(matches!(
            parse_callback(
                "GET /soundcloud/callback?state=expected&state=expected&code=a HTTP/1.1\r\n",
                "expected"
            ),
            Ok(None)
        ));
        assert_eq!(
            parse_callback(
                "GET /soundcloud/callback?state=expected&code=a%2Bb HTTP/1.1\r\n",
                "expected"
            )
            .unwrap(),
            Some("a+b".into())
        );
    }

    #[test]
    fn oauth_uses_pkce_and_never_puts_client_secret_in_browser_url() {
        let app = AppCredentials::new("id".into(), "never-in-browser".into()).unwrap();
        let flow = begin(&app).unwrap();
        let url = reqwest::Url::parse(&flow.url).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query.get("redirect_uri").unwrap().as_ref(), REDIRECT_URI);
        assert_eq!(query.get("code_challenge_method").unwrap().as_ref(), "S256");
        assert!(!flow.url.contains("never-in-browser"));
        assert_ne!(begin(&app).unwrap().state, flow.state);
    }
}
