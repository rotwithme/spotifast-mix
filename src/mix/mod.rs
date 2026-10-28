//! An independent, local queue for Spotify and SoundCloud.
//!
//! Spotify Connect remains owned by `player::Engine`. Nothing in this module
//! writes a mixed queue to Spotify, or substitutes another service's audio.

mod runtime;
mod stream;

use std::time::Instant;

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

use crate::api::models::PlayableItem;
use crate::player::{Playback, RepeatMode};

pub use runtime::{Command, Event, MixRuntime, Notify};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "provider", content = "id", rename_all = "lowercase")]
pub enum MediaRef {
    Spotify(String),
    SoundCloud(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum MediaItem {
    Spotify(Box<PlayableItem>),
    SoundCloud(Box<crate::soundcloud::Track>),
}

impl From<PlayableItem> for MediaItem {
    fn from(item: PlayableItem) -> Self {
        Self::Spotify(Box::new(item))
    }
}

impl From<crate::soundcloud::Track> for MediaItem {
    fn from(track: crate::soundcloud::Track) -> Self {
        Self::SoundCloud(Box::new(track))
    }
}

impl MediaItem {
    pub fn media_ref(&self) -> MediaRef {
        match self {
            Self::Spotify(item) => MediaRef::Spotify(item.uri().to_string()),
            Self::SoundCloud(track) => MediaRef::SoundCloud(track.urn.clone()),
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Self::Spotify(item) => item.name(),
            Self::SoundCloud(track) => &track.title,
        }
    }

    pub fn creator(&self) -> String {
        match self {
            Self::Spotify(item) => item.subtitle(),
            Self::SoundCloud(track) => track.creator().to_string(),
        }
    }

    pub fn duration_ms(&self) -> u32 {
        match self {
            Self::Spotify(item) => item.duration_ms(),
            Self::SoundCloud(track) => track.duration.min(u64::from(u32::MAX)) as u32,
        }
    }

    pub fn artwork(&self) -> Option<&str> {
        match self {
            Self::Spotify(item) => item.image(640),
            Self::SoundCloud(track) => track.artwork_url.as_deref(),
        }
    }

    pub fn permalink(&self) -> String {
        match self {
            Self::Spotify(item) => {
                let mut parts = item.uri().split(':');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some("spotify"), Some(kind), Some(id)) => {
                        format!("https://open.spotify.com/{kind}/{id}")
                    }
                    _ => item.uri().to_string(),
                }
            }
            Self::SoundCloud(track) => track.permalink_url.clone(),
        }
    }

