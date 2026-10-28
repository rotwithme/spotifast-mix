//! The runtime, rather than a window or Spotify's queue, owns Local mix.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use librespot_core::session::Session;
use librespot_playback::player::PlayerEvent;
use tokio::sync::mpsc;

use super::stream::{SoundCloudPlayback, StreamEvent};
use super::{MediaItem, MixContext, MixSession, MixSnapshot, QueueEntry, SavedEntry, SavedMedia};
use crate::http::Http;
use crate::player::{
    EngineConfig, OutputOwner, Playback, RepeatMode, StandaloneEvent, StandalonePlayer,
};

#[derive(Clone, Debug)]
pub enum Command {
    Start {
        items: Vec<MediaItem>,
        index: usize,
        context: Option<MixContext>,
    },
    Add(Vec<MediaItem>),
    PlayNext(Vec<MediaItem>),
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Seek(u32),
    Volume(u16),
    Shuffle(bool),
    Repeat(RepeatMode),
    Clear,
    Move {
        from: usize,
        to: usize,
    },
    Remove(Vec<u64>),
    PlayEntry(u64),
    Restore(Box<MixSession>),
    RefreshSoundCloud,
    ForgetSoundCloud,
    Shutdown,
}

#[derive(Clone, Debug)]
pub enum Event {
    State(Box<MixSnapshot>),
    Error(String),
}

pub type Notify = Arc<dyn Fn(Event) + Send + Sync>;

enum Message {
    Command {
        serial: u64,
        command: Command,
    },
    Session(Option<Session>),
    Configure(EngineConfig),
    Spotify(StandaloneEvent),
    SoundCloud {
        generation: u64,
        event: StreamEvent,
    },
    Restored {
        revision: u64,
        snapshot: MixSnapshot,
    },
    Metadata {
        generation: u64,
        tracks: HashMap<String, crate::soundcloud::Track>,
    },
}

pub struct MixRuntime {
    commands: mpsc::UnboundedSender<Message>,
}

impl MixRuntime {
    /// Call on Spotifast's existing Tokio runtime. Audio decoding is the only
    /// blocking worker; network requests and the scheduler stay asynchronous.
    pub fn spawn(
        config: EngineConfig,
        http: Http,
        soundcloud: Arc<crate::soundcloud::Client>,
        notify: Notify,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let snapshot = MixSnapshot {
            volume: config.initial_volume,
            ..Default::default()
        };
        let worker = Worker {
            config,
            http,
            soundcloud,
            notify,
            commands: tx.clone(),
            snapshot,
            spotify_session: None,
            spotify: None,
            stream: None,
            loaded_generation: None,
            playing_wanted: false,
            ended_paused: None,
            restore: None,
            metadata: None,
            metadata_generation: 0,
            soundcloud_allowed: true,
        };
        tokio::spawn(worker.run(rx));
        Self { commands: tx }
    }

    pub fn send(&self, command: Command) {
        // Authentication/configuration work has no UI intent serial. It must
        // never acknowledge a command the interface has only just issued.
        let _ = self.commands.send(Message::Command { serial: 0, command });
    }

    pub fn send_with_serial(&self, serial: u64, command: Command) {
        let _ = self.commands.send(Message::Command { serial, command });
    }

    pub fn set_spotify_session(&self, session: Option<Session>) {
        let _ = self.commands.send(Message::Session(session));
    }
    pub fn configure(&self, config: EngineConfig) {
        let _ = self.commands.send(Message::Configure(config));
    }
}

impl Drop for MixRuntime {
    fn drop(&mut self) {
        let _ = self.commands.send(Message::Command {
            serial: 0,
            command: Command::Shutdown,
        });
    }
}

