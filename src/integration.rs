//! SoundCloud account work and the independent local mixed-playback session.
//!
//! The UI owns only a projection. Network jobs and both decoders run on the
//! backend runtime, and all account answers carry an authorization generation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::{credentials, http::Http, mix, player::EngineConfig, soundcloud};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    All,
    Spotify,
    SoundCloud,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Library,
    Accounts,
    Playlist(String),
    LocalPlaylist(u64),
    Queue,
}

/// UI intents carry no application secrets. SignIn reads its draft only after drawing.
#[derive(Clone, Debug)]
pub enum Action {
    Open(View),
    Source(Source),
    SignIn,
    CancelSignIn,
    Disconnect,
    Refresh,
    OpenPlaylist(String),
    OpenLink(String),
    Like {
        urn: String,
        liked: bool,
    },
    CreateRemote {
        name: String,
        private: bool,
    },
    RenameRemote {
        urn: String,
        name: String,
    },
    DeleteRemote(String),
    ReplaceRemote {
        urn: String,
        expected: Vec<String>,
        desired: Vec<String>,
    },
    AppendRemote {
        urn: String,
        items: Vec<String>,
    },
    CreateLocal(String),
    RenameLocal {
        id: u64,
        name: String,
    },
    DeleteLocal(u64),
    AddLocal {
        id: u64,
        items: Vec<mix::MediaRef>,
    },
    RemoveLocal {
        id: u64,
        index: usize,
    },
    MoveLocal {
        id: u64,
        from: usize,
        to: usize,
    },
    PlayLocal(u64),
    ResolveItems {
        items: Vec<mix::MediaRef>,
        play: bool,
    },
    Command(mix::Command),
    SelectMix(bool),
}

/// Requests passed from the interface to the existing backend runtime.
pub enum Request {
    Restore,
    LoadLocal,
    SignIn(soundcloud::AppCredentials),
    CancelSignIn,
    Disconnect,
    Search {
        query: String,
        serial: u64,
    },
    Library {
        serial: u64,
    },
    Playlist {
        urn: String,
        serial: u64,
    },
    Like {
        urn: String,
        liked: bool,
        serial: u64,
    },
    Create {
        name: String,
        private: bool,
        serial: u64,
    },
    Rename {
        urn: String,
        name: String,
        serial: u64,
    },
    Delete {
        urn: String,
        serial: u64,
    },
    Replace {
        urn: String,
        expected: Vec<String>,
        desired: Vec<String>,
        serial: u64,
    },
    Append {
        urn: String,
        items: Vec<String>,
        serial: u64,
    },
    Resolve {
        url: String,
        serial: u64,
    },
    ResolveItems {
        items: Vec<mix::MediaRef>,
        play: bool,
        serial: u64,
    },
    Mix {
        serial: u64,
        command: mix::Command,
    },
}

