//! Native SoundCloud requests, with independent OAuth and protected grants.
//!
//! Only the public API is used. Callers keep Spotify and SoundCloud identities
//! distinct and must obtain the permissions required for mixed playback before
//! distributing that experience.

mod auth;
mod models;

pub use auth::{AppCredentials, REDIRECT_URI, StoredGrant};
pub use models::{Access, Library, Page, Playlist, Resolved, StreamUrls, Track, User};

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::{Mutex, Semaphore, watch};

use crate::{
    credentials::{self, Lease, Slot},
    http::Http,
};

pub type Result<T> = std::result::Result<T, Error>;

/// Errors contain classifications only, never provider response text, tokens,
/// client secrets or signed streaming URLs.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("Connect your SoundCloud account first.")]
    NotSignedIn,
    #[error("Enter your SoundCloud application's Client ID and Client Secret.")]
    InvalidCredentials,
    #[error("SoundCloud sign-in was declined.")]
    AccessDenied,
    #[error("SoundCloud sign-in expired or was revoked. Connect again.")]
    SignInExpired,
    #[error("The SoundCloud operation was cancelled.")]
    Cancelled,
    #[error(
        "SoundCloud is disconnected locally, but remote authorization revocation could not be confirmed."
    )]
    RemoteRevocationUnconfirmed,
    #[error("SoundCloud sign-in timed out. Try again.")]
    LoginTimeout,
    #[error(
        "Cannot start SoundCloud sign-in. Port 8899 is in use or unavailable. Close the app using that port and try again."
    )]
    CallbackPort,
    #[error("Could not open the default browser for SoundCloud sign-in.")]
    Browser,
    #[error("Cannot reach SoundCloud. Check your network and proxy settings.")]
    Network,
    #[error("The configured HTTP client is unavailable. Correct the proxy settings and try again.")]
    ProxyConfiguration,
    #[error("SoundCloud returned an unreadable response.")]
    InvalidResponse,
    #[error("SoundCloud refused this request (HTTP {status}).")]
    Status { status: u16 },
    #[error("SoundCloud is limiting requests. Try again after its rate limit resets.")]
    RateLimited { retry_after: u64 },
    #[error("This song is not available for complete playback through SoundCloud's API.")]
    Unavailable,
    #[error("You can edit only your own SoundCloud playlists.")]
    NotOwned,
    #[error("This playlist changed elsewhere. Reload it before applying your pending edit.")]
    Conflict,
    #[error(
        "SoundCloud may have received the change, but its result could not be confirmed. Refresh before trying again."
    )]
    UncertainMutation,
    #[error("The resource identifier or URL is not a supported SoundCloud resource.")]
    InvalidResource,
    #[error("{0}")]
    Storage(#[from] credentials::Error),
}

struct Session {
    grant: StoredGrant,
    lease: Lease,
    refresh_blocked: bool,
}

struct Inner {
    http: Http,
    mutations: Http,
    store: credentials::Store,
    session: Mutex<Option<Session>>,
    generation: AtomicU64,
    sign_in_cancel: StdMutex<Option<watch::Sender<bool>>>,
    storage_warning: StdMutex<Option<credentials::Error>>,
    playlist_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    requests: Semaphore,
    limited_until: StdMutex<Option<Instant>>,
    base: Url,
    token_url: Url,
}

