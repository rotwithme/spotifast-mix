//! Interface projections for SoundCloud and the independent Local mix.

use super::*;
use crate::integration::{Action as MixAction, Event as MixEvent, Request, View};
use crate::mix::{Command as MixCommand, MediaItem, MediaRef, MixContext};

fn projection_count(projection: &crate::integration::PlaylistProjection) -> Option<usize> {
    projection
        .order
        .as_ref()
        .map(Vec::len)
        .or_else(|| {
            projection
                .baseline_count
                .map(|count| count + projection.appended.len())
        })
        .or_else(|| {
            projection
                .created
                .as_ref()
                .map(|playlist| playlist.track_count)
        })
}

fn project_metadata(
    projection: &crate::integration::PlaylistProjection,
    playlist: &mut crate::soundcloud::Playlist,
) {
    if let Some(title) = &projection.title {
        playlist.title = title.clone();
    }
    if let Some(count) = projection_count(projection) {
        playlist.track_count = count;
    }
}

fn page_matches(
    projection: &crate::integration::PlaylistProjection,
    playlist: &crate::soundcloud::Playlist,
    tracks: &[crate::soundcloud::Track],
) -> bool {
    if projection
        .title
        .as_ref()
        .is_some_and(|title| *title != playlist.title)
    {
        return false;
    }
    if let Some(order) = &projection.order {
        return tracks.iter().map(|track| &track.urn).eq(order.iter());
    }
    if !projection.appended.is_empty() {
        let count = projection_count(projection).unwrap_or(projection.appended.len());
        return tracks.len() >= count
            && tracks
                .iter()
                .rev()
                .take(projection.appended.len())
                .map(|track| &track.urn)
                .eq(projection.appended.iter().rev().map(|track| &track.urn));
    }
    true
}

fn project_page(
    projection: &crate::integration::PlaylistProjection,
    playlist: &mut crate::soundcloud::Playlist,
    tracks: &mut Vec<crate::soundcloud::Track>,
) {
    let matches = page_matches(projection, playlist, tracks);
    let mut known = projection.tracks.clone();
    known.extend(
        tracks
            .iter()
            .map(|track| (track.urn.clone(), track.clone())),
    );
    if let Some(order) = &projection.order {
        *tracks = order
            .iter()
            .map(|urn| {
                known
                    .get(urn)
                    .cloned()
                    .unwrap_or_else(|| crate::soundcloud::Track {
                        urn: urn.clone(),
                        title: "SoundCloud track".into(),
                        ..Default::default()
                    })
            })
            .collect();
    } else if !projection.appended.is_empty() && !matches {
        if let Some(count) = projection.baseline_count {
            tracks.truncate(count);
        }
        tracks.extend(projection.appended.clone());
    }
    project_metadata(projection, playlist);
}

impl App {
    fn integration_request(&self, request: Request) {
        self.backend.send(Command::Integration(Box::new(request)));
    }

    pub(crate) fn search_soundcloud(&mut self, query: &str) {
        if self.integration.source == crate::integration::Source::Spotify {
            return;
        }
        if self.integration.search_query == query && !self.integration.tracks.is_empty() {
            return;
        }
        let serial = self.integration.next_serial();
        self.integration.search_serial = serial;
        self.integration.search_query = query.to_string();
        self.integration.tracks.clear();
        self.integration.search_playlists.clear();
        self.integration.track_error = None;
        self.integration.playlist_search_error = None;
        self.integration.searching = !query.is_empty() && self.integration.user.is_some();
        if self.integration.searching {
            self.integration_request(Request::Search {
                query: query.into(),
                serial,
            });
        }
    }

    fn refresh_soundcloud_library(&mut self) {
        if self.integration.user.is_none() {
            return;
        }
        let serial = self.integration.next_serial();
        self.integration.library_serial = serial;
        self.integration.library_loading = true;
        self.integration.library_error = None;
        self.integration_request(Request::Library { serial });
    }

    fn load_soundcloud_playlist(&mut self, urn: String) {
        let serial = self.integration.next_serial();
        self.integration.playlist_serial = serial;
        self.integration.playlist_loading = self.integration.view == View::Playlist(urn.clone());
        self.integration.error = None;
        self.integration_request(Request::Playlist { urn, serial });
    }

    fn activate_mix(&mut self) {
        if !self.integration.local_active {
            if self.now_playing().is_some_and(|now| now.playing) {
                self.toggle_play();
            }
            self.integration.local_active = true;
            self.frame_now = None;
        }
    }

    fn send_mix(&mut self, command: MixCommand) {
        if matches!(
            command,
            MixCommand::Start { .. }
                | MixCommand::Play
                | MixCommand::Pause
                | MixCommand::Toggle
                | MixCommand::Next
                | MixCommand::Previous
                | MixCommand::Seek(_)
                | MixCommand::PlayEntry(_)
        ) {
            self.supersede_mix_resolution();
        }
        if matches!(
            command,
            MixCommand::Start { .. }
                | MixCommand::Add(_)
                | MixCommand::PlayNext(_)
                | MixCommand::Clear
        ) {
            self.integration.session_authorized = true;
            self.integration.session_writable = true;
        }
        let serial = self.integration.next_serial();
        self.integration.mix_serial = serial;
        self.integration.snapshot.preview(&command);
        self.frame_now = None;
        self.integration_request(Request::Mix { serial, command });
        self.persist_mix_session();
    }

    pub(crate) fn supersede_mix_resolution(&mut self) {
        self.integration.play_intent = self.integration.play_intent.wrapping_add(1);
        if self.integration.resolving_play {
            self.integration.resolving = false;
            self.integration.resolving_play = false;
            self.integration.resolve_target = None;
            self.integration.resolve_serial = self.integration.next_serial();
        }
    }

    fn cancel_mix_resolution(&mut self) {
        self.supersede_mix_resolution();
        self.integration.resolving = false;
        self.integration.resolving_play = false;
        self.integration.resolve_target = None;
        self.integration.resolve_serial = self.integration.next_serial();
    }

    pub(crate) fn cancel_spotify_import(&mut self) {
        if self.integration.spotify_import.take().is_some() {
            self.integration.notice = None;
        }
        self.integration_request(Request::CancelSpotifyImport);
    }

    fn create_local_with_items(&mut self, name: String, items: Vec<MediaRef>) -> bool {
        if !self.valid_local_additions(&items) || name.trim().is_empty() {
            return false;
        }
        let id = self.integration.local.next_id;
        if id == 0 || id >= u64::MAX - 1 {
            self.integration.error = Some("Local playlist identities have been exhausted.".into());
            return false;
        }
        self.integration.local.next_id = id + 1;
        let metadata_items = items.clone();
        self.integration
            .local
            .playlists
            .push(crate::mix::MixedPlaylist {
                id,
                name: name.trim().into(),
                items,
            });
        self.integration.view = View::LocalPlaylist(id);
        self.integration.draft_name.clear();
        self.integration.resolved_items.clear();
        self.open(Page::Mix);
        self.persist_local_playlists();
        // Creating a destination must leave a pending song add or Play intact.
        if !metadata_items.is_empty() && !self.integration.resolving {
            self.resolve_mix_items(metadata_items, false, Some(id));
        }
        true
    }

    fn valid_local_additions(&mut self, items: &[MediaRef]) -> bool {
        if !self.integration.local_ready {
            self.integration.error =
                Some("Local playlists are not ready. Nothing was added.".into());
            return false;
        }
        if items
            .iter()
            .any(|item| !crate::integration::valid_media_ref(item))
        {
            self.integration.error = Some(
                "The selection contains an unsupported song reference. Nothing was added.".into(),
            );
            return false;
        }
        true
    }

    fn add_local_items(&mut self, id: u64, items: Vec<MediaRef>) -> bool {
        if !self.valid_local_additions(&items) {
            return false;
        }
        let Some(playlist) = self
            .integration
            .local
            .playlists
            .iter_mut()
            .find(|playlist| playlist.id == id)
        else {
            self.integration.notice = None;
            self.integration.error = Some(
                "That local playlist was deleted. Choose another playlist and add the songs again."
                    .into(),
            );
            return false;
        };
        playlist.items.extend(items);
        let updated = playlist.items.clone();
        self.persist_local_playlists();
        if self.integration.view == View::LocalPlaylist(id) && !self.integration.resolving_play {
            self.resolve_mix_items(updated, false, Some(id));
        }
        self.integration.notice = Some("Added to local playlist".into());
        true
    }