pub enum Event {
    Auth {
        generation: u64,
        result: Result<Option<soundcloud::User>, String>,
    },
    StorageWarning {
        generation: u64,
        message: String,
    },
    SessionPersistence {
        writable: bool,
        message: Option<String>,
    },
    Search {
        generation: u64,
        serial: u64,
        tracks: Result<soundcloud::Page<soundcloud::Track>, String>,
        playlists: Result<soundcloud::Page<soundcloud::Playlist>, String>,
    },
    Library {
        generation: u64,
        serial: u64,
        result: Result<soundcloud::Library, String>,
    },
    Playlist {
        generation: u64,
        serial: u64,
        urn: String,
        result: Result<(soundcloud::Playlist, Vec<soundcloud::Track>), String>,
    },
    Liked {
        generation: u64,
        serial: u64,
        urn: String,
        liked: bool,
        result: Result<(), String>,
    },
    Mutation {
        generation: u64,
        serial: u64,
        result: Result<Option<soundcloud::Playlist>, String>,
    },
    Resolved {
        generation: u64,
        serial: u64,
        result: Result<soundcloud::Resolved, String>,
    },
    Items {
        generation: u64,
        serial: u64,
        play: bool,
        result: Result<Vec<mix::MediaItem>, String>,
    },
    LocalLoaded {
        result: Result<LocalPlaylists, String>,
        session: Option<mix::MixSession>,
    },
    Mix(mix::Event),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalPlaylists {
    pub version: u32,
    pub next_id: u64,
    pub playlists: Vec<mix::MixedPlaylist>,
}
impl Default for LocalPlaylists {
    fn default() -> Self {
        Self {
            version: 1,
            next_id: 1,
            playlists: Vec::new(),
        }
    }
}
impl LocalPlaylists {
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut value: Self = serde_json::from_slice(bytes).map_err(|_| {
            "The local playlist file could not be read. Its original contents have been kept."
                .to_string()
        })?;
        if value.version != 1 {
            return Err("This local playlist file was written by a newer version. Its contents have been kept.".into());
        }
        let mut ids = std::collections::HashSet::new();
        if value
            .playlists
            .iter()
            .any(|p| p.id == 0 || !ids.insert(p.id))
        {
            return Err("The local playlist file contains duplicate identities. Its contents have been kept.".into());
        }
        if value.playlists.iter().any(|playlist| {
            playlist.id == u64::MAX
                || playlist.name.trim().is_empty()
                || playlist.items.iter().any(|item| !valid_media_ref(item))
        }) {
            return Err("The local playlist file contains an invalid playlist or song reference. Its contents have been kept.".into());
        }
        value.next_id = value.next_id.max(
            value
                .playlists
                .iter()
                .map(|p| p.id)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        );
        if value.next_id == u64::MAX {
            return Err("The local playlist file has exhausted its playlist identities. Its contents have been kept.".into());
        }
        Ok(value)
    }
}

/// A user's edit stays visible while independent SoundCloud reads catch up.
/// These account-scoped projections are never persisted.
#[derive(Clone, Debug, Default)]
pub struct PlaylistProjection {
    pub serial: u64,
    pub title: Option<String>,
    pub order: Option<Vec<String>>,
    pub appended: Vec<soundcloud::Track>,
    pub baseline_count: Option<usize>,
    pub tracks: HashMap<String, soundcloud::Track>,
    pub created: Option<soundcloud::Playlist>,
    pub deleted: bool,
    pub confirmed: bool,
    pub failed: bool,
    pub library_seen: bool,
    pub page_seen: bool,
}