/// Clones share a token mutex, so a single-use refresh token is consumed once.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    /// `mutations` must use `http::build_collection_client`: retries and
    /// redirects are disabled for writes and single-use token exchanges.
    pub fn new(http: Http, mutations: Http, store: credentials::Store) -> Self {
        Self::with_endpoints(
            http,
            mutations,
            store,
            Url::parse("https://api.soundcloud.com/").expect("constant API URL"),
            Url::parse(auth::TOKEN_URL).expect("constant token URL"),
        )
    }

    fn with_endpoints(
        http: Http,
        mutations: Http,
        store: credentials::Store,
        base: Url,
        token_url: Url,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                http,
                mutations,
                store,
                session: Mutex::new(None),
                generation: AtomicU64::new(0),
                sign_in_cancel: StdMutex::new(None),
                storage_warning: StdMutex::new(None),
                playlist_locks: Mutex::new(HashMap::new()),
                requests: Semaphore::new(4),
                limited_until: StdMutex::new(None),
                base,
                token_url,
            }),
        }
    }

    /// A failed keyring write does not undo an otherwise successful sign-in.
    /// Show this warning to explain that reconnecting may be needed on restart.
    pub fn take_storage_warning(&self) -> Option<credentials::Error> {
        self.inner
            .storage_warning
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    pub async fn is_signed_in(&self) -> bool {
        self.inner
            .session
            .lock()
            .await
            .as_ref()
            .is_some_and(|session| session.lease.current())
    }

    pub async fn configured_client_id(&self) -> Option<String> {
        self.inner
            .session
            .lock()
            .await
            .as_ref()
            .map(|session| session.grant.app.client_id.clone())
    }

    pub fn cancel_sign_in(&self) {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(cancel) = self
            .inner
            .sign_in_cancel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            self.inner.store.invalidate(Slot::SoundCloud);
            let _ = cancel.send(true);
        }
    }

    /// Cancelling the account dialog also revokes a just-completed sign-in.
    /// A commit can beat its UI notification, so stopping only the listener
    /// would leave an apparently cancelled account restorable on next launch.
    pub fn cancel_authorization(&self) -> Result<()> {
        self.cancel_sign_in();
        let revoked = self.inner.store.revoke(Slot::SoundCloud);
        // Enqueue deletion now, before another sign-in can enqueue its save.
        let deleting = self.inner.store.lease(Slot::SoundCloud).delete();
        let generation = self.generation();
        let client = self.clone();
        tokio::spawn(async move {
            if let Err(error) = deleting.await
                && client.generation() == generation
            {
                client.warn_storage(error);
            }
            let mut session = client.inner.session.lock().await;
            if client.generation() == generation {
                *session = None;
            }
        });
        revoked.map_err(Error::Storage)
    }

    fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::SeqCst)
    }

    fn check_generation(&self, generation: u64) -> Result<()> {
        if self.generation() == generation {
            Ok(())
        } else {
            Err(Error::Cancelled)
        }
    }

    pub async fn restore(&self) -> Result<Option<User>> {
        let generation = self.generation();
        let lease = self.inner.store.lease(Slot::SoundCloud);
        let loaded = lease.load().await?;
        self.check_generation(generation)?;
        if let Some(warning) = loaded.warning {
            self.warn_storage(warning);
        }
        let Some(credentials::Grant::SoundCloud(grant)) = loaded.grant else {
            return Ok(None);
        };
        if !grant.valid() {
            return Err(Error::InvalidCredentials);
        }
        {
            let mut session = self.inner.session.lock().await;
            self.check_generation(generation)?;
            *session = Some(Session {
                grant,
                lease,
                refresh_blocked: false,
            });
        }
        match self.me().await {
            Ok(user) => Ok(Some(user)),
            Err(error) => {
                if matches!(error, Error::SignInExpired) {
                    let mut session = self.inner.session.lock().await;
                    self.check_generation(generation)?;
                    *session = None;
                }
                Err(error)
            }
        }
    }

    /// Reserve the loopback port before opening the system browser. Passwords
    /// are entered only at SoundCloud; this process receives an OAuth code.
    pub async fn sign_in(
        &self,
        app: AppCredentials,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<User> {
        self.cancel_sign_in();
        let generation = self.generation();
        self.inner.store.invalidate(Slot::SoundCloud);
        let lease = self.inner.store.lease(Slot::SoundCloud);
        let flow = auth::begin(&app)?;
        let listener = auth::listen()?;
        let (own_cancel, mut own_receiver) = watch::channel(false);
        *self
            .inner
            .sign_in_cancel
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(own_cancel);
        let listener_cancel = own_receiver.clone();
        let authorize = async {
            let browser_url = flow.url;
            tokio::task::spawn_blocking(move || crate::opener::open(browser_url))
                .await
                .map_err(|_| Error::Browser)?
                .map_err(|_| Error::Browser)?;
            let code = auth::wait_for_code(listener, &flow.state, listener_cancel).await?;
            self.check_generation(generation)?;
            let response = self
                .token_request(&[
                    ("grant_type", "authorization_code"),
                    ("client_id", &app.client_id),
                    ("client_secret", &app.client_secret),
                    ("redirect_uri", REDIRECT_URI),
                    ("code", &code),
                    ("code_verifier", &flow.verifier),
                ])
                .await?;
            self.check_generation(generation)?;
            let grant = response.into_grant(app)?;
            let user: User = self.read_with_token("me", &grant.access_token).await?;
            if user.urn.is_empty() {
                return Err(Error::InvalidResponse);
            }
            self.check_generation(generation)?;
            let mut session = self.inner.session.lock().await;
            self.check_generation(generation)?;
            if let Err(error) = lease
                .save(credentials::Grant::SoundCloud(grant.clone()))
                .await
            {
                if error == credentials::Error::Stale {
                    return Err(Error::Cancelled);
                }
                self.warn_storage(error);
            }
            self.check_generation(generation)?;
            *session = Some(Session {
                grant,
                lease,
                refresh_blocked: false,
            });
            Ok(user)
        };
        let result = tokio::select! {
            biased;
            _ = auth::cancelled(&mut cancel) => {
                if self.generation() == generation {
                    self.cancel_sign_in();
                    self.inner.store.invalidate(Slot::SoundCloud);
                }
                Err(Error::Cancelled)
            }
            _ = auth::cancelled(&mut own_receiver) => Err(Error::Cancelled),
            result = authorize => result,
        };
        if self.generation() == generation {
            self.inner
                .sign_in_cancel
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take();
        }
        result
    }

    /// Local revocation precedes network I/O. A failed remote request cannot
    /// restore a disconnected account, even when the native keychain is locked.
    pub async fn disconnect(&self) -> Result<()> {
        self.cancel_sign_in();
        let revoked = self.inner.store.revoke(Slot::SoundCloud);
        let token = self
            .inner
            .session
            .lock()
            .await
            .take()
            .map(|session| session.grant.access_token);
        let deletion = self.inner.store.lease(Slot::SoundCloud).delete();
        let remote = async {
            let Some(token) = token else {
                return Ok(());
            };
            let url = self.api_url("disconnect")?;
            let response = self
                .inner
                .mutations
                .client()
                .map_err(|_| Error::ProxyConfiguration)?
                .post(url)
                .header("Authorization", format!("OAuth {token}"))
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => Ok(()),
                // The access token may have expired while its refresh token is
                // still valid. A 401 does not prove remote app revocation.
                Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                    Err(Error::RemoteRevocationUnconfirmed)
                }
                Ok(response) => Err(self.status_error(&response)),
                Err(_) => Err(Error::Network),
            }
        }
        .await;
        let deleted = deletion.await;
        revoked?;
        deleted?;
        remote
    }

    fn warn_storage(&self, error: credentials::Error) {
        *self
            .inner
            .storage_warning
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(error);
    }

    async fn token_request(&self, fields: &[(&str, &str)]) -> Result<auth::TokenResponse> {
        let response = self
            .inner
            .mutations
            .client()
            .map_err(|_| Error::ProxyConfiguration)?
            .post(self.inner.token_url.clone())
            .form(fields)
            .send()
            .await
            .map_err(|_| Error::Network)?;
        if response.status() == StatusCode::BAD_REQUEST
            || response.status() == StatusCode::UNAUTHORIZED
        {
            // OAuth response bodies can echo usable secrets. Never propagate them.
            return Err(Error::SignInExpired);
        }
        if !response.status().is_success() {
            return Err(self.status_error(&response));
        }
        response.json().await.map_err(|_| Error::InvalidResponse)
    }

    async fn access_token(&self, rejected: Option<&str>) -> Result<String> {
        let generation = self.generation();
        let client = self.clone();
        let rejected = rejected.map(str::to_owned);
        // A superseded search may be cancelled while refreshing. The owned
        // worker completes rotation and persistence even if its caller drops
        // this future; otherwise a consumed single-use token could be lost.
        tokio::spawn(async move {
            client
                .refresh_access_token(generation, rejected.as_deref())
                .await
        })
        .await
        .map_err(|_| Error::Cancelled)?
    }

    async fn refresh_access_token(
        &self,
        generation: u64,
        rejected: Option<&str>,
    ) -> Result<String> {
        let mut guard = self.inner.session.lock().await;
        self.check_generation(generation)?;
        let session = guard.as_mut().ok_or(Error::NotSignedIn)?;
        if !session.lease.current() || session.refresh_blocked {
            return Err(Error::SignInExpired);
        }
        if session.grant.expires_soon()
            || rejected.is_some_and(|token| token == session.grant.access_token)
        {
            let app = session.grant.app.clone();
            let response = self
                .token_request(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", &app.client_id),
                    ("client_secret", &app.client_secret),
                    ("refresh_token", &session.grant.refresh_token),
                    ("redirect_uri", REDIRECT_URI),
                ])
                .await;
            self.check_generation(generation)?;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    // A timed-out refresh may already have consumed the token.
                    // Never replay it after an ambiguous response or restart.
                    if !matches!(error, Error::RateLimited { .. } | Error::ProxyConfiguration) {
                        session.refresh_blocked = true;
                        if let Err(failure) = self.inner.store.revoke(Slot::SoundCloud) {
                            self.warn_storage(failure);
                        }
                    }
                    return Err(error);
                }
            };
            self.check_generation(generation)?;
            let grant = match response.into_grant(app) {
                Ok(grant) => grant,
                Err(error) => {
                    session.refresh_blocked = true;
                    if let Err(failure) = self.inner.store.revoke(Slot::SoundCloud) {
                        self.warn_storage(failure);
                    }
                    return Err(error);
                }
            };
            // Update memory before awaiting storage: the single-use old refresh
            // token must never be retried following a keyring failure.
            session.grant = grant;
            if let Err(error) = session
                .lease
                .save(credentials::Grant::SoundCloud(session.grant.clone()))
                .await
            {
                if error == credentials::Error::Stale {
                    return Err(Error::Cancelled);
                }
                self.warn_storage(error);
            }
        }
        self.check_generation(generation)?;
        Ok(session.grant.access_token.clone())
    }

    fn api_url(&self, path: &str) -> Result<Url> {
        let url = self
            .inner
            .base
            .join(path)
            .map_err(|_| Error::InvalidResource)?;
        if url.origin() != self.inner.base.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::InvalidResource);
        }
        Ok(url)
    }

    fn resource_path(kind: &str, urn: &str) -> Result<String> {
        let prefix = format!("soundcloud:{kind}:");
        let id = urn
            .strip_prefix(&prefix)
            .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or(Error::InvalidResource)?;
        Ok(format!("{kind}/{prefix}{id}"))
    }

    fn status_error(&self, response: &reqwest::Response) -> Error {
        match response.status() {
            StatusCode::UNAUTHORIZED => Error::SignInExpired,
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(60)
                    .min(86_400);
                *self
                    .inner
                    .limited_until
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) =
                    Some(Instant::now() + Duration::from_secs(retry_after));
                Error::RateLimited { retry_after }
            }
            status => Error::Status {
                status: status.as_u16(),
            },
        }
    }

    fn check_rate_limit(&self) -> Result<()> {
        if let Some(until) = *self
            .inner
            .limited_until
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            && let Some(remaining) = until.checked_duration_since(Instant::now())
        {
            return Err(Error::RateLimited {
                retry_after: remaining.as_secs().saturating_add(1),
            });
        }
        Ok(())
    }

    async fn read_with_token<T: DeserializeOwned>(&self, path: &str, token: &str) -> Result<T> {
        self.check_rate_limit()?;
        let url = self.api_url(path)?;
        let response = self
            .inner
            .http
            .client()
            .map_err(|_| Error::ProxyConfiguration)?
            .get(url)
            .header("Authorization", format!("OAuth {token}"))
            .send()
            .await
            .map_err(|_| Error::Network)?;
        if !response.status().is_success() {
            return Err(self.status_error(&response));
        }
        response.json().await.map_err(|_| Error::InvalidResponse)
    }

    async fn read<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let generation = self.generation();
        let _permit = self
            .inner
            .requests
            .acquire()
            .await
            .map_err(|_| Error::Cancelled)?;
        let token = self.access_token(None).await?;
        let result = self.read_with_token(path, &token).await;
        self.check_generation(generation)?;
        if matches!(result, Err(Error::SignInExpired)) {
            let token = self.access_token(Some(&token)).await?;
            let result = self.read_with_token(path, &token).await;
            self.check_generation(generation)?;
            result
        } else {
            result
        }
    }

    async fn write(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Option<Value>> {
        let generation = self.generation();
        self.check_rate_limit()?;
        let _permit = self
            .inner
            .requests
            .acquire()
            .await
            .map_err(|_| Error::Cancelled)?;
        let token = self.access_token(None).await?;
        self.check_generation(generation)?;
        let url = self.api_url(path)?;
        let mut request = self
            .inner
            .mutations
            .client()
            .map_err(|_| Error::ProxyConfiguration)?
            .request(method, url)
            .header("Authorization", format!("OAuth {token}"));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|_| Error::UncertainMutation)?;
        self.check_generation(generation)?;
        if response.status().is_server_error() {
            return Err(Error::UncertainMutation);
        }
        if !response.status().is_success() {
            return Err(self.status_error(&response));
        }
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| Error::UncertainMutation)?;
        if bytes.is_empty() {
            return Ok(None);
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| Error::UncertainMutation)
    }

    async fn page<T: DeserializeOwned>(&self, path: &str) -> Result<Page<T>> {
        let value: Value = self.read(path).await?;
        if value.is_array() {
            Ok(Page {
                collection: serde_json::from_value(value).map_err(|_| Error::InvalidResponse)?,
                next_href: None,
            })
        } else {
            serde_json::from_value(value).map_err(|_| Error::InvalidResponse)
        }
    }

    async fn all<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        let generation = self.generation();
        let mut next = Some(path.to_owned());
        let mut visited = HashSet::new();
        let mut collection = Vec::new();
        while let Some(path) = next {
            // A looping provider cursor must never issue unbounded requests.
            let canonical = self.api_url(&path)?.to_string();
            if !visited.insert(canonical) {
                return Err(Error::InvalidResponse);
            }
            let page = self.page(&path).await?;
            self.check_generation(generation)?;
            collection.extend(page.collection);
            next = page.next_href;
        }
        Ok(collection)
    }

    pub async fn me(&self) -> Result<User> {
        self.read("me").await
    }

    pub async fn track(&self, urn: &str) -> Result<Track> {
        self.read(&Self::resource_path("tracks", urn)?).await
    }

    fn search_path(
        &self,
        kind: &str,
        query: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<String> {
        if let Some(cursor) = cursor {
            let url = self.api_url(cursor)?;
            if url.path() != format!("/{kind}") {
                return Err(Error::InvalidResource);
            }
            return Ok(url.to_string());
        }
        let mut url = self.api_url(kind)?;
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("limit", &limit.clamp(1, 200).to_string())
            .append_pair("linked_partitioning", "true");
        Ok(url.to_string())
    }

    pub async fn search_tracks(
        &self,
        query: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Page<Track>> {
        self.page(&self.search_path("tracks", query, limit, cursor)?)
            .await
    }

    pub async fn search_playlists(
        &self,
        query: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<Page<Playlist>> {
        self.page(&self.search_path("playlists", query, limit, cursor)?)
            .await
    }

    pub async fn library(&self) -> Result<Library> {
        let generation = self.generation();
        let (user, liked_tracks, playlists, liked_playlists) = tokio::try_join!(
            self.me(),
            self.all("me/likes/tracks?linked_partitioning=true&limit=200"),
            self.all("me/playlists?linked_partitioning=true&limit=200&show_tracks=false"),
            self.all("me/likes/playlists?linked_partitioning=true&limit=200")
        )?;
        self.check_generation(generation)?;
        Ok(Library {
            user,
            liked_tracks,
            playlists,
            liked_playlists,
        })
    }

    pub async fn playlist(&self, urn: &str) -> Result<Playlist> {
        self.read(&format!(
            "{}?show_tracks=false",
            Self::resource_path("playlists", urn)?
        ))
        .await
    }

    pub async fn playlist_tracks(&self, urn: &str) -> Result<Vec<Track>> {
        self.all(&format!(
            "{}/tracks?linked_partitioning=true&limit=200",
            Self::resource_path("playlists", urn)?
        ))
        .await
    }

    pub async fn set_track_liked(&self, urn: &str, liked: bool) -> Result<()> {
        let path = format!("likes/{}", Self::resource_path("tracks", urn)?);
        self.write(
            if liked { Method::POST } else { Method::DELETE },
            &path,
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn set_playlist_liked(&self, urn: &str, liked: bool) -> Result<()> {
        let path = format!("likes/{}", Self::resource_path("playlists", urn)?);
        self.write(
            if liked { Method::POST } else { Method::DELETE },
            &path,
            None,
        )
        .await?;
        Ok(())
    }

    async fn playlist_lock(&self, urn: &str) -> Arc<Mutex<()>> {
        self.inner
            .playlist_locks
            .lock()
            .await
            .entry(urn.into())
            .or_default()
            .clone()
    }

    async fn require_owned(&self, urn: &str) -> Result<Playlist> {
        let (user, playlist) = tokio::try_join!(self.me(), self.playlist(urn))?;
        if user.urn.is_empty() || playlist.user.urn != user.urn {
            return Err(Error::NotOwned);
        }
        Ok(playlist)
    }

    pub async fn create_playlist(
        &self,
        title: &str,
        private: bool,
        tracks: &[String],
    ) -> Result<Playlist> {
        let generation = self.generation();
        validate_title(title)?;
        validate_tracks(tracks)?;
        let body = json!({"playlist": {"title": title.trim(), "sharing": if private { "private" } else { "public" }, "tracks": track_values(tracks)}});
        let value = self
            .write(Method::POST, "playlists", Some(&body))
            .await?
            .ok_or(Error::UncertainMutation)?;
        let playlist: Playlist =
            serde_json::from_value(value).map_err(|_| Error::UncertainMutation)?;
        if playlist.urn.is_empty() {
            return Err(Error::UncertainMutation);
        }
        self.check_generation(generation)?;
        self.verify_playlist(&playlist.urn, Some(title.trim()), Some(tracks))
            .await
    }

    pub async fn rename_playlist(&self, urn: &str, title: &str) -> Result<Playlist> {
        let generation = self.generation();
        validate_title(title)?;
        let lock = self.playlist_lock(urn).await;
        let _guard = lock.lock().await;
        self.check_generation(generation)?;
        self.require_owned(urn).await?;
        self.check_generation(generation)?;
        self.write(
            Method::PUT,
            &Self::resource_path("playlists", urn)?,
            Some(&json!({"playlist":{"title":title.trim()}})),
        )
        .await?;
        self.verify_playlist(urn, Some(title.trim()), None).await
    }

    /// Structural writes replace the complete ordered list. Reject an edit
    /// based on a stale list, preserve duplicate URNs, then verify readback.
    /// SoundCloud supplies no conditional revision token, so another client's
    /// write between this read and PUT cannot be prevented atomically.
    pub async fn replace_playlist_tracks(
        &self,
        urn: &str,
        expected: &[String],
        desired: &[String],
    ) -> Result<Playlist> {
        let generation = self.generation();
        validate_tracks(desired)?;
        let lock = self.playlist_lock(urn).await;
        let _guard = lock.lock().await;
        self.check_generation(generation)?;
        self.require_owned(urn).await?;
        self.check_generation(generation)?;
        let current = self.playlist_tracks(urn).await?;
        self.check_generation(generation)?;
        if !same_order(&current, expected) {
            return Err(Error::Conflict);
        }
        self.write(
            Method::PUT,
            &Self::resource_path("playlists", urn)?,
            Some(&json!({"playlist":{"tracks":track_values(desired)}})),
        )
        .await?;
        self.verify_playlist(urn, None, Some(desired)).await
    }

    pub async fn delete_playlist(&self, urn: &str) -> Result<()> {
        let generation = self.generation();
        let lock = self.playlist_lock(urn).await;
        let _guard = lock.lock().await;
        self.check_generation(generation)?;
        self.require_owned(urn).await?;
        self.check_generation(generation)?;
        self.write(
            Method::DELETE,
            &Self::resource_path("playlists", urn)?,
            None,
        )
        .await?;
        match self.playlist(urn).await {
            Err(Error::Status { status: 404 }) => Ok(()),
            _ => Err(Error::UncertainMutation),
        }
    }

    /// Fetch and append under the same per-playlist lock as replacements.
    /// Concurrent appends in this process therefore cannot overwrite each other.
    pub async fn append_playlist_tracks(&self, urn: &str, items: &[String]) -> Result<Playlist> {
        let generation = self.generation();
        validate_tracks(items)?;
        let lock = self.playlist_lock(urn).await;
        let _guard = lock.lock().await;
        self.check_generation(generation)?;
        self.require_owned(urn).await?;
        let current = self.playlist_tracks(urn).await?;
        self.check_generation(generation)?;
        let mut desired: Vec<_> = current.into_iter().map(|track| track.urn).collect();
        desired.extend_from_slice(items);
        validate_tracks(&desired)?;
        self.write(
            Method::PUT,
            &Self::resource_path("playlists", urn)?,
            Some(&json!({"playlist":{"tracks":track_values(&desired)}})),
        )
        .await?;
        self.verify_playlist(urn, None, Some(&desired)).await
    }

    async fn verify_playlist(
        &self,
        urn: &str,
        title: Option<&str>,
        tracks: Option<&[String]>,
    ) -> Result<Playlist> {
        let mut playlist = self
            .playlist(urn)
            .await
            .map_err(|_| Error::UncertainMutation)?;
        if title.is_some_and(|title| playlist.title != title) {
            return Err(Error::UncertainMutation);
        }
        if let Some(expected) = tracks {
            let actual = self
                .playlist_tracks(urn)
                .await
                .map_err(|_| Error::UncertainMutation)?;
            if !same_order(&actual, expected) {
                return Err(Error::UncertainMutation);
            }
            playlist.tracks = actual;
            playlist.track_count = playlist.tracks.len();
        }
        Ok(playlist)
    }

    pub async fn streams(&self, urn: &str) -> Result<StreamUrls> {
        let generation = self.generation();
        let track = self.track(urn).await?;
        self.check_generation(generation)?;
        if !track.is_playable() {
            return Err(Error::Unavailable);
        }
        let streams: StreamUrls = self
            .read(&format!("{}/streams", Self::resource_path("tracks", urn)?))
            .await?;
        let Some(url) = streams.preferred() else {
            return Err(Error::Unavailable);
        };
        let url = Url::parse(url).map_err(|_| Error::InvalidResponse)?;
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return Err(Error::InvalidResponse);
        }
        Ok(streams)
    }

    /// Resolve only a public SoundCloud permalink, never an arbitrary URL.
    pub async fn resolve(&self, permalink: &str) -> Result<Resolved> {
        let url = Url::parse(permalink).map_err(|_| Error::InvalidResource)?;
        if url.scheme() != "https"
            || !matches!(
                url.host_str(),
                Some("soundcloud.com" | "www.soundcloud.com" | "on.soundcloud.com")
            )
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::InvalidResource);
        }
        let mut endpoint = self.api_url("resolve")?;
        endpoint.query_pairs_mut().append_pair("url", permalink);
        self.read(endpoint.as_str()).await
    }
}

fn validate_title(title: &str) -> Result<()> {
    if title.trim().is_empty() {
        Err(Error::InvalidResource)
    } else {
        Ok(())
    }
}

fn validate_tracks(tracks: &[String]) -> Result<()> {
    for urn in tracks {
        Client::resource_path("tracks", urn)?;
    }
    Ok(())
}

fn track_values(tracks: &[String]) -> Vec<Value> {
    tracks.iter().map(|urn| json!({"urn":urn})).collect()
}

fn same_order(actual: &[Track], expected: &[String]) -> bool {
    actual.len() == expected.len() && actual.iter().map(|track| &track.urn).eq(expected.iter())
}

#[cfg(test)]
mod tests;