impl MixSnapshot {
    /// The same reducer runs before drawing a command's result and when the
    /// backend accepts it. `acknowledged` distinguishes a later position tick
    /// from confirmation of the last action a window optimistically applied.
    pub fn preview(&mut self, command: &Command) {
        let paused = self.playback == Playback::Paused;
        match command {
            Command::Start {
                items,
                index,
                context,
            } => self.start(items.clone(), *index, context.clone()),
            Command::Add(items) => self.append(items.clone(), false),
            Command::PlayNext(items) => self.append(items.clone(), true),
            Command::Play | Command::Toggle
                if self.playback != Playback::Playing && self.playback != Playback::Loading =>
            {
                if self.current.is_none() {
                    self.advance(false);
                }
                if self.current.is_some() {
                    self.playback = Playback::Playing;
                    self.position_at = Some(Instant::now());
                    self.error = None;
                    self.changed();
                }
            }
            Command::Pause | Command::Toggle => {
                self.position_ms = self.position_now();
                self.position_at = None;
                if self.current.is_some() {
                    self.playback = Playback::Paused;
                }
                self.changed();
            }
            Command::Next => {
                self.advance(false);
                if paused && self.current.is_some() {
                    self.playback = Playback::Paused;
                }
            }
            Command::Previous => {
                if self.position_now() > 3000 {
                    self.position_ms = 0;
                    self.position_at = None;
                    self.generation = self.generation.wrapping_add(1);
                } else if let Some(previous) = self.history.pop() {
                    if let Some(current) = self.current.take() {
                        if current.manual {
                            self.manual.insert(0, current);
                        } else {
                            self.context.insert(0, current);
                        }
                    }
                    self.current = Some(previous);
                    self.position_ms = 0;
                    self.position_at = None;
                    self.generation = self.generation.wrapping_add(1);
                }
                self.error = None;
                if self.current.is_some() {
                    self.playback = if paused {
                        Playback::Paused
                    } else {
                        Playback::Loading
                    };
                }
                self.changed();
            }
            Command::Seek(position) => {
                self.position_ms = self.current.as_ref().map_or(*position, |current| {
                    let duration = current.item.duration_ms();
                    if duration > 0 {
                        (*position).min(duration.saturating_sub(1))
                    } else {
                        *position
                    }
                });
                self.position_at = None;
                self.generation = self.generation.wrapping_add(1);
                if self.current.is_some() {
                    self.playback = if paused {
                        Playback::Paused
                    } else {
                        Playback::Loading
                    };
                }
                self.changed();
            }
            Command::Volume(volume) => {
                self.volume = *volume;
                self.changed();
            }
            Command::Shuffle(shuffle) => self.set_shuffle(*shuffle),
            Command::Repeat(repeat) => {
                self.repeat = *repeat;
                self.changed();
            }
            Command::Clear => {
                self.manual.clear();
                self.changed();
            }
            Command::Move { from, to } => {
                if *from < self.manual.len() && *to <= self.manual.len() {
                    let entry = self.manual.remove(*from);
                    self.manual.insert((*to).min(self.manual.len()), entry);
                    self.changed();
                }
            }
            Command::Remove(ids) => {
                self.manual.retain(|entry| !ids.contains(&entry.id));
                self.changed();
            }
            Command::PlayEntry(id) => {
                self.play_entry(*id);
            }
            Command::ForgetSoundCloud => {
                if let Some(context) = &mut self.context_info {
                    context.forget_soundcloud();
                }
                if matches!(
                    self.current.as_ref().map(|entry| &entry.item),
                    Some(MediaItem::SoundCloud(_))
                ) {
                    self.position_ms = self.position_now();
                    self.position_at = None;
                    self.playback = Playback::Paused;
                    self.generation = self.generation.wrapping_add(1);
                    self.error =
                        Some("Connect your SoundCloud account to resume this song.".into());
                }
                for entry in self
                    .current
                    .iter_mut()
                    .chain(&mut self.manual)
                    .chain(&mut self.context)
                    .chain(&mut self.original_context)
                    .chain(&mut self.history)
                {
                    if let MediaItem::SoundCloud(track) = &mut entry.item {
                        **track = crate::soundcloud::Track {
                            urn: track.urn.clone(),
                            title: "SoundCloud track".into(),
                            ..Default::default()
                        };
                    }
                }
                self.changed();
            }
            Command::Play
            | Command::Restore(_)
            | Command::RefreshSoundCloud
            | Command::Shutdown => {}
        }
    }

    fn restore_answer(&mut self, revision: u64, mut answer: MixSnapshot) -> bool {
        if self.revision != revision {
            return false;
        }
        answer.revision = revision.wrapping_add(1);
        answer.acknowledged = answer.acknowledged.max(self.acknowledged);
        *self = answer;
        true
    }
}

struct Worker {
    config: EngineConfig,
    http: Http,
    soundcloud: Arc<crate::soundcloud::Client>,
    notify: Notify,
    commands: mpsc::UnboundedSender<Message>,
    snapshot: MixSnapshot,
    spotify_session: Option<Session>,
    spotify: Option<StandalonePlayer>,
    stream: Option<SoundCloudPlayback>,
    loaded_generation: Option<u64>,
    playing_wanted: bool,
    ended_paused: Option<u64>,
    restore: Option<tokio::task::JoinHandle<()>>,
    metadata: Option<tokio::task::JoinHandle<()>>,
    metadata_generation: u64,
    soundcloud_allowed: bool,
}