    fn resolve_mix_items(&mut self, items: Vec<MediaRef>, play: bool, target: Option<u64>) {
        self.cancel_mix_resolution();
        let serial = self.integration.next_serial();
        self.integration.resolve_serial = serial;
        self.integration.resolve_play_intent = self.integration.play_intent;
        self.integration.resolve_target = target;
        self.integration.resolving_play = play;
        self.integration.resolving = true;
        self.integration_request(Request::ResolveItems {
            items,
            play,
            serial,
        });
    }

    fn known_soundcloud_track(&self, urn: &str) -> crate::soundcloud::Track {
        self.integration
            .playlist
            .as_ref()
            .and_then(|(_, tracks)| tracks.iter().find(|track| track.urn == urn))
            .cloned()
            .or_else(|| {
                self.integration
                    .tracks
                    .iter()
                    .find(|track| track.urn == urn)
                    .cloned()
            })
            .or_else(|| {
                self.integration
                    .library
                    .as_ref()
                    .and_then(|library| library.liked_tracks.iter().find(|track| track.urn == urn))
                    .cloned()
            })
            .or_else(|| {
                self.integration
                    .resolved_items
                    .iter()
                    .find_map(|item| match item {
                        MediaItem::SoundCloud(track) if track.urn == urn => Some((**track).clone()),
                        _ => None,
                    })
            })
            .unwrap_or_else(|| crate::soundcloud::Track {
                urn: urn.into(),
                title: "SoundCloud track".into(),
                ..Default::default()
            })
    }

    fn overlay_soundcloud_cache(&mut self) {
        let projections = self.integration.playlist_projections.clone();
        if let Some(library) = &mut self.integration.library {
            for (urn, projection) in &projections {
                if projection.deleted {
                    library.playlists.retain(|playlist| &playlist.urn != urn);
                    library
                        .liked_playlists
                        .retain(|playlist| &playlist.urn != urn);
                } else {
                    if let Some(created) = &projection.created
                        && !library
                            .playlists
                            .iter()
                            .any(|playlist| &playlist.urn == urn)
                    {
                        library.playlists.insert(0, created.clone());
                    }
                    for playlist in library
                        .playlists
                        .iter_mut()
                        .chain(&mut library.liked_playlists)
                    {
                        if &playlist.urn == urn {
                            project_metadata(projection, playlist);
                        }
                    }
                }
            }
        }
        if let Some((playlist, tracks)) = &mut self.integration.playlist
            && let Some(projection) = projections.get(&playlist.urn)
        {
            project_page(projection, playlist, tracks);
        }
    }

    fn schedule_soundcloud_recheck(&mut self) {
        self.integration
            .playlist_projections
            .retain(|_, projection| {
                !(projection.confirmed && projection.library_seen && projection.page_seen)
            });
        if self
            .integration
            .playlist_projections
            .values()
            .any(|projection| projection.confirmed && !projection.failed)
        {
            if self.integration.recheck_count < 3 {
                self.integration.recheck_at = Some(Instant::now() + Duration::from_secs(2));
            } else {
                self.integration.recheck_at = None;
                self.integration.error = Some("SoundCloud has not reflected the saved playlist change yet. Your edit stays visible. Refresh to check again.".into());
            }
        } else {
            self.integration.recheck_at = None;
        }
    }