pub struct State {
    pub source: Source,
    pub view: View,
    pub user: Option<soundcloud::User>,
    pub auth_busy: bool,
    pub auth_error: Option<String>,
    pub client_id: String,
    pub client_secret: String,
    pub generation: u64,
    pub serial: u64,
    pub search_serial: u64,
    pub search_query: String,
    pub searching: bool,
    pub tracks: Vec<soundcloud::Track>,
    pub search_playlists: Vec<soundcloud::Playlist>,
    pub track_error: Option<String>,
    pub playlist_search_error: Option<String>,
    pub library: Option<soundcloud::Library>,
    pub library_loading: bool,
    pub library_serial: u64,
    pub library_error: Option<String>,
    pub playlist: Option<(soundcloud::Playlist, Vec<soundcloud::Track>)>,
    pub playlist_loading: bool,
    pub playlist_serial: u64,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub liked: HashMap<String, bool>,
    pub liked_revisions: HashMap<String, u64>,
    pub pending_likes: HashMap<String, (u64, bool)>,
    pub mutation_serial: u64,
    pub mutation_pending: bool,
    pub pending_edit: Option<Action>,
    pub playlist_projections: HashMap<String, PlaylistProjection>,
    pub recheck_at: Option<std::time::Instant>,
    pub recheck_count: u8,
    pub forget_art_pending: bool,
    pub local: LocalPlaylists,
    pub local_ready: bool,
    pub session_writable: bool,
    pub session_authorized: bool,
    pub local_error: Option<String>,
    pub local_active: bool,
    pub snapshot: mix::MixSnapshot,
    pub draft_name: String,
    pub draft_remote_name: String,
    pub draft_link: String,
    pub draft_private: bool,
    pub resolved_items: Vec<mix::MediaItem>,
    pub resolving: bool,
    pub resolving_play: bool,
    pub play_intent: u64,
    pub resolve_play_intent: u64,
    pub resolve_target: Option<u64>,
    pub resolve_serial: u64,
    pub local_revision: u64,
    pub persisted_revision: u64,
    pub mix_serial: u64,
}
impl Default for State {
    fn default() -> Self {
        Self {
            source: Source::All,
            view: View::Library,
            user: None,
            auth_busy: false,
            auth_error: None,
            client_id: String::new(),
            client_secret: String::new(),
            generation: 0,
            serial: 0,
            search_serial: 0,
            search_query: String::new(),
            searching: false,
            tracks: Vec::new(),
            search_playlists: Vec::new(),
            track_error: None,
            playlist_search_error: None,
            library: None,
            library_loading: false,
            library_serial: 0,
            library_error: None,
            playlist: None,
            playlist_loading: false,
            playlist_serial: 0,
            error: None,
            notice: None,
            liked: HashMap::new(),
            liked_revisions: HashMap::new(),
            pending_likes: HashMap::new(),
            mutation_serial: 0,
            mutation_pending: false,
            pending_edit: None,
            playlist_projections: HashMap::new(),
            recheck_at: None,
            recheck_count: 0,
            forget_art_pending: false,
            local: LocalPlaylists::default(),
            local_ready: false,
            session_writable: false,
            session_authorized: false,
            local_error: None,
            local_active: false,
            snapshot: mix::MixSnapshot::default(),
            draft_name: String::new(),
            draft_remote_name: String::new(),
            draft_link: String::new(),
            draft_private: true,
            resolved_items: Vec::new(),
            resolving: false,
            resolving_play: false,
            play_intent: 0,
            resolve_play_intent: 0,
            resolve_target: None,
            resolve_serial: 0,
            local_revision: 0,
            persisted_revision: 0,
            mix_serial: 0,
        }
    }
}
impl State {
    pub fn next_serial(&mut self) -> u64 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ReadKind {
    Account,
    Search,
    Library,
    Playlist,
    Resolve,
    Local,
}

struct MutationJob {
    generation: u64,
    request: Request,
}

pub struct Worker {
    pub client: Arc<soundcloud::Client>,
    pub runtime: mix::MixRuntime,
    pub mutations: Http,
    api: Arc<crate::api::ApiGateway>,
    generation: Arc<AtomicU64>,
    cancel_signin: Option<tokio::sync::watch::Sender<bool>>,
    account_ready: tokio::sync::watch::Sender<bool>,
    reads: HashMap<ReadKind, tokio::task::JoinHandle<()>>,
    mutation_queue: tokio::sync::mpsc::UnboundedSender<MutationJob>,
    mutation_worker: tokio::task::JoinHandle<()>,
    notify: Arc<dyn Fn(Event) + Send + Sync>,
    state_dir: PathBuf,
}

impl Worker {
    pub fn new(
        config: EngineConfig,
        http: Http,
        store: credentials::Store,
        api: Arc<crate::api::ApiGateway>,
        state_dir: PathBuf,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Self {
        let mutations = crate::http::build_collection_client(&config.proxy)
            .map(Http::new)
            .unwrap_or_else(Http::unavailable);
        let client = Arc::new(soundcloud::Client::new(
            http.clone(),
            mutations.clone(),
            store,
        ));
        let callback = Arc::clone(&notify);
        let runtime = mix::MixRuntime::spawn(
            config,
            http,
            Arc::clone(&client),
            Arc::new(move |event| callback(Event::Mix(event))),
        );
        let generation = Arc::new(AtomicU64::new(0));
        let (account_ready, _) = tokio::sync::watch::channel(false);
        let (mutation_queue, mut jobs) = tokio::sync::mpsc::unbounded_channel::<MutationJob>();
        let writer_client = Arc::clone(&client);
        let writer_generation = Arc::clone(&generation);
        let writer_notify = Arc::clone(&notify);
        // Intent order is fixed at the channel boundary. A late Like or append
        // can never overtake an earlier write merely because tasks were polled
        // in a different order. Reads remain independent of this actor.
        let mutation_worker = tokio::spawn(async move {
            while let Some(job) = jobs.recv().await {
                if writer_generation.load(Ordering::SeqCst) != job.generation {
                    continue;
                }
                let event = mutate(job.request, &writer_client, job.generation).await;
                if writer_generation.load(Ordering::SeqCst) == job.generation {
                    writer_notify(event);
                    notify_storage_warning(&writer_client, job.generation, &writer_notify);
                }
            }
        });
        Self {
            client,
            runtime,
            mutations,
            api,
            generation,
            cancel_signin: None,
            account_ready,
            reads: HashMap::new(),
            mutation_queue,
            mutation_worker,
            notify,
            state_dir,
        }
    }

    pub fn handle(&mut self, request: Request) {
        if let Request::Mix { serial, command } = request {
            self.runtime.send_with_serial(serial, command);
            return;
        }
        if matches!(
            &request,
            Request::SignIn(_) | Request::Disconnect | Request::CancelSignIn
        ) {
            self.generation.fetch_add(1, Ordering::SeqCst);
            self.client.cancel_sign_in();
            if let Some(cancel) = self.cancel_signin.take() {
                let _ = cancel.send(true);
            }
            // Every old account read is cancelled. Local files are independent
            // of account identity and continue loading.
            self.reads.retain(|kind, task| {
                if *kind == ReadKind::Local {
                    true
                } else {
                    task.abort();
                    false
                }
            });
        }
        let generation = self.generation.load(Ordering::SeqCst);
        if matches!(&request, Request::CancelSignIn) {
            self.account_ready.send_replace(true);
            (self.notify)(Event::Auth {
                generation,
                result: self
                    .client
                    .cancel_authorization()
                    .map(|()| None)
                    .map_err(|error| error.to_string()),
            });
            return;
        }
        if matches!(
            &request,
            Request::Like { .. }
                | Request::Create { .. }
                | Request::Rename { .. }
                | Request::Delete { .. }
                | Request::Replace { .. }
                | Request::Append { .. }
        ) {
            let _ = self.mutation_queue.send(MutationJob {
                generation,
                request,
            });
            return;
        }
        let kind = match &request {
            Request::Restore | Request::SignIn(_) | Request::Disconnect => ReadKind::Account,
            Request::Search { .. } => ReadKind::Search,
            Request::Library { .. } => ReadKind::Library,
            Request::Playlist { .. } => ReadKind::Playlist,
            Request::Resolve { .. } | Request::ResolveItems { .. } => ReadKind::Resolve,
            Request::LoadLocal => ReadKind::Local,
            _ => return,
        };
        if let Some(previous) = self.reads.remove(&kind) {
            previous.abort();
        }
        let cancel = if matches!(&request, Request::SignIn(_)) {
            self.account_ready.send_replace(false);
            let (tx, rx) = tokio::sync::watch::channel(false);
            self.cancel_signin = Some(tx);
            Some(rx)
        } else {
            None
        };
        let client = Arc::clone(&self.client);
        let api = Arc::clone(&self.api);
        let notify = Arc::clone(&self.notify);
        let guard = Arc::clone(&self.generation);
        let ready = self.account_ready.clone();
        let state_dir = self.state_dir.clone();
        let task = tokio::spawn(async move {
            let event = match request {
                Request::Restore => {
                    let result = client.restore().await.map_err(|error| error.to_string());
                    ready.send_replace(true);
                    Event::Auth { generation, result }
                }
                Request::SignIn(app) => {
                    let result = client
                        .sign_in(app, cancel.expect("sign-in cancellation channel"))
                        .await
                        .map(Some)
                        .map_err(|error| error.to_string());
                    if guard.load(Ordering::SeqCst) == generation {
                        ready.send_replace(true);
                    }
                    Event::Auth { generation, result }
                }
                Request::Disconnect => {
                    let result = client
                        .disconnect()
                        .await
                        .map(|()| None)
                        .map_err(|error| error.to_string());
                    if guard.load(Ordering::SeqCst) == generation {
                        ready.send_replace(true);
                    }
                    Event::Auth { generation, result }
                }
                Request::Search { query, serial } => {
                    let (tracks, playlists) = tokio::join!(
                        client.search_tracks(&query, 50, None),
                        client.search_playlists(&query, 25, None)
                    );
                    Event::Search {
                        generation,
                        serial,
                        tracks: tracks.map_err(|error| error.to_string()),
                        playlists: playlists.map_err(|error| error.to_string()),
                    }
                }
                Request::Library { serial } => Event::Library {
                    generation,
                    serial,
                    result: client.library().await.map_err(|error| error.to_string()),
                },
                Request::Playlist { urn, serial } => {
                    let result = async {
                        let playlist = client.playlist(&urn).await?;
                        let tracks = client.playlist_tracks(&urn).await?;
                        Ok::<_, soundcloud::Error>((playlist, tracks))
                    }
                    .await
                    .map_err(|error| error.to_string());
                    Event::Playlist {
                        generation,
                        serial,
                        urn,
                        result,
                    }
                }
                Request::Resolve { url, serial } => {
                    let result = async {
                        match client.resolve(&url).await? {
                            soundcloud::Resolved::Playlist(mut playlist) => {
                                playlist.tracks = client.playlist_tracks(&playlist.urn).await?;
                                playlist.track_count = playlist.tracks.len();
                                Ok(soundcloud::Resolved::Playlist(playlist))
                            }
                            track => Ok(track),
                        }
                    }
                    .await
                    .map_err(|error: soundcloud::Error| error.to_string());
                    Event::Resolved {
                        generation,
                        serial,
                        result,
                    }
                }
                Request::ResolveItems {
                    items,
                    play,
                    serial,
                } => {
                    let result = resolve_items(items, &client, &api).await;
                    Event::Items {
                        generation,
                        serial,
                        play,
                        result,
                    }
                }
                Request::LoadLocal => {
                    let result = match tokio::fs::read(state_dir.join("mixed-playlists.json")).await
                    {
                        Ok(bytes) => LocalPlaylists::decode(&bytes),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            Ok(LocalPlaylists::default())
                        }
                        Err(_) => {
                            Err("Local playlists could not be read. The file has been kept.".into())
                        }
                    };
                    notify(Event::LocalLoaded {
                        result: result.clone(),
                        session: None,
                    });
                    let (session, writable, message) = match tokio::fs::read(
                        state_dir.join("mixed-session.json"),
                    )
                    .await
                    {
                        Ok(bytes) => match decode_session(&bytes) {
                            Ok(session) => (Some(session), true, None),
                            Err(message) => (None, false, Some(message)),
                        },
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            (None, true, None)
                        }
                        Err(_) => (
                            None,
                            false,
                            Some(
                                "The saved Local mix could not be read. Its file has been kept."
                                    .into(),
                            ),
                        ),
                    };
                    notify(Event::SessionPersistence { writable, message });
                    let mut ready = ready.subscribe();
                    loop {
                        let complete = *ready.borrow_and_update();
                        if complete {
                            break;
                        }
                        if ready.changed().await.is_err() {
                            return;
                        }
                    }
                    // The app restores this only before the first user command.
                    notify(Event::LocalLoaded { result, session });
                    return;
                }
                _ => return,
            };
            if guard.load(Ordering::SeqCst) == generation {
                notify(event);
                notify_storage_warning(&client, generation, &notify);
            }
        });
        self.reads.insert(kind, task);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.client.cancel_sign_in();
        for (_, task) in self.reads.drain() {
            task.abort();
        }
        self.mutation_worker.abort();
    }
}