impl Worker {
    fn emit(&self) {
        (self.notify)(Event::State(Box::new(self.snapshot.clone())));
    }

    fn fail(&mut self, message: String) {
        self.snapshot.position_ms = self.snapshot.position_now();
        self.snapshot.position_at = None;
        self.snapshot.playback = if self.snapshot.current.is_some() {
            Playback::Paused
        } else {
            Playback::Stopped
        };
        self.snapshot.error = Some(message.clone());
        self.snapshot.changed();
        self.playing_wanted = false;
        (self.notify)(Event::Error(message));
        self.emit();
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Message>) {
        while let Some(message) = rx.recv().await {
            match message {
                Message::Command { serial, command } => {
                    if serial != 0 {
                        if serial < self.snapshot.acknowledged {
                            continue;
                        }
                        self.snapshot.acknowledged = serial;
                    }
                    if matches!(command, Command::Shutdown) {
                        break;
                    }
                    if matches!(command, Command::ForgetSoundCloud) {
                        self.soundcloud_allowed = false;
                        self.snapshot.preview(&command);
                        self.metadata_generation = self.metadata_generation.wrapping_add(1);
                        if let Some(metadata) = self.metadata.take() {
                            metadata.abort();
                        }
                        self.playing_wanted = matches!(
                            self.snapshot.playback,
                            Playback::Playing | Playback::Loading
                        );
                        self.emit();
                        if matches!(
                            self.snapshot.current.as_ref().map(|entry| &entry.item),
                            Some(MediaItem::SoundCloud(_))
                        ) {
                            self.loaded_generation = None;
                            if let Some(stream) = self.stream.take() {
                                stream.stop().await;
                            }
                        }
                        continue;
                    }
                    if matches!(command, Command::RefreshSoundCloud) {
                        self.soundcloud_allowed = true;
                    }
                    if let Command::Restore(session) = command {
                        if let Some(restore) = self.restore.take() {
                            restore.abort();
                        }
                        let client = Arc::clone(&self.soundcloud);
                        let commands = self.commands.clone();
                        let revision = self.snapshot.revision;
                        let acknowledged = self.snapshot.acknowledged;
                        self.restore = Some(tokio::spawn(async move {
                            let mut snapshot = restore(*session, client).await;
                            snapshot.acknowledged = acknowledged;
                            let _ = commands.send(Message::Restored { revision, snapshot });
                        }));
                        continue;
                    }
                    let before_generation = self.snapshot.generation;
                    self.snapshot.preview(&command);
                    if !self.soundcloud_allowed
                        && matches!(
                            command,
                            Command::Start { .. } | Command::Add(_) | Command::PlayNext(_)
                        )
                    {
                        // A UI row or a resolve result from before disconnect
                        // may arrive late. Retain its reference, never its data.
                        self.snapshot.preview(&Command::ForgetSoundCloud);
                    }
                    self.playing_wanted = matches!(
                        self.snapshot.playback,
                        Playback::Playing | Playback::Loading
                    );
                    self.emit();
                    if before_generation != self.snapshot.generation {
                        if let Err(error) = self.load_current().await {
                            self.fail(error.to_string());
                        }
                    } else {
                        match command {
                            Command::Play | Command::Toggle if self.playing_wanted => {
                                crate::player::claim_output(OutputOwner::LocalMix).await;
                                if self.ended_paused == Some(self.snapshot.generation) {
                                    self.ended().await;
                                } else if self.loaded_generation != Some(self.snapshot.generation) {
                                    if let Err(error) = self.load_current().await {
                                        self.fail(error.to_string());
                                    }
                                } else if let Some(stream) = &self.stream {
                                    stream.pause(false);
                                } else if let Some(spotify) = &self.spotify {
                                    spotify.play();
                                }
                            }
                            Command::Pause | Command::Toggle => {
                                if let Some(stream) = &self.stream {
                                    stream.pause(true);
                                }
                                if let Some(spotify) = &self.spotify {
                                    spotify.pause();
                                }
                            }
                            Command::Volume(volume) => {
                                self.config.initial_volume = volume;
                                if let Some(stream) = &self.stream {
                                    stream.volume(volume);
                                }
                                if let Some(spotify) = &self.spotify {
                                    spotify.volume(volume);
                                }
                            }
                            Command::RefreshSoundCloud => {
                                let urns = self
                                    .snapshot
                                    .current
                                    .iter()
                                    .chain(&self.snapshot.manual)
                                    .chain(&self.snapshot.context)
                                    .chain(&self.snapshot.original_context)
                                    .chain(&self.snapshot.history)
                                    .filter_map(|entry| match &entry.item {
                                        MediaItem::SoundCloud(track) => Some(track.urn.clone()),
                                        _ => None,
                                    })
                                    .collect();
                                self.refresh_metadata(urns);
                            }
                            _ => {}
                        }
                    }
                }
                Message::Session(session) => {
                    if let (Some(spotify), Some(session)) = (&self.spotify, &session) {
                        spotify.set_session(session.clone());
                    }
                    if session.is_none() {
                        if let Some(spotify) = self.spotify.take() {
                            spotify.quiesce().await;
                        }
                        if matches!(
                            self.snapshot.current.as_ref().map(|entry| &entry.item),
                            Some(MediaItem::Spotify(_))
                        ) {
                            self.loaded_generation = None;
                            self.fail("Sign in to Spotify and enable local playback to play this Local mix track.".into());
                        }
                    }
                    self.spotify_session = session;
                }
                Message::Configure(config) => {
                    self.config = config;
                    self.snapshot.volume = self.config.initial_volume;
                    if let Some(spotify) = self.spotify.take() {
                        spotify.quiesce().await;
                    }
                    if let Err(error) = self.load_current().await {
                        self.fail(error.to_string());
                    }
                }
                Message::Spotify(event) => self.spotify_event(event).await,
                Message::SoundCloud { generation, event } => {
                    if !self.snapshot.accepts_event(generation, "SoundCloud") {
                        continue;
                    }
                    match event {
                        StreamEvent::Playing(position) | StreamEvent::Position(position) => {
                            self.position(position)
                        }
                        StreamEvent::Ended if self.playing_wanted => self.ended().await,
                        StreamEvent::Ended => {
                            self.ended_paused = Some(self.snapshot.generation);
                        }
                        StreamEvent::Error(message) => {
                            self.loaded_generation = None;
                            self.fail(message);
                        }
                    }
                }
                Message::Restored { revision, snapshot } => {
                    if !self.snapshot.restore_answer(revision, snapshot) {
                        continue;
                    }
                    self.playing_wanted = false;
                    if !self.soundcloud_allowed {
                        self.snapshot.preview(&Command::ForgetSoundCloud);
                    }
                    self.loaded_generation = None;
                    self.emit();
                }
                Message::Metadata { generation, tracks } => {
                    if !self.soundcloud_allowed
                        || generation != self.metadata_generation
                        || tracks.is_empty()
                    {
                        continue;
                    }
                    for entry in self
                        .snapshot
                        .current
                        .iter_mut()
                        .chain(&mut self.snapshot.manual)
                        .chain(&mut self.snapshot.context)
                        .chain(&mut self.snapshot.original_context)
                        .chain(&mut self.snapshot.history)
                    {
                        if let MediaItem::SoundCloud(track) = &mut entry.item
                            && let Some(fresh) = tracks.get(&track.urn)
                        {
                            **track = fresh.clone();
                        }
                    }
                    self.snapshot.changed();
                    self.emit();
                }
            }
        }
        if let Some(restore) = self.restore.take() {
            restore.abort();
        }
        if let Some(metadata) = self.metadata.take() {
            metadata.abort();
        }
        if let Some(stream) = self.stream.take() {
            stream.stop().await;
        }
        if let Some(spotify) = self.spotify.take() {
            spotify.quiesce().await;
        }
    }