    pub fn source(&self) -> &'static str {
        match self {
            Self::Spotify(_) => "Spotify",
            Self::SoundCloud(_) => "SoundCloud",
        }
    }

    pub fn playable(&self) -> bool {
        match self {
            Self::Spotify(item) => match item.as_ref() {
                PlayableItem::Track(track) => {
                    !track.uri.is_empty() && track.is_playable != Some(false)
                }
                PlayableItem::Episode(episode) => !episode.uri.is_empty(),
            },
            Self::SoundCloud(track) => track.is_playable(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueueEntry {
    pub id: u64,
    pub item: MediaItem,
    pub manual: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MixContext {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub id: Option<String>,
}

impl MixContext {
    fn is_soundcloud(&self) -> bool {
        self.id
            .as_deref()
            .is_some_and(|id| id.starts_with("soundcloud:"))
    }
    fn durable(&self) -> Self {
        let mut context = self.clone();
        if context.is_soundcloud() {
            context.name.clear();
        }
        context
    }
    fn forget_soundcloud(&mut self) {
        if self.is_soundcloud() {
            self.name = "SoundCloud playlist".into();
        }
    }
}

#[derive(Clone, Debug)]
pub struct MixSnapshot {
    pub current: Option<QueueEntry>,
    pub manual: Vec<QueueEntry>,
    pub context: Vec<QueueEntry>,
    pub context_info: Option<MixContext>,
    pub playback: Playback,
    pub position_ms: u32,
    pub volume: u16,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub revision: u64,
    pub generation: u64,
    pub acknowledged: u64,
    pub error: Option<String>,
    position_at: Option<Instant>,
    history: Vec<QueueEntry>,
    original_context: Vec<QueueEntry>,
    next_id: u64,
}

impl Default for MixSnapshot {
    fn default() -> Self {
        Self {
            current: None,
            manual: Vec::new(),
            context: Vec::new(),
            context_info: None,
            playback: Playback::Stopped,
            position_ms: 0,
            volume: u16::MAX,
            shuffle: false,
            repeat: RepeatMode::Off,
            revision: 0,
            generation: 0,
            acknowledged: 0,
            error: None,
            position_at: None,
            history: Vec::new(),
            original_context: Vec::new(),
            next_id: 1,
        }
    }
}

impl MixSnapshot {
    pub fn position_now(&self) -> u32 {
        let advanced = self.position_at.map_or(0, |at| {
            at.elapsed().as_millis().min(u128::from(u32::MAX)) as u32
        });
        let position = self.position_ms.saturating_add(advanced);
        self.current.as_ref().map_or(position, |current| {
            let duration = current.item.duration_ms();
            if duration == 0 {
                position
            } else {
                position.min(duration)
            }
        })
    }

    pub fn session(&self) -> MixSession {
        MixSession {
            version: 1,
            current: self.current.as_ref().map(SavedEntry::from),
            manual: self.manual.iter().map(SavedEntry::from).collect(),
            context: self.context.iter().map(SavedEntry::from).collect(),
            original_context: self.original_context.iter().map(SavedEntry::from).collect(),
            history: self
                .history
                .iter()
                .rev()
                .take(100)
                .rev()
                .map(SavedEntry::from)
                .collect(),
            context_info: self.context_info.as_ref().map(MixContext::durable),
            position_ms: self.position_now(),
            volume: self.volume,
            shuffle: self.shuffle,
            repeat: self.repeat.api_name().to_string(),
            next_id: self.next_id,
        }
    }

    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    fn accepts_event(&self, generation: u64, source: &str) -> bool {
        generation == self.generation
            && self
                .current
                .as_ref()
                .is_some_and(|entry| entry.item.source() == source)
    }

    fn completed(&mut self, generation: u64, source: &str) -> bool {
        if !self.accepts_event(generation, source) {
            return false;
        }
        self.advance(true);
        true
    }

    fn entry(&mut self, item: MediaItem, manual: bool) -> QueueEntry {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        QueueEntry { id, item, manual }
    }

    fn select(&mut self, entry: Option<QueueEntry>) {
        if let Some(previous) = self.current.take() {
            self.history.push(previous);
            if self.history.len() > 100 {
                self.history.remove(0);
            }
        }
        self.current = entry;
        self.position_ms = 0;
        self.position_at = None;
        self.error = None;
        self.generation = self.generation.wrapping_add(1);
        self.playback = if self.current.is_some() {
            Playback::Loading
        } else {
            Playback::Stopped
        };
        self.changed();
    }

    fn start(&mut self, items: Vec<MediaItem>, index: usize, context: Option<MixContext>) {
        let entries: Vec<_> = items
            .into_iter()
            .map(|item| self.entry(item, false))
            .collect();
        let selected = entries.get(index).cloned();
        if selected.is_none() {
            return;
        }
        self.original_context = entries.clone();
        self.context = if self.shuffle {
            entries
                .into_iter()
                .enumerate()
                .filter(|(position, _)| *position != index)
                .map(|(_, entry)| entry)
                .collect()
        } else {
            entries.into_iter().skip(index + 1).collect()
        };
        self.context_info = context;
        if self.shuffle {
            self.context.shuffle(&mut rand::rng());
        }
        self.select(selected);
    }

    fn append(&mut self, items: Vec<MediaItem>, first: bool) {
        let entries: Vec<_> = items
            .into_iter()
            .map(|item| self.entry(item, true))
            .collect();
        if first {
            self.manual.splice(..0, entries);
        } else {
            self.manual.extend(entries);
        }
        self.changed();
    }

    fn advance(&mut self, natural: bool) {
        if natural && self.repeat == RepeatMode::Track && self.current.is_some() {
            self.position_ms = 0;
            self.position_at = None;
            self.generation = self.generation.wrapping_add(1);
            self.playback = Playback::Loading;
            self.changed();
            return;
        }
        if self.manual.is_empty() && self.context.is_empty() && self.repeat == RepeatMode::Context {
            self.context = self.original_context.clone();
            if self.shuffle {
                self.context.shuffle(&mut rand::rng());
            }
        }
        let selected = if !self.manual.is_empty() {
            Some(self.manual.remove(0))
        } else if !self.context.is_empty() {
            Some(self.context.remove(0))
        } else {
            None
        };
        self.select(selected);
    }

    fn play_entry(&mut self, id: u64) -> bool {
        if let Some(index) = self.manual.iter().position(|entry| entry.id == id) {
            self.manual.drain(..index);
            let entry = self.manual.remove(0);
            self.select(Some(entry));
            true
        } else if let Some(index) = self.context.iter().position(|entry| entry.id == id) {
            self.manual.clear();
            self.context.drain(..index);
            let entry = self.context.remove(0);
            self.select(Some(entry));
            true
        } else {
            false
        }
    }

    fn set_shuffle(&mut self, shuffle: bool) {
        if self.shuffle == shuffle {
            return;
        }
        self.shuffle = shuffle;
        if shuffle {
            self.context.shuffle(&mut rand::rng());
        } else {
            self.context.sort_by_key(|entry| {
                self.original_context
                    .iter()
                    .position(|original| original.id == entry.id)
            });
        }
        self.changed();
    }
}

/// SoundCloud's terms require fresh catalogue data. Its durable record is a
/// provider reference, never cached metadata or a short-lived stream URL.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", content = "item", rename_all = "lowercase")]
pub enum SavedMedia {
    Spotify(Box<PlayableItem>),
    SoundCloud(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedEntry {
    pub id: u64,
    pub media: SavedMedia,
    pub manual: bool,
}

impl From<&QueueEntry> for SavedEntry {
    fn from(entry: &QueueEntry) -> Self {
        Self {
            id: entry.id,
            media: match &entry.item {
                MediaItem::Spotify(item) => SavedMedia::Spotify(item.clone()),
                MediaItem::SoundCloud(track) => SavedMedia::SoundCloud(track.urn.clone()),
            },
            manual: entry.manual,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MixSession {
    pub version: u32,
    pub current: Option<SavedEntry>,
    pub manual: Vec<SavedEntry>,
    pub context: Vec<SavedEntry>,
    pub original_context: Vec<SavedEntry>,
    pub history: Vec<SavedEntry>,
    pub context_info: Option<MixContext>,
    pub position_ms: u32,
    pub volume: u16,
    pub shuffle: bool,
    pub repeat: String,
    pub next_id: u64,
}

impl Default for MixSession {
    fn default() -> Self {
        Self {
            version: 1,
            current: None,
            manual: Vec::new(),
            context: Vec::new(),
            original_context: Vec::new(),
            history: Vec::new(),
            context_info: None,
            position_ms: 0,
            volume: u16::MAX,
            shuffle: false,
            repeat: "off".into(),
            next_id: 1,
        }
    }
}

impl MixSession {
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MixedPlaylist {
    pub id: u64,
    pub name: String,
    pub items: Vec<MediaRef>,
}

impl MixedPlaylist {
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
    pub fn decode(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::Track;

    fn song(id: &str) -> MediaItem {
        PlayableItem::Track(Track {
            uri: format!("spotify:track:{id}"),
            name: id.into(),
            ..Default::default()
        })
        .into()
    }

    #[test]
    fn manual_copies_keep_order_and_survive_a_new_context() {
        let mut queue = MixSnapshot::default();
        queue.append(vec![song("a"), song("a")], false);
        let ids: Vec<_> = queue.manual.iter().map(|entry| entry.id).collect();
        assert_ne!(ids[0], ids[1]);
        queue.start(vec![song("b"), song("c")], 0, None);
        assert_eq!(
            queue
                .manual
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            ids
        );
        queue.advance(false);
        assert_eq!(queue.current.as_ref().unwrap().id, ids[0]);
        queue.advance(false);
        assert_eq!(queue.current.as_ref().unwrap().id, ids[1]);
        queue.advance(false);
        assert_eq!(queue.current.as_ref().unwrap().item.title(), "c");
    }

    #[test]
    fn playing_a_later_row_consumes_only_the_rows_before_it() {
        let mut queue = MixSnapshot::default();
        queue.start(vec![song("a"), song("b"), song("c")], 0, None);
        queue.append(vec![song("x"), song("y"), song("z")], false);
        let chosen = queue.manual[1].id;
        assert!(queue.play_entry(chosen));
        assert_eq!(queue.current.as_ref().unwrap().item.title(), "y");
        assert_eq!(queue.manual[0].item.title(), "z");
        assert_eq!(queue.context[0].item.title(), "b");
    }

    #[test]
    fn shuffle_never_reorders_manual_rows_and_can_restore_context_order() {
        let mut queue = MixSnapshot::default();
        queue.start(vec![song("a"), song("b"), song("c"), song("d")], 0, None);
        queue.append(vec![song("x"), song("y")], false);
        let manual = queue.manual.clone();
        let context = queue.context.clone();
        queue.set_shuffle(true);
        queue.set_shuffle(false);
        assert_eq!(queue.manual, manual);
        assert_eq!(queue.context, context);
    }

    #[test]
    fn repeat_track_applies_to_end_of_track_but_not_explicit_next() {
        let mut queue = MixSnapshot::default();
        queue.start(vec![song("a"), song("b")], 0, None);
        queue.repeat = RepeatMode::Track;
        let id = queue.current.as_ref().unwrap().id;
        queue.advance(true);
        assert_eq!(queue.current.as_ref().unwrap().id, id);
        queue.advance(false);
        assert_eq!(queue.current.as_ref().unwrap().item.title(), "b");
    }

    #[test]
    fn a_chosen_row_starts_first_while_shuffle_keeps_every_other_occurrence() {
        let mut queue = MixSnapshot {
            shuffle: true,
            ..Default::default()
        };
        queue.start(vec![song("a"), song("b"), song("a"), song("d")], 2, None);
        assert_eq!(queue.current.as_ref().unwrap().item.title(), "a");
        let mut titles: Vec<_> = queue
            .context
            .iter()
            .map(|entry| entry.item.title())
            .collect();
        titles.sort_unstable();
        assert_eq!(titles, ["a", "b", "d"]);
    }

    #[test]
    fn soundcloud_session_holds_only_the_reference() {
        let track = crate::soundcloud::Track {
            urn: "soundcloud:tracks:123".into(),
            title: "Private fresh title".into(),
            artwork_url: Some("https://example.invalid/private-art".into()),
            ..Default::default()
        };
        let mut queue = MixSnapshot::default();
        queue.start(
            vec![track.into()],
            0,
            Some(MixContext {
                name: "Private remote playlist title".into(),
                id: Some("soundcloud:playlists:321".into()),
            }),
        );
        let session = queue.session();
        let json = session.encode().unwrap();
        assert!(!json.contains("Private fresh title"));
        assert!(!json.contains("private-art"));
        assert!(!json.contains("Private remote playlist title"));
        assert!(json.contains("soundcloud:playlists:321"));
        assert_eq!(MixSession::decode(&json).unwrap(), session);
    }
}