    pub(crate) fn recheck_soundcloud_if_due(&mut self, ctx: &egui::Context) {
        let Some(due) = self.integration.recheck_at else {
            return;
        };
        if self.integration.user.is_none() {
            self.integration.recheck_at = None;
            return;
        }
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            ctx.request_repaint_after(wait);
            return;
        }
        self.integration.recheck_at = None;
        self.integration.recheck_count = self.integration.recheck_count.saturating_add(1);
        self.refresh_soundcloud_library();
        let urn = self
            .integration
            .playlist_projections
            .iter()
            .find(|(_, projection)| {
                projection.confirmed && !projection.failed && !projection.page_seen
            })
            .map(|(urn, _)| urn.clone());
        if let Some(urn) = urn {
            self.load_soundcloud_playlist(urn);
        }
    }

    pub(crate) fn apply_integration(&mut self, action: MixAction) {
        self.integration.error = None;
        if matches!(
            &action,
            MixAction::CreateRemote { .. }
                | MixAction::RenameRemote { .. }
                | MixAction::DeleteRemote(_)
                | MixAction::ReplaceRemote { .. }
                | MixAction::AppendRemote { .. }
        ) {
            if self.integration.mutation_pending {
                return;
            }
            self.integration.pending_edit = Some(action.clone());
        }
        match action {
            MixAction::Open(view) => {
                self.integration.view = view.clone();
                self.open(Page::Mix);
                if matches!(view, View::Library) && self.integration.library.is_none() {
                    self.refresh_soundcloud_library();
                }
                if let View::LocalPlaylist(id) = view {
                    self.integration.resolved_items.clear();
                    self.integration.draft_name.clear();
                    if let Some(playlist) =
                        self.integration.local.playlists.iter().find(|p| p.id == id)
                    {
                        self.apply_integration(MixAction::ResolveItems {
                            items: playlist.items.clone(),
                            play: false,
                        });
                    }
                }
            }
            MixAction::Source(source) => {
                self.integration.source = source;
                self.open(Page::Search);
                self.search.results = Loadable::NotLoaded;
                self.run_search(self.search.query.trim().to_string());
            }
            MixAction::SignIn => {
                match crate::soundcloud::AppCredentials::new(
                    self.integration.client_id.clone(),
                    std::mem::take(&mut self.integration.client_secret),
                ) {
                    Ok(app) => {
                        self.cancel_mix_resolution();
                        self.integration.auth_busy = true;
                        self.integration.auth_error = None;
                        self.integration.generation = self.integration.generation.wrapping_add(1);
                        self.integration_request(Request::SignIn(app));
                    }
                    Err(error) => self.integration.auth_error = Some(error.to_string()),
                }
            }
            MixAction::CancelSignIn => {
                self.cancel_mix_resolution();
                self.integration.generation = self.integration.generation.wrapping_add(1);
                self.integration.auth_busy = false;
                self.integration_request(Request::CancelSignIn);
            }
            MixAction::Disconnect => {
                self.cancel_mix_resolution();
                self.integration.forget_art_pending = true;
                self.integration.generation = self.integration.generation.wrapping_add(1);
                self.integration.auth_busy = false;
                self.integration.user = None;
                self.integration.library = None;
                self.integration.playlist = None;
                self.integration.tracks.clear();
                self.integration.search_playlists.clear();
                self.integration.liked.clear();
                self.integration.liked_revisions.clear();
                self.integration.pending_likes.clear();
                self.integration.pending_edit = None;
                self.integration.playlist_projections.clear();
                self.integration.recheck_at = None;
                self.integration.recheck_count = 0;
                self.integration.mutation_pending = false;
                self.integration.resolved_items.clear();
                self.integration.searching = false;
                self.integration.library_loading = false;
                self.integration.playlist_loading = false;
                self.send_mix(MixCommand::ForgetSoundCloud);
                if self
                    .integration
                    .snapshot
                    .current
                    .as_ref()
                    .is_some_and(|entry| matches!(entry.item, MediaItem::SoundCloud(_)))
                {
                    self.send_mix(MixCommand::Pause);
                }
                self.integration_request(Request::Disconnect);
            }
            MixAction::Refresh => {
                if self
                    .integration
                    .playlist_projections
                    .values()
                    .any(|projection| projection.failed)
                {
                    self.integration
                        .playlist_projections
                        .retain(|_, projection| !projection.failed);
                    self.integration.pending_edit = None;
                }
                self.integration.recheck_count = 0;
                self.refresh_soundcloud_library();
                if let View::Playlist(urn) = self.integration.view.clone() {
                    self.load_soundcloud_playlist(urn);
                }
            }
            MixAction::OpenPlaylist(urn) => {
                self.integration.view = View::Playlist(urn.clone());
                self.integration.playlist = None;
                self.integration.draft_remote_name.clear();
                self.open(Page::Mix);
                self.load_soundcloud_playlist(urn);
            }
            MixAction::OpenLink(url) => {
                let target = match self.integration.view {
                    View::LocalPlaylist(id) => Some(id),
                    _ => None,
                };
                self.cancel_mix_resolution();
                let serial = self.integration.next_serial();
                self.integration.resolve_serial = serial;
                self.integration.resolving = true;
                self.integration.resolve_target = target;
                self.integration_request(Request::Resolve { url, serial });
            }
            MixAction::Like { urn, liked } => {
                let serial = self.integration.next_serial();
                self.integration.liked.insert(urn.clone(), liked);
                self.integration.liked_revisions.insert(urn.clone(), serial);
                self.integration
                    .pending_likes
                    .insert(urn.clone(), (serial, liked));
                self.integration_request(Request::Like { urn, liked, serial });
            }
            MixAction::CreateRemote { name, private } => {
                self.integration.notice = Some(format!("Creating SoundCloud playlist: {name}"));
                let serial = self.begin_soundcloud_mutation();
                self.integration_request(Request::Create {
                    name,
                    private,
                    serial,
                });
            }
            MixAction::RenameRemote { urn, name } => {
                let serial = self.begin_soundcloud_mutation();
                let projection = self
                    .integration
                    .playlist_projections
                    .entry(urn.clone())
                    .or_default();
                projection.serial = serial;
                projection.title = Some(name.clone());
                projection.confirmed = false;
                projection.failed = false;
                projection.library_seen = false;
                projection.page_seen = false;
                self.overlay_soundcloud_cache();
                self.integration_request(Request::Rename { urn, name, serial });
            }
            MixAction::DeleteRemote(urn) => {
                self.integration.view = View::Library;
                if let Some(library) = &mut self.integration.library {
                    library.playlists.retain(|playlist| playlist.urn != urn);
                    library
                        .liked_playlists
                        .retain(|playlist| playlist.urn != urn);
                }
                let serial = self.begin_soundcloud_mutation();
                self.integration.playlist_projections.insert(
                    urn.clone(),
                    crate::integration::PlaylistProjection {
                        serial,
                        deleted: true,
                        page_seen: true,
                        ..Default::default()
                    },
                );
                self.integration_request(Request::Delete { urn, serial });
            }
            MixAction::ReplaceRemote {
                urn,
                expected,
                desired,
            } => {
                let serial = self.begin_soundcloud_mutation();
                let known: std::collections::HashMap<_, _> = desired
                    .iter()
                    .chain(&expected)
                    .map(|urn| (urn.clone(), self.known_soundcloud_track(urn)))
                    .collect();
                let projection = self
                    .integration
                    .playlist_projections
                    .entry(urn.clone())
                    .or_default();
                projection.serial = serial;
                projection.order = Some(desired.clone());
                projection.tracks.extend(known);
                projection.appended.clear();
                projection.confirmed = false;
                projection.failed = false;
                projection.library_seen = false;
                projection.page_seen = false;
                if let Some((playlist, tracks)) = &mut self.integration.playlist
                    && playlist.urn == urn
                {
                    let known: std::collections::HashMap<_, _> = tracks
                        .iter()
                        .map(|track| (track.urn.clone(), track.clone()))
                        .collect();
                    *tracks = desired
                        .iter()
                        .filter_map(|urn| known.get(urn).cloned())
                        .collect();
                    playlist.track_count = desired.len();
                }
                self.overlay_soundcloud_cache();
                self.integration_request(Request::Replace {
                    urn,
                    expected,
                    desired,
                    serial,
                });
            }
            MixAction::AppendRemote { urn, items } => {
                let serial = self.begin_soundcloud_mutation();
                let additions: Vec<_> = items
                    .iter()
                    .map(|urn| self.known_soundcloud_track(urn))
                    .collect();
                let page = self
                    .integration
                    .playlist
                    .as_ref()
                    .filter(|(playlist, _)| playlist.urn == urn)
                    .map(|(_, tracks)| tracks.clone());
                let library_playlist = self.integration.library.as_ref().and_then(|library| {
                    library
                        .playlists
                        .iter()
                        .chain(&library.liked_playlists)
                        .find(|playlist| playlist.urn == urn)
                });
                let complete = page.or_else(|| {
                    library_playlist
                        .filter(|playlist| playlist.track_count == playlist.tracks.len())
                        .map(|playlist| playlist.tracks.clone())
                });
                let count = library_playlist.map(|playlist| playlist.track_count);
                let projection = self
                    .integration
                    .playlist_projections
                    .entry(urn.clone())
                    .or_default();
                projection.serial = serial;
                projection.confirmed = false;
                projection.failed = false;
                projection.library_seen = false;
                projection.page_seen = false;
                if let Some(tracks) = complete {
                    let mut order: Vec<_> = tracks.iter().map(|track| track.urn.clone()).collect();
                    order.extend(items.clone());
                    projection.order = Some(order);
                    projection
                        .tracks
                        .extend(tracks.into_iter().map(|track| (track.urn.clone(), track)));
                    projection.appended.clear();
                } else {
                    if projection.baseline_count.is_none() {
                        projection.baseline_count = count;
                    }
                    projection.appended.extend(additions.clone());
                }
                projection.tracks.extend(
                    additions
                        .into_iter()
                        .map(|track| (track.urn.clone(), track)),
                );
                self.overlay_soundcloud_cache();
                self.integration_request(Request::Append { urn, items, serial });
            }
            MixAction::CreateLocal(name) => {
                self.create_local_with_items(name, Vec::new());
            }
            MixAction::CreateLocalWithItems { name, items } => {
                self.create_local_with_items(name, items);
            }
            MixAction::ImportSpotifyPlaylist { uri, name, target } => {
                if !self.valid_local_additions(&[]) {
                    return;
                }
                let Some(account) = self
                    .user_id()
                    .filter(|_| self.is_connected())
                    .map(str::to_string)
                else {
                    self.integration.error =
                        Some("Connect Spotify before importing a playlist.".into());
                    return;
                };
                if target.is_some_and(|id| {
                    !self
                        .integration
                        .local
                        .playlists
                        .iter()
                        .any(|playlist| playlist.id == id)
                }) {
                    self.integration.error =
                        Some("That local playlist was deleted. Choose another playlist.".into());
                    return;
                }
                let serial = self.integration.next_serial();
                self.integration.error = None;
                self.apply_integration(MixAction::Open(
                    target.map_or(View::Library, View::LocalPlaylist),
                ));
                self.integration.spotify_import = Some(crate::integration::SpotifyImport {
                    serial,
                    name,
                    target,
                    account: account.clone(),
                });
                self.integration.notice = Some("Loading the complete Spotify playlist…".into());
                self.integration_request(Request::ImportSpotifyPlaylist {
                    uri,
                    serial,
                    account,
                });
            }
            MixAction::CancelSpotifyImport => self.cancel_spotify_import(),
            MixAction::RenameLocal { id, name } => {
                if let Some(playlist) = self
                    .integration
                    .local
                    .playlists
                    .iter_mut()
                    .find(|p| p.id == id)
                    && !name.trim().is_empty()
                {
                    playlist.name = name.trim().into();
                }
                self.persist_local_playlists();
            }
            MixAction::DeleteLocal(id) => {
                if self
                    .integration
                    .spotify_import
                    .as_ref()
                    .is_some_and(|pending| pending.target == Some(id))
                {
                    self.cancel_spotify_import();
                }
                if self.integration.resolve_target == Some(id) {
                    self.cancel_mix_resolution();
                }
                self.integration.local.playlists.retain(|p| p.id != id);
                self.integration.view = View::Library;
                self.integration.resolved_items.clear();
                self.persist_local_playlists();
            }
            MixAction::AddLocal { id, items } => {
                self.add_local_items(id, items);
            }
            MixAction::RemoveLocal { id, index } => {
                if let Some(playlist) = self
                    .integration
                    .local
                    .playlists
                    .iter_mut()
                    .find(|p| p.id == id)
                    && index < playlist.items.len()
                {
                    playlist.items.remove(index);
                }
                self.integration.resolved_items.clear();
                self.persist_local_playlists();
            }
            MixAction::MoveLocal { id, from, to } => {
                if let Some(playlist) = self
                    .integration
                    .local
                    .playlists
                    .iter_mut()
                    .find(|p| p.id == id)
                    && from < playlist.items.len()
                    && to < playlist.items.len()
                {
                    let item = playlist.items.remove(from);
                    playlist.items.insert(to, item);
                }
                self.integration.resolved_items.clear();
                self.persist_local_playlists();
            }
            MixAction::PlayLocal(id) => {
                if let Some(playlist) = self.integration.local.playlists.iter().find(|p| p.id == id)
                {
                    self.resolve_mix_items(playlist.items.clone(), true, Some(id));
                }
            }
            MixAction::ResolveItems { items, play } => {
                let target = match self.integration.view {
                    View::LocalPlaylist(id) => Some(id),
                    _ => None,
                };
                self.resolve_mix_items(items, play, target);
            }
            MixAction::Command(command) => {
                if matches!(
                    &command,
                    MixCommand::Start { .. }
                        | MixCommand::Play
                        | MixCommand::Toggle
                        | MixCommand::PlayEntry(_)
                ) {
                    self.activate_mix();
                }
                if matches!(&command, MixCommand::Add(_) | MixCommand::PlayNext(_)) {
                    self.integration.notice = Some("Added to Local mix".into());
                }
                self.send_mix(command);
            }
            MixAction::SelectMix(selected) => {
                if selected {
                    self.activate_mix();
                } else {
                    self.send_mix(MixCommand::Pause);
                    self.integration.local_active = false;
                    self.frame_now = None;
                }
            }
        }
    }

    fn begin_soundcloud_mutation(&mut self) -> u64 {
        let serial = self.integration.next_serial();
        self.integration.mutation_serial = serial;
        self.integration.mutation_pending = true;
        self.integration.recheck_count = 0;
        self.integration.recheck_at = None;
        serial
    }

    pub(crate) fn handle_integration_event(&mut self, event: MixEvent) {
        self.mark_soundcloud_art(&event);
        match event {
            MixEvent::SpotifyPlaylistImported {
                serial,
                account,
                result,
            } => {
                if !self
                    .integration
                    .spotify_import
                    .as_ref()
                    .is_some_and(|pending| pending.serial == serial && pending.account == account)
                {
                    return;
                }
                let pending = self
                    .integration
                    .spotify_import
                    .take()
                    .expect("matching import");
                self.integration.notice = None;
                if !self.is_connected() || self.user_id() != Some(account.as_str()) {
                    return;
                }
                match result {
                    Ok(items) => {
                        let count = items.len();
                        let applied = match pending.target {
                            Some(id) => self.add_local_items(id, items),
                            None => self.create_local_with_items(pending.name, items),
                        };
                        if applied {
                            self.integration.notice = Some(format!(
                                "Added {count} songs from Spotify to the local playlist"
                            ));
                        }
                    }
                    Err(error) => self.integration.error = Some(error),
                }
            }
            MixEvent::SessionPersistence { writable, message } => {
                self.integration.session_writable = writable || self.integration.session_authorized;
                if !self.integration.session_authorized
                    && let Some(message) = message
                {
                    self.integration.notice = Some(message);
                }
            }
            MixEvent::StorageWarning {
                generation,
                message,
            } if generation == self.integration.generation => {
                self.integration.notice = Some(message);
            }
            MixEvent::LocalLoaded { result, session } => {
                if self.integration.local_revision == 0 {
                    match result {
                        Ok(local) => {
                            self.integration.local = local;
                            self.integration.local_ready = true;
                        }
                        Err(e) => self.integration.local_error = Some(e),
                    }
                }
                if let Some(session) = session
                    && self.integration.mix_serial == 0
                    && !self.integration.resolving_play
                {
                    self.send_mix(MixCommand::Restore(Box::new(session)));
                    if self.integration.user.is_some() {
                        self.send_mix(MixCommand::RefreshSoundCloud);
                    }
                }
            }
            MixEvent::Mix(crate::mix::Event::State(snapshot)) => {
                if snapshot.acknowledged >= self.integration.mix_serial {
                    self.integration.snapshot = *snapshot;
                    self.frame_now = None;
                }
            }
            MixEvent::Mix(crate::mix::Event::Error(error)) => {
                self.integration.error = Some(error.clone());
                self.toast_error(error);
            }
            MixEvent::Auth { generation, result } if generation >= self.integration.generation => {
                self.integration.generation = generation;
                self.integration.auth_busy = false;
                match result {
                    Ok(user) => {
                        self.integration.user = user;
                        self.integration.auth_error = None;
                        self.refresh_soundcloud_library();
                        if self.integration.user.is_some()
                            && (self.integration.snapshot.current.is_some()
                                || !self.integration.snapshot.manual.is_empty()
                                || !self.integration.snapshot.context.is_empty())
                        {
                            self.send_mix(MixCommand::RefreshSoundCloud);
                        }
                        if !self.integration.search_query.is_empty() {
                            let query = std::mem::take(&mut self.integration.search_query);
                            self.search_soundcloud(&query);
                        }
                    }
                    Err(error) => self.integration.auth_error = Some(error),
                }
            }
            MixEvent::Search {
                generation,
                serial,
                tracks,
                playlists,
            } if generation == self.integration.generation
                && serial == self.integration.search_serial =>
            {
                self.integration.searching = false;
                match tracks {
                    Ok(page) => self.integration.tracks = page.collection,
                    Err(error) => self.integration.track_error = Some(error),
                }
                match playlists {
                    Ok(page) => self.integration.search_playlists = page.collection,
                    Err(error) => self.integration.playlist_search_error = Some(error),
                }
            }
            MixEvent::Library {
                generation,
                serial,
                result,
            } if generation == self.integration.generation
                && serial == self.integration.library_serial =>
            {
                self.integration.library_loading = false;
                match result {
                    Ok(mut library) => {
                        let actual: std::collections::HashSet<_> = library
                            .liked_tracks
                            .iter()
                            .map(|track| track.urn.clone())
                            .collect();
                        let known: std::collections::HashSet<_> = self
                            .integration
                            .liked
                            .keys()
                            .cloned()
                            .chain(actual.iter().cloned())
                            .collect();
                        for urn in known {
                            if !self.integration.pending_likes.contains_key(&urn)
                                && self
                                    .integration
                                    .liked_revisions
                                    .get(&urn)
                                    .copied()
                                    .unwrap_or(0)
                                    < serial
                            {
                                self.integration
                                    .liked
                                    .insert(urn.clone(), actual.contains(&urn));
                            }
                        }
                        for (urn, projection) in &mut self.integration.playlist_projections {
                            let actual = library
                                .playlists
                                .iter()
                                .chain(&library.liked_playlists)
                                .find(|playlist| &playlist.urn == urn);
                            let matched = if projection.deleted {
                                actual.is_none()
                            } else {
                                actual.is_some_and(|playlist| {
                                    projection
                                        .title
                                        .as_ref()
                                        .is_none_or(|title| *title == playlist.title)
                                        && projection_count(projection)
                                            .is_none_or(|count| count == playlist.track_count)
                                })
                            };
                            if projection.confirmed && serial > projection.serial {
                                projection.library_seen = matched;
                            }
                            if projection.deleted {
                                library.playlists.retain(|playlist| &playlist.urn != urn);
                                library
                                    .liked_playlists
                                    .retain(|playlist| &playlist.urn != urn);
                            } else {
                                if let Some(created) = &projection.created
                                    && !library
                                        .playlists
                                        .iter()
                                        .any(|playlist| &playlist.urn == urn)
                                {
                                    library.playlists.insert(0, created.clone());
                                }
                                for playlist in library
                                    .playlists
                                    .iter_mut()
                                    .chain(&mut library.liked_playlists)
                                {
                                    if &playlist.urn == urn {
                                        project_metadata(projection, playlist);
                                    }
                                }
                            }
                        }
                        self.integration.library = Some(library);
                        self.schedule_soundcloud_recheck();
                    }
                    Err(error) => {
                        self.integration.library_error = Some(error);
                        self.schedule_soundcloud_recheck();
                    }
                }
            }
            MixEvent::Playlist {
                generation,
                serial,
                urn,
                result,
            } if generation == self.integration.generation
                && serial == self.integration.playlist_serial =>
            {
                let visible = self.integration.view == View::Playlist(urn.clone());
                if visible {
                    self.integration.playlist_loading = false;
                }
                match result {
                    Ok((mut playlist, mut tracks)) => {
                        if let Some(projection) =
                            self.integration.playlist_projections.get_mut(&urn)
                        {
                            if projection.confirmed && serial > projection.serial {
                                projection.page_seen = page_matches(projection, &playlist, &tracks);
                            }
                            project_page(projection, &mut playlist, &mut tracks);
                        }
                        if visible
                            && !self
                                .integration
                                .playlist_projections
                                .get(&urn)
                                .is_some_and(|projection| projection.deleted)
                        {
                            self.integration.playlist = Some((playlist, tracks));
                        }
                        self.schedule_soundcloud_recheck();
                    }
                    Err(error) => {
                        if visible {
                            self.integration.error = Some(error);
                        }
                        self.schedule_soundcloud_recheck();
                    }
                }
            }
            MixEvent::Liked {
                generation,
                serial,
                urn,
                liked,
                result,
            } if generation == self.integration.generation => {
                if self.integration.pending_likes.get(&urn) == Some(&(serial, liked)) {
                    match result {
                        Ok(()) => {
                            self.integration.pending_likes.remove(&urn);
                        }
                        Err(error) => self.integration.error = Some(error),
                    }
                }
            }
            MixEvent::Mutation {
                generation,
                serial,
                result,
            } if generation == self.integration.generation
                && serial == self.integration.mutation_serial =>
            {
                self.integration.mutation_pending = false;
                match result {
                    Ok(resource) => {
                        let pending = self.integration.pending_edit.take();
                        if let Some(playlist) = resource {
                            if self.integration.library.is_none() {
                                self.integration.library = Some(crate::soundcloud::Library {
                                    user: self.integration.user.clone().unwrap_or_default(),
                                    ..Default::default()
                                });
                            }
                            self.integration.playlist_projections.insert(
                                playlist.urn.clone(),
                                crate::integration::PlaylistProjection {
                                    serial,
                                    title: Some(playlist.title.clone()),
                                    created: Some(playlist),
                                    confirmed: true,
                                    page_seen: true,
                                    ..Default::default()
                                },
                            );
                        } else if matches!(pending, Some(MixAction::CreateRemote { .. })) {
                            // A create without an actual server identity cannot
                            // become a navigable local resource.
                            self.integration.pending_edit = pending;
                            self.integration.error = Some("SoundCloud did not return the new playlist. Refresh before trying again.".into());
                            return;
                        }
                        for projection in self
                            .integration
                            .playlist_projections
                            .values_mut()
                            .filter(|projection| projection.serial == serial)
                        {
                            projection.confirmed = true;
                        }
                        self.overlay_soundcloud_cache();
                        self.integration.notice = Some("SoundCloud playlist saved".into());
                        self.refresh_soundcloud_library();
                        let urn = self
                            .integration
                            .playlist_projections
                            .iter()
                            .find(|(_, projection)| {
                                projection.serial == serial && !projection.page_seen
                            })
                            .map(|(urn, _)| urn.clone());
                        if let Some(urn) = urn {
                            self.load_soundcloud_playlist(urn);
                        }
                        self.schedule_soundcloud_recheck();
                    }
                    Err(error) => {
                        for projection in self
                            .integration
                            .playlist_projections
                            .values_mut()
                            .filter(|projection| projection.serial == serial)
                        {
                            projection.failed = true;
                        }
                        self.integration.recheck_at = None;
                        self.integration.error = Some(format!(
                            "{error} Your edit stays visible. Retry the edit, or Refresh to load SoundCloud's version."
                        ));
                    }
                }
            }
            MixEvent::Resolved {
                generation,
                serial,
                result,
            } if generation == self.integration.generation
                && serial == self.integration.resolve_serial =>
            {
                self.integration.resolving = false;
                self.integration.resolving_play = false;
                let target = self.integration.resolve_target.take();
                match result {
                    Ok(crate::soundcloud::Resolved::Track(track)) => {
                        self.integration.resolved_items = vec![MediaItem::from(track)];
                        if let Some(id) = target {
                            let refs = self
                                .integration
                                .resolved_items
                                .iter()
                                .map(MediaItem::media_ref)
                                .collect();
                            self.apply_integration(MixAction::AddLocal { id, items: refs });
                        }
                    }
                    Ok(crate::soundcloud::Resolved::Playlist(playlist)) => {
                        if let Some(id) = target {
                            let refs = playlist
                                .tracks
                                .iter()
                                .map(|track| MediaRef::SoundCloud(track.urn.clone()))
                                .collect();
                            self.apply_integration(MixAction::AddLocal { id, items: refs });
                        } else {
                            self.apply_integration(MixAction::OpenPlaylist(playlist.urn));
                        }
                    }
                    Err(error) => self.integration.error = Some(error),
                }
            }
            MixEvent::Items {
                generation,
                serial,
                play,
                result,
            } if generation == self.integration.generation
                && serial == self.integration.resolve_serial
                && (!play
                    || (self.integration.resolving_play
                        && self.integration.resolve_play_intent
                            == self.integration.play_intent)) =>
            {
                self.integration.resolving = false;
                self.integration.resolving_play = false;
                let target = self.integration.resolve_target.take();
                match result {
                    Ok(items) => {
                        self.integration.resolved_items = items.clone();
                        let playable: Vec<_> =
                            items.into_iter().filter(MediaItem::playable).collect();
                        if play && !playable.is_empty() {
                            let name = match target {
                                Some(id) => self
                                    .integration
                                    .local
                                    .playlists
                                    .iter()
                                    .find(|p| p.id == id)
                                    .map(|p| p.name.clone())
                                    .unwrap_or_else(|| "Local playlist".into()),
                                _ => "Local mix".into(),
                            };
                            self.apply_integration(MixAction::Command(MixCommand::Start {
                                items: playable,
                                index: 0,
                                context: Some(MixContext { name, id: None }),
                            }));
                        } else if play {
                            self.integration.error = Some("This local playlist has no songs currently available for playback.".into());
                        }
                    }
                    Err(error) => self.integration.error = Some(error),
                }
            }
            _ => {}
        }
    }

    fn persist_local_playlists(&mut self) {
        if !self.integration.local_ready {
            return;
        }
        self.integration.local_revision = self.integration.local_revision.wrapping_add(1);
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.integration.local) {
            self.files.write(
                &self.dirs.state.join("mixed-playlists.json"),
                bytes,
                fastframe_file::Access::Default,
            );
        }
    }

    pub(crate) fn persist_mix_session(&self) {
        if !self.integration.session_writable {
            return;
        }
        if self.integration.snapshot.revision == 0
            && self.integration.snapshot.current.is_none()
            && self.integration.snapshot.manual.is_empty()
            && self.integration.snapshot.context.is_empty()
        {
            return;
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&self.integration.snapshot.session()) {
            self.files.write(
                &self.dirs.state.join("mixed-session.json"),
                bytes,
                fastframe_file::Access::Default,
            );
        }
    }

    pub(crate) fn mix_now_playing(&self) -> Option<NowPlaying> {
        let snapshot = &self.integration.snapshot;
        let entry = snapshot.current.as_ref()?;
        let item = &entry.item;
        let uri = match item.media_ref() {
            MediaRef::Spotify(uri) | MediaRef::SoundCloud(uri) => uri,
        };
        Some(NowPlaying {
            local: true,
            device_name: Some("Local mix".into()),
            device_kind: Some("Computer".into()),
            uri,
            id: None,
            title: item.title().into(),
            artists: Vec::new(),
            subtitle: format!("{} · {}", item.source(), item.creator()),
            album_name: item.source().into(),
            album_id: None,
            show_id: None,
            art_url: item.artwork().map(str::to_string),
            art_small: item.artwork().map(str::to_string),
            duration_ms: item.duration_ms(),
            position_ms: snapshot.position_now(),
            playing: matches!(snapshot.playback, Playback::Playing | Playback::Loading),
            loading: snapshot.playback == Playback::Loading,
            shuffle: snapshot.shuffle,
            repeat: snapshot.repeat,
            volume_percent: volume_to_percent(snapshot.volume),
            can_control: true,
            can_set_volume: true,
            is_episode: false,
            resuming: false,
        })
    }

    pub(super) fn route_mix_transport(&mut self, action: &crate::model::Action) -> bool {
        if !self.integration.local_active {
            return false;
        }
        let volume = volume_to_percent(self.integration.snapshot.volume);
        let command = match action {
            crate::model::Action::TogglePlay => MixCommand::Toggle,
            crate::model::Action::Next => MixCommand::Next,
            crate::model::Action::Previous => MixCommand::Previous,
            crate::model::Action::Seek(ms) => MixCommand::Seek(*ms),
            crate::model::Action::SeekBy(delta) => MixCommand::Seek(
                (i64::from(self.integration.snapshot.position_now()) + delta).clamp(
                    0,
                    i64::from(self.mix_now_playing().map_or(0, |now| now.duration_ms)),
                ) as u32,
            ),
            crate::model::Action::SetVolume(percent)
            | crate::model::Action::PreviewVolume(percent) => {
                MixCommand::Volume(percent_to_volume(*percent))
            }
            crate::model::Action::VolumeBy(delta) => MixCommand::Volume(percent_to_volume(
                (i16::from(volume) + i16::from(*delta)).clamp(0, 100) as u8,
            )),
            crate::model::Action::ToggleMute => {
                let next = if volume == 0 {
                    self.volume_before_mute.take().unwrap_or(50)
                } else {
                    self.volume_before_mute = Some(volume);
                    0
                };
                MixCommand::Volume(percent_to_volume(next))
            }
            crate::model::Action::ToggleShuffle => {
                MixCommand::Shuffle(!self.integration.snapshot.shuffle)
            }
            crate::model::Action::SetShuffle(enabled) => MixCommand::Shuffle(*enabled),
            crate::model::Action::CycleRepeat => {
                MixCommand::Repeat(match self.integration.snapshot.repeat {
                    RepeatMode::Off => RepeatMode::Context,
                    RepeatMode::Context => RepeatMode::Track,
                    RepeatMode::Track => RepeatMode::Off,
                })
            }
            crate::model::Action::SetRepeat(mode) => MixCommand::Repeat(*mode),
            _ => return false,
        };
        self.send_mix(command);
        true
    }

    fn mark_soundcloud_art(&self, event: &MixEvent) {
        let mark_track = |track: &crate::soundcloud::Track| {
            if let Some(url) = &track.artwork_url {
                self.backend.art().mark_session_only(url);
            }
            if let Some(url) = &track.user.avatar_url {
                self.backend.art().mark_session_only(url);
            }
        };
        let mark_playlist = |playlist: &crate::soundcloud::Playlist| {
            if let Some(url) = &playlist.artwork_url {
                self.backend.art().mark_session_only(url);
            }
        };
        match event {
            MixEvent::Search {
                tracks, playlists, ..
            } => {
                if let Ok(page) = tracks {
                    for track in &page.collection {
                        mark_track(track);
                    }
                }
                if let Ok(page) = playlists {
                    for playlist in &page.collection {
                        mark_playlist(playlist);
                    }
                }
            }
            MixEvent::Library {
                result: Ok(library),
                ..
            } => {
                for track in &library.liked_tracks {
                    mark_track(track);
                }
                for playlist in library.playlists.iter().chain(&library.liked_playlists) {
                    mark_playlist(playlist);
                }
            }
            MixEvent::Playlist {
                result: Ok((playlist, tracks)),
                ..
            } => {
                mark_playlist(playlist);
                for track in tracks {
                    mark_track(track);
                }
            }
            MixEvent::Items {
                result: Ok(items), ..
            } => {
                for item in items {
                    if let MediaItem::SoundCloud(track) = item {
                        mark_track(track);
                    }
                }
            }
            MixEvent::Resolved {
                result: Ok(crate::soundcloud::Resolved::Track(track)),
                ..
            } => mark_track(track),
            MixEvent::Mix(crate::mix::Event::State(snapshot)) => {
                for entry in snapshot
                    .current
                    .iter()
                    .chain(&snapshot.manual)
                    .chain(&snapshot.context)
                {
                    if let MediaItem::SoundCloud(track) = &entry.item {
                        mark_track(track);
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_local_additions_keep_occurrences_and_reject_the_whole_invalid_selection() {
        let mut app = app("bulk-local-additions");
        let spotify = MediaRef::Spotify("spotify:track:1234567890abcdefghijkl".into());
        let soundcloud = MediaRef::SoundCloud("soundcloud:tracks:99".into());
        let items = vec![spotify.clone(), soundcloud, spotify];
        app.apply_integration(MixAction::CreateLocalWithItems {
            name: "Imported".into(),
            items: items.clone(),
        });
        let id = app.integration.local.playlists[0].id;
        assert_eq!(app.integration.local.playlists[0].items, items);
        assert_eq!(app.integration.view, View::LocalPlaylist(id));
        let revision = app.integration.local_revision;
        app.apply_integration(MixAction::AddLocal {
            id,
            items: vec![
                items[0].clone(),
                MediaRef::Spotify("spotify:local:unsupported".into()),
            ],
        });
        assert_eq!(app.integration.local.playlists[0].items, items);
        assert_eq!(app.integration.local_revision, revision);
        assert!(
            app.integration
                .error
                .as_deref()
                .unwrap()
                .contains("Nothing was added")
        );
        app.apply_integration(MixAction::CreateLocalWithItems {
            name: "Invalid".into(),
            items: vec![MediaRef::SoundCloud("https://invalid.example/song".into())],
        });
        assert_eq!(app.integration.local.playlists.len(), 1);
    }

    #[test]
    fn bulk_playlist_import_ignores_stale_canceled_and_deleted_destinations() {
        let mut app = app("bulk-import-fences");
        app.auth = AuthStatus::Connected {
            username: "alice".into(),
        };
        app.user = Some(crate::api::models::User {
            id: "alice".into(),
            ..Default::default()
        });
        app.apply_integration(MixAction::CreateLocal("Destination".into()));
        let id = app.integration.local.playlists[0].id;
        let items = vec![MediaRef::Spotify("spotify:track:1234567890abcdefghijkl".into()); 2];
        let pending = |serial| crate::integration::SpotifyImport {
            serial,
            name: "Source".into(),
            target: Some(id),
            account: "alice".into(),
        };
        let event = |serial| MixEvent::SpotifyPlaylistImported {
            serial,
            account: "alice".into(),
            result: Ok(items.clone()),
        };
        app.integration.spotify_import = Some(pending(20));
        app.handle_integration_event(event(19));
        assert!(app.integration.local.playlists[0].items.is_empty());
        assert_eq!(app.integration.spotify_import.as_ref().unwrap().serial, 20);
        app.handle_integration_event(event(20));
        assert_eq!(app.integration.local.playlists[0].items, items);
        let revision = app.integration.local_revision;
        app.handle_integration_event(event(20));
        assert_eq!(app.integration.local_revision, revision);
        app.integration.spotify_import = Some(pending(21));
        app.cancel_spotify_import();
        app.handle_integration_event(event(21));
        assert_eq!(app.integration.local_revision, revision);
        app.integration.spotify_import = Some(pending(22));
        app.user.as_mut().unwrap().id = "bob".into();
        app.handle_integration_event(event(22));
        assert_eq!(app.integration.local_revision, revision);
        app.user.as_mut().unwrap().id = "alice".into();
        app.integration.spotify_import = Some(pending(23));
        app.integration.local.playlists.clear();
        app.handle_integration_event(event(23));
        assert!(app.integration.local.playlists.is_empty());
        assert_eq!(app.integration.local_revision, revision);
        assert!(
            app.integration
                .error
                .as_deref()
                .unwrap()
                .contains("deleted")
        );
    }

    fn app(name: &str) -> App {
        let mut app = super::super::tests::test_app(name);
        app.offline = true;
        app.backend.set_offline(true);
        app.integration.local_ready = true;
        app
    }

    fn soundcloud(urn: &str) -> MediaItem {
        MediaItem::from(crate::soundcloud::Track {
            urn: urn.into(),
            title: "Native SoundCloud song".into(),
            streamable: true,
            access: crate::soundcloud::Access::Playable,
            duration: 60_000,
            ..Default::default()
        })
    }

    #[test]
    fn soundcloud_next_is_optimistic_and_never_queues_a_spotify_uri() {
        let mut app = app("mix-next-routing");
        app.apply_integration(MixAction::Command(MixCommand::Start {
            items: vec![soundcloud("soundcloud:tracks:1")],
            index: 0,
            context: None,
        }));
        app.apply_integration(MixAction::Command(MixCommand::Add(vec![
            soundcloud("soundcloud:tracks:2"),
            soundcloud("soundcloud:tracks:2"),
        ])));
        let ids: Vec<_> = app
            .integration
            .snapshot
            .manual
            .iter()
            .map(|entry| entry.id)
            .collect();
        app.apply(Action::Next, &egui::Context::default());
        assert_eq!(
            app.integration.snapshot.current.as_ref().unwrap().id,
            ids[0]
        );
        assert_eq!(app.integration.snapshot.manual[0].id, ids[1]);
        assert!(app.now_playing_item().is_none());
        assert!(app.backend.take_queued_tracks().is_empty());
        assert!(app.backend.take_player_commands().is_empty());
        assert!(app.backend.take_remote_play_requests().is_empty());
        app.request_contains(vec!["soundcloud:tracks:2".into()]);
        assert!(!app.saved_pending.contains("soundcloud:tracks:2"));
        assert!(app.queue_playlist_uris().is_empty());
        app.apply(Action::SaveQueueAsPlaylist, &egui::Context::default());
        assert!(app.dialog.is_none());
        assert!(
            app.actions
                .iter()
                .all(|action| !matches!(action, Action::CreatePlaylist { .. }))
        );
        assert_eq!(app.integration.local.playlists[0].items.len(), 2);
    }

    #[test]
    fn stale_mix_snapshots_cannot_undo_next() {
        let mut app = app("mix-stale-answer");
        app.apply_integration(MixAction::Command(MixCommand::Start {
            items: vec![
                soundcloud("soundcloud:tracks:1"),
                soundcloud("soundcloud:tracks:2"),
            ],
            index: 0,
            context: None,
        }));
        let stale = app.integration.snapshot.clone();
        app.apply(Action::Next, &egui::Context::default());
        app.handle_integration_event(MixEvent::Mix(crate::mix::Event::State(Box::new(stale))));
        assert_eq!(app.now_playing().unwrap().uri, "soundcloud:tracks:2");
    }

    #[test]
    fn adding_to_local_mix_does_not_switch_spotify_playback() {
        let mut app = app("mix-staged-queue");
        app.apply_integration(MixAction::Command(MixCommand::Add(vec![soundcloud(
            "soundcloud:tracks:1",
        )])));
        assert!(!app.integration.local_active);
        assert_eq!(app.integration.snapshot.manual.len(), 1);
        assert!(app.backend.take_player_commands().is_empty());
        assert!(app.backend.take_remote_play_requests().is_empty());
    }

    #[test]
    fn local_playlist_edits_preserve_duplicate_occurrences() {
        let mut app = app("mixed-playlist-occurrences");
        app.apply_integration(MixAction::CreateLocal("A local mix".into()));
        let id = app.integration.local.playlists[0].id;
        let reference = MediaRef::SoundCloud("soundcloud:tracks:42".into());
        app.apply_integration(MixAction::AddLocal {
            id,
            items: vec![reference.clone(), reference.clone()],
        });
        app.apply_integration(MixAction::RemoveLocal { id, index: 0 });
        assert_eq!(app.integration.local.playlists[0].items, vec![reference]);
        assert!(app.flush_files());
        let bytes = std::fs::read(app.dirs.state.join("mixed-playlists.json")).unwrap();
        let restored = crate::integration::LocalPlaylists::decode(&bytes).unwrap();
        assert_eq!(restored.playlists[0].items.len(), 1);
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains("Native SoundCloud song")
        );
    }

    #[test]
    fn late_saved_mix_cannot_replace_a_user_started_song() {
        let mut app = app("mix-restore-race");
        app.apply_integration(MixAction::Command(MixCommand::Start {
            items: vec![soundcloud("soundcloud:tracks:42")],
            index: 0,
            context: None,
        }));
        let serial = app.integration.mix_serial;
        app.handle_integration_event(MixEvent::LocalLoaded {
            result: Ok(crate::integration::LocalPlaylists::default()),
            session: Some(crate::mix::MixSession::default()),
        });
        assert_eq!(app.integration.mix_serial, serial);
        assert_eq!(app.now_playing().unwrap().uri, "soundcloud:tracks:42");
    }

    #[test]
    fn stale_auth_and_search_do_not_restore_disconnected_account_data() {
        let mut app = app("soundcloud-account-generation");
        app.integration.generation = 2;
        app.integration.search_serial = 3;
        app.handle_integration_event(MixEvent::Auth {
            generation: 1,
            result: Ok(Some(crate::soundcloud::User {
                urn: "soundcloud:users:1".into(),
                ..Default::default()
            })),
        });
        assert!(app.integration.user.is_none());
        app.handle_integration_event(MixEvent::Search {
            generation: 1,
            serial: 3,
            tracks: Ok(crate::soundcloud::Page {
                collection: vec![crate::soundcloud::Track::default()],
                next_href: None,
            }),
            playlists: Ok(crate::soundcloud::Page::default()),
        });
        assert!(app.integration.tracks.is_empty());
    }

    fn remote_playlist(
        name: &str,
        ids: &[&str],
    ) -> (crate::soundcloud::Playlist, Vec<crate::soundcloud::Track>) {
        let tracks: Vec<_> = ids
            .iter()
            .map(|id| crate::soundcloud::Track {
                urn: (*id).into(),
                title: (*id).into(),
                streamable: true,
                access: crate::soundcloud::Access::Playable,
                ..Default::default()
            })
            .collect();
        (
            crate::soundcloud::Playlist {
                urn: "soundcloud:playlists:1".into(),
                title: name.into(),
                track_count: tracks.len(),
                tracks: tracks.clone(),
                ..Default::default()
            },
            tracks,
        )
    }

    fn remote_app(name: &str) -> App {
        let mut app = app(name);
        app.integration.user = Some(crate::soundcloud::User {
            urn: "soundcloud:users:1".into(),
            ..Default::default()
        });
        let (playlist, tracks) =
            remote_playlist("Before", &["soundcloud:tracks:a", "soundcloud:tracks:b"]);
        app.integration.library = Some(crate::soundcloud::Library {
            playlists: vec![playlist.clone()],
            ..Default::default()
        });
        app.integration.view = View::Playlist(playlist.urn.clone());
        app.integration.playlist = Some((playlist, tracks));
        app
    }

    #[test]
    fn a_corrupt_saved_session_survives_pause_volume_and_disconnect_until_a_new_queue_is_chosen() {
        let mut app = app("mixed-session-preservation");
        std::fs::create_dir_all(&app.dirs.state).unwrap();
        let path = app.dirs.state.join("mixed-session.json");
        let original = b"{future-version-or-incomplete-data";
        std::fs::write(&path, original).unwrap();
        app.handle_integration_event(MixEvent::SessionPersistence {
            writable: false,
            message: Some("Original saved session kept".into()),
        });
        for command in [
            MixCommand::Pause,
            MixCommand::Volume(100),
            MixCommand::ForgetSoundCloud,
        ] {
            app.apply_integration(MixAction::Command(command));
        }
        app.persist_mix_session();
        assert!(app.flush_files());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        app.apply_integration(MixAction::Command(MixCommand::Clear));
        app.handle_integration_event(MixEvent::SessionPersistence {
            writable: false,
            message: None,
        });
        assert!(app.integration.session_writable);
        assert!(app.flush_files());
        assert!(crate::mix::MixSession::decode(&std::fs::read_to_string(path).unwrap()).is_ok());
    }

    #[test]
    fn stale_library_and_playlist_answers_do_not_undo_a_rename() {
        let mut app = remote_app("soundcloud-rename-projection");
        let urn = "soundcloud:playlists:1".to_string();
        app.apply_integration(MixAction::RenameRemote {
            urn: urn.clone(),
            name: "After".into(),
        });
        assert_eq!(
            app.integration.library.as_ref().unwrap().playlists[0].title,
            "After"
        );
        let mutation = app.integration.mutation_serial;
        app.handle_integration_event(MixEvent::Mutation {
            generation: 0,
            serial: mutation,
            result: Ok(None),
        });
        let library_serial = app.integration.library_serial;
        let playlist_serial = app.integration.playlist_serial;
        let (before, tracks) =
            remote_playlist("Before", &["soundcloud:tracks:a", "soundcloud:tracks:b"]);
        app.handle_integration_event(MixEvent::Library {
            generation: 0,
            serial: library_serial,
            result: Ok(crate::soundcloud::Library {
                playlists: vec![before.clone()],
                ..Default::default()
            }),
        });
        app.handle_integration_event(MixEvent::Playlist {
            generation: 0,
            serial: playlist_serial,
            urn: urn.clone(),
            result: Ok((before, tracks.clone())),
        });
        assert_eq!(
            app.integration.library.as_ref().unwrap().playlists[0].title,
            "After"
        );
        assert_eq!(app.integration.playlist.as_ref().unwrap().0.title, "After");
        assert!(app.integration.recheck_at.is_some());
        let mut after = remote_playlist("After", &["soundcloud:tracks:a", "soundcloud:tracks:b"]).0;
        after.tracks = tracks.clone();
        app.handle_integration_event(MixEvent::Library {
            generation: 0,
            serial: library_serial,
            result: Ok(crate::soundcloud::Library {
                playlists: vec![after.clone()],
                ..Default::default()
            }),
        });
        app.handle_integration_event(MixEvent::Playlist {
            generation: 0,
            serial: playlist_serial,
            urn,
            result: Ok((after, tracks)),
        });
        assert!(app.integration.playlist_projections.is_empty());
    }

    #[test]
    fn appended_duplicates_and_deleted_playlists_stay_projected_over_old_reads() {
        let mut app = remote_app("soundcloud-append-projection");
        let urn = "soundcloud:playlists:1".to_string();
        app.apply_integration(MixAction::AppendRemote {
            urn: urn.clone(),
            items: vec!["soundcloud:tracks:a".into()],
        });
        assert_eq!(app.integration.playlist.as_ref().unwrap().1.len(), 3);
        let mutation = app.integration.mutation_serial;
        app.handle_integration_event(MixEvent::Mutation {
            generation: 0,
            serial: mutation,
            result: Ok(None),
        });
        let page_serial = app.integration.playlist_serial;
        app.handle_integration_event(MixEvent::Playlist {
            generation: 0,
            serial: page_serial,
            urn: urn.clone(),
            result: Ok(remote_playlist(
                "Before",
                &["soundcloud:tracks:a", "soundcloud:tracks:b"],
            )),
        });
        let tracks = &app.integration.playlist.as_ref().unwrap().1;
        assert_eq!(
            tracks
                .iter()
                .map(|track| track.urn.as_str())
                .collect::<Vec<_>>(),
            [
                "soundcloud:tracks:a",
                "soundcloud:tracks:b",
                "soundcloud:tracks:a"
            ]
        );
        app.apply_integration(MixAction::DeleteRemote(urn.clone()));
        let mutation = app.integration.mutation_serial;
        app.handle_integration_event(MixEvent::Mutation {
            generation: 0,
            serial: mutation,
            result: Ok(None),
        });
        app.handle_integration_event(MixEvent::Library {
            generation: 0,
            serial: app.integration.library_serial,
            result: Ok(crate::soundcloud::Library {
                playlists: vec![remote_playlist("Before", &[]).0],
                ..Default::default()
            }),
        });
        assert!(
            app.integration
                .library
                .as_ref()
                .unwrap()
                .playlists
                .is_empty()
        );
    }

    #[test]
    fn a_created_playlist_uses_the_real_resource_and_survives_a_lagging_library() {
        let mut app = remote_app("soundcloud-create-projection");
        app.apply_integration(MixAction::CreateRemote {
            name: "Created".into(),
            private: true,
        });
        assert!(matches!(
            app.integration.pending_edit,
            Some(MixAction::CreateRemote { .. })
        ));
        assert_eq!(app.integration.library.as_ref().unwrap().playlists.len(), 1);
        let mut created = remote_playlist("Created", &[]).0;
        created.urn = "soundcloud:playlists:actual-server-id".into();
        app.handle_integration_event(MixEvent::Mutation {
            generation: 0,
            serial: app.integration.mutation_serial,
            result: Ok(Some(created.clone())),
        });
        assert!(app.integration.pending_edit.is_none());
        app.handle_integration_event(MixEvent::Library {
            generation: 0,
            serial: app.integration.library_serial,
            result: Ok(crate::soundcloud::Library::default()),
        });
        assert_eq!(
            app.integration.library.as_ref().unwrap().playlists[0].urn,
            created.urn
        );
    }

    #[test]
    fn a_late_playlist_resolution_cannot_override_a_newer_transport_action() {
        let mut app = app("mix-resolver-intent");
        app.apply_integration(MixAction::ResolveItems {
            items: vec![MediaRef::SoundCloud("soundcloud:tracks:old".into())],
            play: true,
        });
        let serial = app.integration.resolve_serial;
        app.apply_integration(MixAction::Command(MixCommand::Start {
            items: vec![soundcloud("soundcloud:tracks:new")],
            index: 0,
            context: None,
        }));
        assert!(!app.integration.resolving);
        app.handle_integration_event(MixEvent::Items {
            generation: 0,
            serial,
            play: true,
            result: Ok(vec![soundcloud("soundcloud:tracks:old")]),
        });
        assert_eq!(
            app.integration
                .snapshot
                .current
                .as_ref()
                .unwrap()
                .item
                .media_ref(),
            MediaRef::SoundCloud("soundcloud:tracks:new".into())
        );
    }

    #[test]
    fn a_resolved_link_appends_to_its_original_local_playlist_after_navigation() {
        for create_while_pending in [false, true] {
            let mut app = app(&format!("mix-link-target-{create_while_pending}"));
            app.apply_integration(MixAction::CreateLocal("First".into()));
            let first = app.integration.local.playlists[0].id;
            if !create_while_pending {
                app.apply_integration(MixAction::CreateLocal("Second".into()));
            }
            app.integration.view = View::LocalPlaylist(first);
            app.apply_integration(MixAction::OpenLink("https://soundcloud.com/a/b".into()));
            let serial = app.integration.resolve_serial;
            if create_while_pending {
                app.apply_integration(MixAction::CreateLocal("Second".into()));
            }
            let second = app.integration.local.playlists[1].id;
            app.integration.view = View::LocalPlaylist(second);
            let track = match soundcloud("soundcloud:tracks:3") {
                MediaItem::SoundCloud(track) => *track,
                _ => unreachable!(),
            };
            app.handle_integration_event(MixEvent::Resolved {
                generation: 0,
                serial,
                result: Ok(crate::soundcloud::Resolved::Track(track)),
            });
            assert_eq!(app.integration.local.playlists[0].items.len(), 1);
            assert!(app.integration.local.playlists[1].items.is_empty());
            assert_eq!(app.integration.view, View::LocalPlaylist(second));
        }
    }

    #[test]
    fn deleting_a_local_playlist_invalidates_its_pending_play_and_link_resolution() {
        for play in [false, true] {
            let mut app = app("mix-deleted-resolution-target");
            app.apply_integration(MixAction::CreateLocal("Temporary".into()));
            let id = app.integration.local.playlists[0].id;
            if play {
                app.apply_integration(MixAction::PlayLocal(id));
            } else {
                app.apply_integration(MixAction::OpenLink("https://soundcloud.com/a/b".into()));
            }
            let serial = app.integration.resolve_serial;
            app.apply_integration(MixAction::DeleteLocal(id));
            assert!(!app.integration.resolving);
            assert_ne!(app.integration.resolve_serial, serial);
            app.handle_integration_event(MixEvent::Items {
                generation: 0,
                serial,
                play,
                result: Ok(vec![soundcloud("soundcloud:tracks:1")]),
            });
            assert!(app.integration.snapshot.current.is_none());
            assert!(app.integration.local.playlists.is_empty());
        }
    }

    #[test]
    fn adding_to_a_deleted_local_playlist_reports_the_missing_target() {
        let mut app = app("mix-deleted-append-target");
        app.apply_integration(MixAction::AddLocal {
            id: 7,
            items: vec![MediaRef::SoundCloud("soundcloud:tracks:1".into())],
        });
        assert!(app.integration.local.playlists.is_empty());
        assert!(app.integration.notice.is_none());
        assert!(
            app.integration
                .error
                .as_deref()
                .unwrap()
                .contains("deleted")
        );
    }

    #[test]
    fn playing_a_local_playlist_filters_unavailable_songs_without_editing_saved_rows() {
        let mut app = app("mix-local-playable-filter");
        app.apply_integration(MixAction::CreateLocal("First".into()));
        let id = app.integration.local.playlists[0].id;
        app.apply_integration(MixAction::AddLocal {
            id,
            items: vec![
                MediaRef::SoundCloud("soundcloud:tracks:1".into()),
                MediaRef::SoundCloud("soundcloud:tracks:2".into()),
            ],
        });
        app.apply_integration(MixAction::PlayLocal(id));
        let mut blocked = soundcloud("soundcloud:tracks:1");
        if let MediaItem::SoundCloud(track) = &mut blocked {
            track.access = crate::soundcloud::Access::Blocked;
        }
        app.handle_integration_event(MixEvent::Items {
            generation: 0,
            serial: app.integration.resolve_serial,
            play: true,
            result: Ok(vec![blocked, soundcloud("soundcloud:tracks:2")]),
        });
        assert_eq!(app.integration.resolved_items.len(), 2);
        assert_eq!(app.integration.local.playlists[0].items.len(), 2);
        assert_eq!(
            app.integration
                .snapshot
                .current
                .as_ref()
                .unwrap()
                .item
                .media_ref(),
            MediaRef::SoundCloud("soundcloud:tracks:2".into())
        );
    }

    #[test]
    fn cancelling_sign_in_invalidates_pending_resolution_and_clears_its_spinner() {
        let mut app = app("mix-resolution-cancel");
        app.apply_integration(MixAction::ResolveItems {
            items: vec![MediaRef::SoundCloud("soundcloud:tracks:old".into())],
            play: true,
        });
        let serial = app.integration.resolve_serial;
        app.apply_integration(MixAction::CancelSignIn);
        assert!(!app.integration.resolving);
        assert!(!app.integration.resolving_play);
        app.handle_integration_event(MixEvent::Items {
            generation: 0,
            serial,
            play: true,
            result: Ok(vec![soundcloud("soundcloud:tracks:old")]),
        });
        assert!(app.integration.snapshot.current.is_none());
    }

    #[test]
    fn stale_playlist_rechecks_stop_with_an_actionable_error_and_keep_the_edit() {
        let mut app = remote_app("soundcloud-bounded-recheck");
        let urn = "soundcloud:playlists:1".to_string();
        app.apply_integration(MixAction::RenameRemote {
            urn: urn.clone(),
            name: "After".into(),
        });
        app.handle_integration_event(MixEvent::Mutation {
            generation: 0,
            serial: app.integration.mutation_serial,
            result: Ok(None),
        });
        app.integration.recheck_count = 3;
        app.handle_integration_event(MixEvent::Playlist {
            generation: 0,
            serial: app.integration.playlist_serial,
            urn,
            result: Ok(remote_playlist(
                "Before",
                &["soundcloud:tracks:a", "soundcloud:tracks:b"],
            )),
        });
        assert!(app.integration.recheck_at.is_none());
        assert!(app.integration.error.as_ref().unwrap().contains("Refresh"));
        assert_eq!(app.integration.playlist.as_ref().unwrap().0.title, "After");
    }
}