    fn position(&mut self, position: u32) {
        if !self.playing_wanted {
            return;
        }
        self.snapshot.position_ms = position;
        self.snapshot.position_at = Some(Instant::now());
        self.snapshot.playback = Playback::Playing;
        self.snapshot.error = None;
        self.snapshot.changed();
        self.emit();
    }

    async fn ended(&mut self) {
        let generation = self.snapshot.generation;
        let source = self
            .snapshot
            .current
            .as_ref()
            .map(|entry| entry.item.source())
            .unwrap_or_default();
        if !self.snapshot.completed(generation, source) {
            return;
        }
        self.playing_wanted = self.snapshot.current.is_some();
        self.emit();
        if let Err(error) = self.load_current().await {
            self.fail(error.to_string());
        }
    }

    async fn spotify_event(&mut self, event: StandaloneEvent) {
        let (generation, event) = match event {
            StandaloneEvent::Player { generation, event } => (generation, event),
            StandaloneEvent::Error {
                generation,
                message,
            } => {
                if generation == self.snapshot.generation {
                    self.loaded_generation = None;
                    self.fail(message);
                }
                return;
            }
        };
        if !self.snapshot.accepts_event(generation, "Spotify") {
            return;
        }
        match event {
            PlayerEvent::Playing { position_ms, .. }
            | PlayerEvent::PositionChanged { position_ms, .. }
            | PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::Seeked { position_ms, .. } => self.position(position_ms),
            PlayerEvent::EndOfTrack { .. } if self.playing_wanted => self.ended().await,
            PlayerEvent::EndOfTrack { .. } => {
                self.ended_paused = Some(self.snapshot.generation);
            }
            PlayerEvent::Unavailable { .. } => {
                self.loaded_generation = None;
                self.fail("Spotify could not play this Local mix track. Choose another song or try again.".into());
            }
            PlayerEvent::AudioKeyUnavailable { .. } => {
                self.loaded_generation = None;
                self.fail(crate::player::AUDIO_KEY_REFUSED.into());
            }
            _ => {}
        }
    }