fn notify_storage_warning(
    client: &soundcloud::Client,
    generation: u64,
    notify: &Arc<dyn Fn(Event) + Send + Sync>,
) {
    if let Some(warning) = client.take_storage_warning() {
        notify(Event::StorageWarning {
            generation,
            message: warning.to_string(),
        });
    }
}

async fn mutate(request: Request, client: &soundcloud::Client, generation: u64) -> Event {
    let (serial, result) = match request {
        Request::Like { urn, liked, serial } => {
            let result = client
                .set_track_liked(&urn, liked)
                .await
                .map_err(|error| error.to_string());
            return Event::Liked {
                generation,
                serial,
                urn,
                liked,
                result,
            };
        }
        Request::Create {
            name,
            private,
            serial,
        } => (
            serial,
            client.create_playlist(&name, private, &[]).await.map(Some),
        ),
        Request::Rename { urn, name, serial } => (
            serial,
            client.rename_playlist(&urn, &name).await.map(|_| None),
        ),
        Request::Delete { urn, serial } => {
            (serial, client.delete_playlist(&urn).await.map(|()| None))
        }
        Request::Replace {
            urn,
            expected,
            desired,
            serial,
        } => (
            serial,
            client
                .replace_playlist_tracks(&urn, &expected, &desired)
                .await
                .map(|_| None),
        ),
        Request::Append { urn, items, serial } => (
            serial,
            client
                .append_playlist_tracks(&urn, &items)
                .await
                .map(|_| None),
        ),
        _ => unreachable!("only mutation requests enter the mutation actor"),
    };
    Event::Mutation {
        generation,
        serial,
        result: result.map_err(|error| error.to_string()),
    }
}