    async fn load_current(&mut self) -> anyhow::Result<()> {
        self.loaded_generation = None;
        self.ended_paused = None;
        if let Some(stream) = self.stream.take() {
            stream.stop().await;
        }
        if let Some(spotify) = &self.spotify {
            spotify.quiesce().await;
        }
        let Some(entry) = self.snapshot.current.clone() else {
            return Ok(());
        };
        if !self.soundcloud_allowed && matches!(entry.item, MediaItem::SoundCloud(_)) {
            anyhow::bail!("Connect your SoundCloud account to resume this song.");
        }
        let unresolved = matches!(&entry.item, MediaItem::SoundCloud(track) if track.access == crate::soundcloud::Access::Unknown);
        if !entry.item.playable() && !unresolved {
            anyhow::bail!(
                "This track is unavailable for full playback from {}.",
                entry.item.source()
            );
        }
        if self.playing_wanted {
            crate::player::claim_output(OutputOwner::LocalMix).await;
        }
        match &entry.item {
            MediaItem::Spotify(item) => {
                if self.spotify.is_none() {
                    let session = self.spotify_session.as_ref().ok_or_else(|| anyhow::anyhow!("Sign in to Spotify and enable local playback to play this Local mix track."))?;
                    let commands = self.commands.clone();
                    let notify = Arc::new(move |event| {
                        let _ = commands.send(Message::Spotify(event));
                    });
                    self.spotify = Some(StandalonePlayer::new(&self.config, session, notify)?);
                }
                let spotify = self
                    .spotify
                    .as_ref()
                    .expect("the standalone player was created");
                spotify.volume(self.snapshot.volume);
                spotify.load(
                    item.uri(),
                    self.snapshot.position_ms,
                    self.playing_wanted,
                    self.snapshot.generation,
                )?;
            }
            MediaItem::SoundCloud(track) => {
                self.refresh_metadata(HashSet::from([track.urn.clone()]));
                let generation = self.snapshot.generation;
                let commands = self.commands.clone();
                let notify = Arc::new(move |event| {
                    let _ = commands.send(Message::SoundCloud { generation, event });
                });
                let mut config = self.config.clone();
                config.initial_volume = self.snapshot.volume;
                self.stream = Some(SoundCloudPlayback::spawn(
                    config,
                    self.http.clone(),
                    Arc::clone(&self.soundcloud),
                    track.urn.clone(),
                    self.snapshot.position_ms,
                    self.playing_wanted,
                    notify,
                ));
            }
        }
        self.loaded_generation = Some(self.snapshot.generation);
        Ok(())
    }

    fn refresh_metadata(&mut self, urns: HashSet<String>) {
        if let Some(metadata) = self.metadata.take() {
            metadata.abort();
        }
        let client = Arc::clone(&self.soundcloud);
        let commands = self.commands.clone();
        self.metadata_generation = self.metadata_generation.wrapping_add(1);
        let generation = self.metadata_generation;
        self.metadata = Some(tokio::spawn(async move {
            let mut tracks = HashMap::new();
            for urn in urns.into_iter().take(10_000) {
                if let Ok(track) = client.track(&urn).await {
                    tracks.insert(urn, track);
                }
            }
            let _ = commands.send(Message::Metadata { generation, tracks });
        }));
    }
}

async fn restore(session: MixSession, client: Arc<crate::soundcloud::Client>) -> MixSnapshot {
    let mut snapshot = MixSnapshot::default();
    if session.version != 1 {
        snapshot.error = Some("This Local mix session was saved by a newer version.".into());
        return snapshot;
    }
    let entries = session
        .current
        .iter()
        .chain(&session.manual)
        .chain(&session.context)
        .chain(&session.original_context)
        .chain(&session.history);
    let mut urns = HashSet::new();
    for entry in entries.take(10_000) {
        if let SavedMedia::SoundCloud(urn) = &entry.media {
            urns.insert(urn.clone());
        }
    }
    let mut tracks = HashMap::new();
    let mut pending = tokio::task::JoinSet::new();
    let mut urns = urns.into_iter();
    loop {
        while pending.len() < 4 {
            let Some(urn) = urns.next() else {
                break;
            };
            let client = Arc::clone(&client);
            pending.spawn(async move {
                let track = client.track(&urn).await;
                (urn, track)
            });
        }
        let Some(answer) = pending.join_next().await else {
            break;
        };
        if let Ok((urn, answer)) = answer {
            let track = answer.unwrap_or_else(|_| crate::soundcloud::Track {
                urn: urn.clone(),
                title: "Unavailable SoundCloud track".into(),
                ..Default::default()
            });
            tracks.insert(urn, track);
        }
    }
    let entry = |saved: SavedEntry| -> QueueEntry {
        let item = match saved.media {
            SavedMedia::Spotify(item) => MediaItem::Spotify(item),
            SavedMedia::SoundCloud(urn) => tracks
                .get(&urn)
                .cloned()
                .unwrap_or_else(|| crate::soundcloud::Track {
                    urn,
                    title: "Unavailable SoundCloud track".into(),
                    ..Default::default()
                })
                .into(),
        };
        QueueEntry {
            id: saved.id,
            item,
            manual: saved.manual,
        }
    };
    snapshot.current = session.current.map(&entry);
    snapshot.manual = session.manual.into_iter().map(&entry).collect();
    snapshot.context = session.context.into_iter().map(&entry).collect();
    snapshot.original_context = session.original_context.into_iter().map(&entry).collect();
    snapshot.history = session.history.into_iter().map(&entry).collect();
    snapshot.context_info = session.context_info;
    if let Some(context) = &mut snapshot.context_info {
        context.forget_soundcloud();
    }
    snapshot.position_ms = session.position_ms;
    snapshot.volume = session.volume;
    snapshot.shuffle = session.shuffle;
    snapshot.repeat = RepeatMode::from_api(&session.repeat);
    snapshot.next_id = session.next_id.max(
        snapshot
            .current
            .iter()
            .chain(&snapshot.manual)
            .chain(&snapshot.context)
            .chain(&snapshot.original_context)
            .chain(&snapshot.history)
            .map(|entry| entry.id.saturating_add(1))
            .max()
            .unwrap_or(1),
    );
    snapshot.playback = if snapshot.current.is_some() {
        Playback::Paused
    } else {
        Playback::Stopped
    };
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::{PlayableItem, Track};

    fn song(title: &str) -> MediaItem {
        PlayableItem::Track(Track {
            name: title.into(),
            uri: format!("spotify:track:{title}"),
            ..Default::default()
        })
        .into()
    }

    #[test]
    fn preview_keeps_paused_next_and_clear_only_removes_manual_rows() {
        let mut snapshot = MixSnapshot::default();
        snapshot.preview(&Command::Start {
            items: vec![song("a"), song("b")],
            index: 0,
            context: None,
        });
        snapshot.preview(&Command::Add(vec![song("x"), song("x")]));
        snapshot.preview(&Command::Pause);
        snapshot.preview(&Command::Next);
        assert_eq!(snapshot.current.as_ref().unwrap().item.title(), "x");
        assert_eq!(snapshot.playback, Playback::Paused);
        snapshot.preview(&Command::Clear);
        assert!(snapshot.manual.is_empty());
        assert_eq!(snapshot.context[0].item.title(), "b");
    }

    #[test]
    fn occurrence_removal_and_moves_do_not_change_matching_context_rows() {
        let mut snapshot = MixSnapshot::default();
        snapshot.preview(&Command::Start {
            items: vec![song("a"), song("x")],
            index: 0,
            context: None,
        });
        snapshot.preview(&Command::Add(vec![song("x"), song("y"), song("x")]));
        let removed = snapshot.manual[0].id;
        snapshot.preview(&Command::Move { from: 2, to: 0 });
        snapshot.preview(&Command::Remove(vec![removed]));
        assert_eq!(snapshot.manual.len(), 2);
        assert_eq!(snapshot.manual[0].item.title(), "x");
        assert_eq!(snapshot.context[0].item.title(), "x");
    }

    fn soundcloud_song(title: &str) -> MediaItem {
        crate::soundcloud::Track {
            urn: format!("soundcloud:tracks:{title}"),
            title: title.into(),
            duration: 100_000,
            streamable: true,
            access: crate::soundcloud::Access::Playable,
            ..Default::default()
        }
        .into()
    }

    #[test]
    fn fake_decoder_completions_cross_providers_and_ignore_old_end_events() {
        let mut queue = MixSnapshot::default();
        queue.preview(&Command::Start {
            items: vec![song("one"), soundcloud_song("two"), song("three")],
            index: 0,
            context: None,
        });
        let spotify_generation = queue.generation;
        assert!(queue.completed(spotify_generation, "Spotify"));
        assert_eq!(queue.current.as_ref().unwrap().item.source(), "SoundCloud");
        let soundcloud_generation = queue.generation;
        assert!(!queue.completed(spotify_generation, "Spotify"));
        assert!(!queue.completed(soundcloud_generation, "Spotify"));
        assert!(queue.completed(soundcloud_generation, "SoundCloud"));
        assert_eq!(queue.current.as_ref().unwrap().item.title(), "three");
        assert!(!queue.completed(soundcloud_generation, "SoundCloud"));
        assert!(queue.completed(queue.generation, "Spotify"));
        assert!(queue.current.is_none());
    }

    #[test]
    fn pause_seek_and_rapid_skips_invalidate_previous_decoder_answers() {
        let mut queue = MixSnapshot::default();
        queue.preview(&Command::Start {
            items: vec![
                soundcloud_song("one"),
                soundcloud_song("two"),
                song("three"),
            ],
            index: 0,
            context: None,
        });
        let first = queue.generation;
        queue.preview(&Command::Pause);
        queue.preview(&Command::Seek(80_000));
        assert_eq!(queue.playback, Playback::Paused);
        assert_eq!(queue.position_now(), 80_000);
        assert!(!queue.accepts_event(first, "SoundCloud"));
        let seek = queue.generation;
        queue.preview(&Command::Next);
        queue.preview(&Command::Next);
        assert_eq!(queue.playback, Playback::Paused);
        assert_eq!(queue.current.as_ref().unwrap().item.title(), "three");
        assert!(!queue.completed(first, "SoundCloud"));
        assert!(!queue.completed(seek, "SoundCloud"));
    }

    #[test]
    fn blocked_soundcloud_rows_never_become_playable_from_a_label_or_url() {
        let mut track = crate::soundcloud::Track {
            urn: "soundcloud:tracks:blocked".into(),
            title: "A song".into(),
            permalink_url: "https://soundcloud.com/artist/song".into(),
            streamable: true,
            access: crate::soundcloud::Access::Preview,
            ..Default::default()
        };
        assert!(!MediaItem::from(track.clone()).playable());
        track.access = crate::soundcloud::Access::Blocked;
        assert!(!MediaItem::from(track.clone()).playable());
        track.access = crate::soundcloud::Access::Playable;
        assert!(MediaItem::from(track).playable());
    }

    #[test]
    fn a_restore_answer_cannot_undo_actions_taken_while_metadata_was_loading() {
        let mut queue = MixSnapshot::default();
        let requested_revision = queue.revision;
        let restored = MixSnapshot::default();
        queue.preview(&Command::Add(vec![song("new-choice")]));
        assert!(!queue.restore_answer(requested_revision, restored));
        assert_eq!(queue.manual[0].item.title(), "new-choice");
        let revision = queue.revision;
        queue.acknowledged = 12;
        let mut answer = queue.clone();
        answer.acknowledged = 5;
        assert!(queue.restore_answer(revision, answer));
        assert_eq!(queue.acknowledged, 12);
    }

    #[test]
    fn disconnect_forgets_soundcloud_metadata_but_preserves_queue_occurrences() {
        let mut private = match soundcloud_song("private-title") {
            MediaItem::SoundCloud(track) => *track,
            _ => unreachable!(),
        };
        private.user.username = "private-uploader".into();
        private.artwork_url = Some("https://images.example/private-cover".into());
        private.permalink_url = "https://soundcloud.com/private-uploader/private-title".into();
        private.user_favorite = true;
        let mut queue = MixSnapshot::default();
        queue.preview(&Command::Start {
            items: vec![
                private.clone().into(),
                song("spotify"),
                private.clone().into(),
            ],
            index: 0,
            context: None,
        });
        queue.preview(&Command::Add(vec![private.into()]));
        let current = queue.current.as_ref().unwrap().id;
        let manual = queue.manual[0].id;
        let generation = queue.generation;
        queue.preview(&Command::ForgetSoundCloud);
        assert_eq!(queue.playback, Playback::Paused);
        assert_eq!(queue.current.as_ref().unwrap().id, current);
        assert_eq!(queue.manual[0].id, manual);
        assert!(!queue.accepts_event(generation, "SoundCloud"));
        for entry in queue
            .current
            .iter()
            .chain(&queue.manual)
            .chain(&queue.context)
            .chain(&queue.original_context)
            .chain(&queue.history)
        {
            if let MediaItem::SoundCloud(track) = &entry.item {
                assert!(!track.urn.is_empty());
                assert_eq!(track.title, "SoundCloud track");
                assert!(track.user.username.is_empty());
                assert!(track.permalink_url.is_empty());
                assert!(track.artwork_url.is_none());
                assert!(!track.user_favorite);
                assert!(!track.is_playable());
            }
        }
        queue.preview(&Command::Next);
        queue.preview(&Command::Next);
        assert_eq!(queue.current.as_ref().unwrap().item.title(), "spotify");
        queue.playback = Playback::Playing;
        let spotify_generation = queue.generation;
        queue.preview(&Command::ForgetSoundCloud);
        assert_eq!(queue.playback, Playback::Playing);
        assert_eq!(queue.generation, spotify_generation);
    }

    #[tokio::test]
    async fn internal_work_cannot_acknowledge_or_overtake_ui_serials() {
        let dirs = crate::paths::AppDirs {
            config: ".cache/mix-serial/config".into(),
            state: ".cache/mix-serial/state".into(),
            cache: ".cache/mix-serial/cache".into(),
        };
        let client = Arc::new(crate::soundcloud::Client::new(
            Http::default(),
            Http::default(),
            crate::credentials::Store::in_memory(dirs),
        ));
        let config = EngineConfig {
            device_name: "Local mix serial test".into(),
            bitrate_kbps: 320,
            normalisation: false,
            autoplay: false,
            gapless: false,
            audio_device: None,
            initial_volume: u16::MAX,
            volume_dir: Default::default(),
            audio_cache_dir: None,
            audio_cache_limit: None,
            buffer_ms: crate::sink::DEFAULT_BUFFER_MS,
            tap: crate::vis::AudioTap::new(),
            eq: crate::eq::shared(),
            proxy: crate::settings::ProxyConfig::Off,
            local_files: None,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let runtime = MixRuntime::spawn(
            config,
            Http::default(),
            client,
            Arc::new(move |event| {
                let _ = tx.send(event);
            }),
        );
        runtime.send_with_serial(4, Command::Add(vec![song("first")]));
        runtime.send(Command::Volume(99));
        runtime.send_with_serial(5, Command::Add(vec![song("second")]));
        runtime.send_with_serial(4, Command::Add(vec![song("stale")]));
        let mut internal_seen = false;
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let Event::State(snapshot) = event {
                if snapshot.volume == 99 && snapshot.manual.len() == 1 {
                    assert_eq!(snapshot.acknowledged, 4);
                    internal_seen = true;
                }
                if snapshot.acknowledged == 5 {
                    assert_eq!(snapshot.manual.len(), 2);
                    assert_eq!(snapshot.manual[1].item.title(), "second");
                    break;
                }
            }
        }
        assert!(internal_seen);
        drop(runtime);
    }

    #[tokio::test]
    async fn failed_fresh_metadata_restoration_keeps_references_and_other_rows_paused() {
        let dirs = crate::paths::AppDirs {
            config: ".cache/mix-tests/config".into(),
            state: ".cache/mix-tests/state".into(),
            cache: ".cache/mix-tests/cache".into(),
        };
        let client = Arc::new(crate::soundcloud::Client::new(
            Http::default(),
            Http::default(),
            crate::credentials::Store::in_memory(dirs),
        ));
        let mut queue = MixSnapshot::default();
        queue.preview(&Command::Start {
            items: vec![song("one"), soundcloud_song("two")],
            index: 0,
            context: None,
        });
        queue.preview(&Command::Add(vec![soundcloud_song("three")]));
        let saved = queue.session();
        let restored = restore(saved, client).await;
        assert_eq!(restored.playback, Playback::Paused);
        assert_eq!(restored.current.as_ref().unwrap().item.title(), "one");
        assert_eq!(
            restored.manual[0].item.media_ref(),
            super::super::MediaRef::SoundCloud("soundcloud:tracks:three".into())
        );
        assert!(!restored.manual[0].item.playable());
        assert_eq!(restored.context.len(), 1);
        assert!(
            restored
                .session()
                .encode()
                .unwrap()
                .contains("soundcloud:tracks:two")
        );
    }
}