fn spotify_resource(uri: &str) -> Option<(&str, &str)> {
    let mut parts = uri.split(':');
    let (Some("spotify"), Some(kind @ ("track" | "episode")), Some(id), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    (id.len() == 22 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())).then_some((kind, id))
}

fn valid_media_ref(item: &mix::MediaRef) -> bool {
    match item {
        mix::MediaRef::Spotify(uri) => spotify_resource(uri).is_some(),
        mix::MediaRef::SoundCloud(urn) => urn
            .strip_prefix("soundcloud:tracks:")
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())),
    }
}

fn decode_session(bytes: &[u8]) -> Result<mix::MixSession, String> {
    let session: mix::MixSession = serde_json::from_slice(bytes).map_err(|_| {
        "The saved Local mix could not be read. Its original file has been kept.".to_string()
    })?;
    if session.version != 1 {
        return Err(
            "This saved Local mix was written by an unsupported version. Its file has been kept."
                .into(),
        );
    }
    let valid_entry = |entry: &mix::SavedEntry| match &entry.media {
        mix::SavedMedia::Spotify(item) => {
            valid_media_ref(&mix::MediaRef::Spotify(item.uri().into()))
        }
        mix::SavedMedia::SoundCloud(urn) => {
            valid_media_ref(&mix::MediaRef::SoundCloud(urn.clone()))
        }
    };
    let entries = session
        .current
        .iter()
        .chain(&session.manual)
        .chain(&session.context)
        .chain(&session.original_context)
        .chain(&session.history);
    if entries
        .into_iter()
        .any(|entry| entry.id == 0 || entry.id == u64::MAX || !valid_entry(entry))
        || session.next_id == u64::MAX
    {
        return Err("The saved Local mix contains an invalid identity or song reference. Its file has been kept.".into());
    }
    let mut identities = std::collections::HashSet::new();
    if session
        .current
        .iter()
        .chain(&session.manual)
        .chain(&session.context)
        .any(|entry| !identities.insert(entry.id))
    {
        return Err(
            "The saved Local mix contains duplicate queue identities. Its file has been kept."
                .into(),
        );
    }
    Ok(session)
}

async fn resolve_items(
    items: Vec<mix::MediaRef>,
    soundcloud: &Arc<soundcloud::Client>,
    spotify: &Arc<crate::api::ApiGateway>,
) -> Result<Vec<mix::MediaItem>, String> {
    let soundcloud = Arc::clone(soundcloud);
    let spotify = Arc::clone(spotify);
    resolve_references(items, move |reference| {
        let soundcloud = Arc::clone(&soundcloud);
        let spotify = Arc::clone(&spotify);
        async move { resolve_one(reference, &soundcloud, &spotify).await }
    })
    .await
}

async fn resolve_references<F, Fut>(
    items: Vec<mix::MediaRef>,
    resolve: F,
) -> Result<Vec<mix::MediaItem>, String>
where
    F: Fn(mix::MediaRef) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Result<mix::MediaItem, String>> + Send + 'static,
{
    let mut output = vec![None; items.len()];
    let mut references = items.into_iter().enumerate();
    let mut pending = tokio::task::JoinSet::new();
    loop {
        while pending.len() < 4 {
            let Some((index, reference)) = references.next() else {
                break;
            };
            let resolve = resolve.clone();
            pending.spawn(async move { (index, resolve(reference).await) });
        }
        let Some(answer) = pending.join_next().await else {
            break;
        };
        let (index, item) =
            answer.map_err(|_| "The playlist lookup was interrupted. Try again.".to_string())?;
        output[index] = Some(item?);
    }
    output
        .into_iter()
        .map(|item| {
            item.ok_or_else(|| "The playlist lookup was interrupted. Try again.".to_string())
        })
        .collect()
}

fn unavailable_reference(reference: mix::MediaRef, title: &str) -> mix::MediaItem {
    match reference {
        mix::MediaRef::SoundCloud(urn) => mix::MediaItem::from(soundcloud::Track {
            urn,
            title: title.into(),
            streamable: false,
            access: soundcloud::Access::Blocked,
            ..Default::default()
        }),
        mix::MediaRef::Spotify(uri) => {
            // A disabled Track projection can retain either Spotify song or
            // episode references without pretending an unavailable episode is
            // playable merely because its URI is nonempty.
            mix::MediaItem::from(crate::api::models::PlayableItem::Track(
                crate::api::models::Track {
                    id: spotify_resource(&uri).map(|(_, id)| id.to_string()),
                    uri,
                    name: title.into(),
                    is_playable: Some(false),
                    ..Default::default()
                },
            ))
        }
    }
}

async fn resolve_one(
    reference: mix::MediaRef,
    soundcloud: &soundcloud::Client,
    spotify: &crate::api::ApiGateway,
) -> Result<mix::MediaItem, String> {
    use crate::api::{Operation, models::PlayableItem};
    match &reference {
        mix::MediaRef::SoundCloud(urn) => match soundcloud.track(urn).await {
            Ok(track) => Ok(mix::MediaItem::from(track)),
            Err(soundcloud::Error::Cancelled) => {
                Err("The SoundCloud account changed while loading this playlist.".into())
            }
            Err(error) => {
                let title = match error {
                    soundcloud::Error::NotSignedIn
                    | soundcloud::Error::SignInExpired
                    | soundcloud::Error::InvalidCredentials => {
                        "Connect SoundCloud to play this song"
                    }
                    soundcloud::Error::RateLimited { .. } => {
                        "SoundCloud request limit reached. Refresh later"
                    }
                    soundcloud::Error::Network | soundcloud::Error::ProxyConfiguration => {
                        "SoundCloud unavailable. Refresh to retry"
                    }
                    _ => "SoundCloud song unavailable",
                };
                Ok(unavailable_reference(reference, title))
            }
        },
        mix::MediaRef::Spotify(uri) => {
            let Some((kind, id)) = spotify_resource(uri) else {
                return Ok(unavailable_reference(
                    reference,
                    "Invalid Spotify song reference",
                ));
            };
            let lookup = async {
                let client = spotify.client_for(Operation::Catalog).await?;
                match kind {
                    "track" => client.track(id).await.map(PlayableItem::Track),
                    "episode" => client.episode(id).await.map(PlayableItem::Episode),
                    _ => unreachable!("validated Spotify reference"),
                }
            };
            match tokio::time::timeout(std::time::Duration::from_secs(20), lookup).await {
                Ok(Ok(item)) => Ok(mix::MediaItem::from(item)),
                Ok(Err(
                    crate::api::ApiError::NotSignedIn | crate::api::ApiError::SignInExpired { .. },
                )) => Ok(unavailable_reference(
                    reference,
                    "Connect Spotify to play this item",
                )),
                _ => Ok(unavailable_reference(
                    reference,
                    "Spotify item unavailable. Refresh to retry",
                )),
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn future_playlist_versions_are_preserved() {
        assert!(LocalPlaylists::decode(br#"{"version":2,"next_id":1,"playlists":[]}"#).is_err());
    }
    #[test]
    fn old_next_id_cannot_replace_an_existing_playlist() {
        let data = br#"{"version":1,"next_id":1,"playlists":[{"id":9,"name":"Mix","items":[]}]}"#;
        assert_eq!(LocalPlaylists::decode(data).unwrap().next_id, 10);
    }

    #[test]
    fn corrupt_or_duplicate_playlist_files_are_not_replaced_by_empty_defaults() {
        assert!(LocalPlaylists::decode(b"{unfinished").is_err());
        for ids in [[0, 1], [2, 2]] {
            let value = serde_json::json!({"version":1,"next_id":3,"playlists":[
                {"id":ids[0],"name":"First","items":[]},
                {"id":ids[1],"name":"Second","items":[]}
            ]});
            assert!(LocalPlaylists::decode(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }

    #[test]
    fn local_playlists_accept_only_provider_references_and_preserve_song_occurrences() {
        let valid = serde_json::json!({"version":1,"next_id":2,"playlists":[{"id":1,"name":"Mix","items":[
            {"provider":"spotify","id":"spotify:track:1234567890abcdefghijkl"},
            {"provider":"soundcloud","id":"soundcloud:tracks:99"},
            {"provider":"soundcloud","id":"soundcloud:tracks:99"}
        ]}]});
        let decoded = LocalPlaylists::decode(&serde_json::to_vec(&valid).unwrap()).unwrap();
        assert_eq!(decoded.playlists[0].items.len(), 3);
        assert_eq!(decoded.playlists[0].items[1], decoded.playlists[0].items[2]);
        let persisted = serde_json::to_string(&decoded).unwrap();
        for secret_field in ["client_secret", "access_token", "refresh_token"] {
            assert!(!persisted.contains(secret_field));
        }
        for bad in [
            "https://example.invalid/song",
            "spotify:track:bad?token=x",
            "spotify:local:track",
        ] {
            let mut invalid = valid.clone();
            invalid["playlists"][0]["items"][0]["id"] = bad.into();
            assert!(LocalPlaylists::decode(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
    }

    #[test]
    fn unsupported_or_invalid_sessions_stay_unrestored() {
        assert!(decode_session(b"not json").is_err());
        assert!(decode_session(br#"{"version":99}"#).is_err());
        let mut session = mix::MixSession::default();
        session.manual.push(mix::SavedEntry {
            id: 1,
            manual: true,
            media: mix::SavedMedia::SoundCloud("https://example.invalid".into()),
        });
        assert!(decode_session(&serde_json::to_vec(&session).unwrap()).is_err());
        assert!(decode_session(br#"{"version":1}"#).is_ok());
    }

    #[tokio::test]
    async fn unavailable_references_keep_order_and_occurrences_while_other_items_resolve() {
        let first = mix::MediaRef::Spotify("spotify:track:1234567890abcdefghijkl".into());
        let unavailable = mix::MediaRef::SoundCloud("soundcloud:tracks:99".into());
        let references = vec![first.clone(), unavailable.clone(), first.clone()];
        let expected = references.clone();
        let resolved = resolve_references(references, |reference| async move {
            if matches!(&reference, mix::MediaRef::SoundCloud(_)) {
                Ok(unavailable_reference(
                    reference,
                    "Connect SoundCloud to play this song",
                ))
            } else {
                tokio::task::yield_now().await;
                let mix::MediaRef::Spotify(uri) = reference else {
                    unreachable!();
                };
                Ok(mix::MediaItem::from(
                    crate::api::models::PlayableItem::Track(crate::api::models::Track {
                        uri,
                        name: "Resolved Spotify metadata".into(),
                        duration_ms: 100_000,
                        is_playable: Some(true),
                        ..Default::default()
                    }),
                ))
            }
        })
        .await
        .unwrap();
        assert_eq!(
            resolved
                .iter()
                .map(mix::MediaItem::media_ref)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(resolved[0].playable() && resolved[2].playable());
        assert!(!resolved[1].playable());
        assert_eq!(resolved[1].title(), "Connect SoundCloud to play this song");
    }
}
