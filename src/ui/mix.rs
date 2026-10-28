//! SoundCloud and Local mix views using the existing native shell.

use crate::{
    app::App,
    integration::{Action as MixAction, Source, View},
    mix::{Command, MediaItem, MediaRef},
    model::{Action, Page},
    theme,
};
use egui::{Color32, RichText};

mod local_playlist;
use std::collections::BTreeSet;

fn emit(app: &mut App, action: MixAction) {
    app.actions.push(Action::Integration(action));
}
fn heading(app: &App, ui: &mut egui::Ui, text: &str) {
    theme::text(ui, text, theme::bold(26.0), app.palette.text);
    ui.add_space(12.0);
}
fn message(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(Color32::from_rgb(220, 110, 65)));
}

pub fn add_to_local_playlist_menu(app: &mut App, ui: &mut egui::Ui, items: &[MediaRef]) {
    let local = app.integration.local.playlists.clone();
    let palette = app.palette;
    ui.add_enabled_ui(app.integration.local_ready && !items.is_empty(), |ui| {
        super::widgets::menu_submenu(ui, &palette, None, "Add to local playlist", |ui| {
            if super::widgets::menu_item(ui, &palette, None, "New local playlist") {
                emit(
                    app,
                    MixAction::CreateLocalWithItems {
                        name: "Local playlist".into(),
                        items: items.to_vec(),
                    },
                );
            }
            for playlist in local {
                if super::widgets::menu_item(ui, &palette, None, &playlist.name) {
                    emit(
                        app,
                        MixAction::AddLocal {
                            id: playlist.id,
                            items: items.to_vec(),
                        },
                    );
                }
            }
        });
    });
}

pub fn spotify_playlist_merge_menu(app: &mut App, ui: &mut egui::Ui, uri: &str, name: &str) {
    let local = app.integration.local.playlists.clone();
    let palette = app.palette;
    ui.add_enabled_ui(
        app.integration.local_ready && app.integration.spotify_import.is_none(),
        |ui| {
            super::widgets::menu_submenu(ui, &palette, None, "Merge into local playlist", |ui| {
                if super::widgets::menu_item(ui, &palette, None, "New local playlist") {
                    emit(
                        app,
                        MixAction::ImportSpotifyPlaylist {
                            uri: uri.into(),
                            name: name.into(),
                            target: None,
                        },
                    );
                }
                for playlist in local {
                    if super::widgets::menu_item(ui, &palette, None, &playlist.name) {
                        emit(
                            app,
                            MixAction::ImportSpotifyPlaylist {
                                uri: uri.into(),
                                name: name.into(),
                                target: Some(playlist.id),
                            },
                        );
                    }
                }
            });
        },
    );
}

struct BulkSelection {
    id: egui::Id,
    rows: BTreeSet<usize>,
}

impl BulkSelection {
    fn load(ui: &egui::Ui, scope: &str, generation: u64, items: &[MediaRef]) -> Self {
        let id = ui.make_persistent_id(("mixed-song-selection", scope, generation, items));
        let rows = ui
            .ctx()
            .data_mut(|data| data.get_temp::<BTreeSet<usize>>(id).unwrap_or_default());
        Self { id, rows }
    }

    fn items(&self, items: &[MediaRef]) -> Vec<MediaRef> {
        self.rows
            .iter()
            .filter_map(|index| items.get(*index).cloned())
            .collect()
    }

    fn set(&mut self, index: usize, selected: bool) {
        if selected {
            self.rows.insert(index);
        } else {
            self.rows.remove(&index);
        }
    }

    fn save(self, ui: &egui::Ui) {
        ui.ctx()
            .data_mut(|data| data.insert_temp(self.id, self.rows));
    }
}

fn bulk_selection_controls(
    app: &mut App,
    ui: &mut egui::Ui,
    scope: &str,
    items: &[MediaRef],
) -> BulkSelection {
    let mut selection = BulkSelection::load(ui, scope, app.integration.generation, items);
    if !items.is_empty() {
        ui.horizontal_wrapped(|ui| {
            let mut all = selection.rows.len() == items.len();
            if ui.checkbox(&mut all, "Select all").changed() {
                selection.rows = if all {
                    (0..items.len()).collect()
                } else {
                    BTreeSet::new()
                };
            }
            ui.label(format!("{} selected", selection.rows.len()));
            add_to_local_playlist_menu(app, ui, &selection.items(items));
        });
    }
    selection
}

pub fn soundcloud_badge(app: &mut App, ui: &mut egui::Ui, link: &str) {
    let response = egui::Frame::new()
        .fill(Color32::from_rgb(255, 85, 0))
        .inner_margin(egui::Margin::symmetric(5, 4))
        .show(ui, |ui| {
            ui.add(
                egui::Image::new(egui::include_image!(
                    "../../assets/soundcloud-logo-black.png"
                ))
                .fit_to_exact_size(egui::vec2(80.0, 10.0))
                .sense(egui::Sense::click()),
            )
        })
        .inner
        .on_hover_text("Open on SoundCloud");
    if response.clicked() {
        app.actions
            .push(Action::OpenUrl(if link.starts_with("https://") {
                link.to_string()
            } else {
                "https://soundcloud.com".into()
            }));
    }
}

pub fn source_filters(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        for (source, label) in [
            (Source::All, "All"),
            (Source::Spotify, "Spotify"),
            (Source::SoundCloud, "SoundCloud"),
        ] {
            if ui
                .selectable_label(app.integration.source == source, label)
                .clicked()
            {
                emit(app, MixAction::Source(source));
            }
        }
        if app.integration.user.is_none() && ui.button("Connect SoundCloud").clicked() {
            emit(app, MixAction::Open(View::Accounts));
        }
    });
    ui.add_space(10.0);
}

pub fn search(app: &mut App, ui: &mut egui::Ui) {
    heading(app, ui, "SoundCloud");
    if app.integration.user.is_none() {
        ui.label("Connect SoundCloud to search its songs and playlists.");
        if ui.button("Connect SoundCloud").clicked() {
            emit(app, MixAction::Open(View::Accounts));
        }
        return;
    }
    if app.integration.searching {
        ui.spinner();
    }
    if let Some(error) = app.integration.track_error.clone() {
        message(ui, &error);
    }
    if let Some(error) = app.integration.playlist_search_error.clone() {
        message(ui, &error);
    }
    let tracks = app.integration.tracks.clone();
    if !app.integration.search_query.is_empty()
        && !app.integration.searching
        && tracks.is_empty()
        && app.integration.track_error.is_none()
    {
        ui.label("No SoundCloud songs found.");
    }
    track_list(app, ui, &tracks, "soundcloud-search");
    let playlists = app.integration.search_playlists.clone();
    if !playlists.is_empty() {
        ui.add_space(14.0);
        ui.heading("SoundCloud playlists");
    }
    for playlist in playlists {
        playlist_link(app, ui, &playlist);
    }
}

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    if !matches!(&app.integration.view, View::LocalPlaylist(_)) {
        ui.horizontal_wrapped(|ui| {
            for (view, title) in [
                (View::Library, "Library"),
                (View::Queue, "Local mix"),
                (View::Accounts, "Accounts"),
            ] {
                if ui
                    .selectable_label(app.integration.view == view, title)
                    .clicked()
                {
                    emit(app, MixAction::Open(view));
                }
            }
        });
        ui.add_space(12.0);
    }
    if let Some(error) = app.integration.error.clone() {
        message(ui, &error);
        if app.integration.pending_edit.is_some() {
            ui.label("Your pending playlist edit has been kept. Reload the playlist and review its current songs before applying it again.");
            if ui.button("Reload SoundCloud library").clicked() {
                emit(app, MixAction::Refresh);
            }
        }
    }
    if let Some(notice) = app.integration.notice.clone() {
        ui.horizontal_wrapped(|ui| {
            ui.label(notice);
            if !app.integration.local_active
                && !app.integration.snapshot.manual.is_empty()
                && ui.button("Play local mix").clicked()
            {
                emit(app, MixAction::Command(Command::Play));
            }
        });
    }
    if let Some(name) = app
        .integration
        .spotify_import
        .as_ref()
        .map(|import| import.name.clone())
    {
        ui.horizontal_wrapped(|ui| {
            ui.spinner();
            theme::text(
                ui,
                format!("Importing {name} from Spotify..."),
                theme::regular(13.0),
                app.palette.secondary,
            );
            if ui.button("Cancel import").clicked() {
                emit(app, MixAction::CancelSpotifyImport);
            }
        });
    }
    match app.integration.view.clone() {
        View::Accounts => accounts(app, ui),
        View::Library => library(app, ui),
        View::Playlist(urn) => soundcloud_playlist(app, ui, &urn),
        View::LocalPlaylist(id) => local_playlist::show(app, ui, id),
        View::Queue => queue(app, ui),
    }
}

pub fn accounts(app: &mut App, ui: &mut egui::Ui) {
    heading(app, ui, "Accounts");
    ui.heading("Spotify");
    if let Some(user) = &app.user {
        theme::text(
            ui,
            format!(
                "Connected as {}",
                user.display_name.as_deref().unwrap_or(&user.id)
            ),
            theme::regular(13.0),
            app.palette.text,
        );
        if ui.button("Spotify account settings").clicked() {
            app.actions.push(Action::Open(Page::Settings));
        }
    } else {
        ui.label("Your existing Spotify setup remains available.");
        if ui.button("Connect Spotify").clicked() {
            app.actions.push(Action::Open(Page::Settings));
        }
    }
    ui.add_space(20.0);
    ui.heading("SoundCloud");
    if let Some(user) = &app.integration.user {
        theme::text(
            ui,
            format!("Connected as {}", user.username),
            theme::regular(13.0),
            app.palette.text,
        );
        ui.menu_button("Disconnect SoundCloud", |ui| {
            ui.label("Remove this app's access and stored SoundCloud credentials?");
            if ui.button("Disconnect").clicked() {
                emit(app, MixAction::Disconnect);
                ui.close();
            }
        });
    } else {
        ui.label("Register your own SoundCloud application, then enter its credentials here.");
        ui.hyperlink_to(
            "SoundCloud app registration",
            "https://developers.soundcloud.com/docs/api/register-app",
        );
        ui.label("Register this exact redirect URL:");
        ui.label(RichText::new(crate::soundcloud::REDIRECT_URI).monospace());
        ui.add_space(8.0);
        ui.label("Client ID");
        ui.add(egui::TextEdit::singleline(&mut app.integration.client_id).desired_width(420.0));
        ui.label("Client secret");
        ui.add(
            egui::TextEdit::singleline(&mut app.integration.client_secret)
                .password(true)
                .desired_width(420.0),
        );
        ui.small(
            "Credentials stay in your system credential store. Sign-in opens your default browser.",
        );
        ui.small("This app reads your profile, likes and playlists, and makes changes only when you ask.");
        ui.hyperlink_to(
            "SoundCloud API terms",
            "https://developers.soundcloud.com/docs/api/terms-of-use",
        );
        ui.collapsing("How this app uses your SoundCloud data", |ui| {
            ui.label("Spotifast Mix reads your profile, likes, playlists, song metadata and playable streams directly from SoundCloud. Likes and playlist changes are sent only when you use their controls.");
            ui.label("Your app secret and tokens are stored in your operating system's protected credential store. Song metadata, SoundCloud artwork and buffered audio are kept for this session. Local playlists save song references and names you choose, without audio or signed stream URLs.");
            ui.label("Disconnect removes this app's stored authorization and clears SoundCloud account data from the interface. Local playlist references remain on this computer. There is no telemetry or hosted Spotifast Mix service.");
        });
        if app.integration.auth_busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Waiting for SoundCloud authorization...");
            });
            if ui.button("Cancel").clicked() {
                emit(app, MixAction::CancelSignIn);
            }
        } else if ui
            .add_enabled(
                !app.integration.client_id.trim().is_empty()
                    && !app.integration.client_secret.trim().is_empty(),
                egui::Button::new("Connect with SoundCloud"),
            )
            .clicked()
        {
            emit(app, MixAction::SignIn);
        }
    }
    if let Some(error) = app.integration.auth_error.clone() {
        message(ui, &error);
    }
}

fn library(app: &mut App, ui: &mut egui::Ui) {
    heading(app, ui, "Your music");
    ui.heading("Local mixed playlists");
    ui.small("Saved on this computer. Songs keep their original service.");
    if let Some(error) = app.integration.local_error.clone() {
        message(ui, &error);
    }
    if !app.integration.local_ready && app.integration.local_error.is_none() {
        ui.spinner();
    }
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut app.integration.draft_name)
                .hint_text("New playlist name")
                .desired_width(220.0),
        );
        if ui
            .add_enabled(
                app.integration.local_ready && !app.integration.draft_name.trim().is_empty(),
                egui::Button::new("Create local playlist"),
            )
            .clicked()
        {
            emit(
                app,
                MixAction::CreateLocal(app.integration.draft_name.clone()),
            );
        }
    });
    let local = app.integration.local.playlists.clone();
    for playlist in local {
        ui.horizontal_wrapped(|ui| {
            if theme::soft_button(ui, &app.palette, None, &playlist.name, false).clicked() {
                emit(app, MixAction::Open(View::LocalPlaylist(playlist.id)));
            }
            theme::text(
                ui,
                format!("Local · {} songs", playlist.items.len()),
                theme::regular(12.0),
                app.palette.secondary,
            );
            if ui
                .add_enabled(!playlist.items.is_empty(), egui::Button::new("Play"))
                .clicked()
            {
                emit(app, MixAction::PlayLocal(playlist.id));
            }
        });
    }
    ui.add_space(20.0);
    ui.heading("Spotify playlists");
    if app.user.is_none() {
        ui.label("Connect Spotify from Accounts to see your Spotify library.");
    }
    let spotify = app.library.playlists.get().cloned().unwrap_or_default();
    for playlist in spotify {
        let response = theme::soft_button(
            ui,
            &app.palette,
            None,
            &format!("{} · Spotify", playlist.name),
            false,
        );
        if response.clicked() {
            app.actions
                .push(Action::Open(Page::Playlist(playlist.id.clone())));
        }
        let uri = if playlist.uri.is_empty() {
            format!("spotify:playlist:{}", playlist.id)
        } else {
            playlist.uri.clone()
        };
        let owned = app.user_id().is_some_and(|user| playlist.owned_by(user));
        egui::Popup::context_menu(&response)
            .frame(super::widgets::menu_frame(&app.palette))
            .show(|ui| {
                super::widgets::context_menu_items(
                    ui,
                    app,
                    &uri,
                    &playlist.name,
                    owned.then_some(&playlist),
                );
            });
    }
    ui.add_space(20.0);
    ui.horizontal_wrapped(|ui| {
        ui.heading("SoundCloud");
        if app.integration.user.is_some() && ui.button("Refresh library").clicked() {
            emit(app, MixAction::Refresh);
        }
    });
    if app.integration.user.is_none() {
        ui.label("Connect SoundCloud to browse your likes and playlists.");
        if ui.button("Connect SoundCloud").clicked() {
            emit(app, MixAction::Open(View::Accounts));
        }
        return;
    }
    if app.integration.library_loading {
        ui.spinner();
    }
    if let Some(error) = app.integration.library_error.clone() {
        message(ui, &error);
    }
    ui.horizontal_wrapped(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut app.integration.draft_remote_name)
                .id_salt("new-soundcloud-playlist")
                .hint_text("New SoundCloud playlist")
                .desired_width(220.0),
        );
        ui.checkbox(&mut app.integration.draft_private, "Private");
        if ui
            .add_enabled(
                !app.integration.mutation_pending
                    && !app.integration.draft_remote_name.trim().is_empty(),
                egui::Button::new("Create on SoundCloud"),
            )
            .clicked()
        {
            emit(
                app,
                MixAction::CreateRemote {
                    name: app.integration.draft_remote_name.clone(),
                    private: app.integration.draft_private,
                },
            );
        }
    });
    if let Some(MixAction::CreateRemote { name, .. }) = &app.integration.pending_edit {
        theme::text(ui, name, theme::semibold(14.0), app.palette.text);
        ui.weak(if app.integration.mutation_pending {
            "Creating on SoundCloud..."
        } else {
            "Creation has not been confirmed. Reload before trying again."
        });
    }
    if let Some(library) = app.integration.library.clone() {
        for playlist in library.playlists.iter().chain(&library.liked_playlists) {
            playlist_link(app, ui, playlist);
        }
        ui.add_space(12.0);
        ui.heading("SoundCloud likes");
        let liked: Vec<_> = library
            .liked_tracks
            .into_iter()
            .filter(|track| {
                app.integration
                    .liked
                    .get(&track.urn)
                    .copied()
                    .unwrap_or(true)
            })
            .collect();
        track_list(app, ui, &liked, "soundcloud-likes");
    }
}

fn playlist_link(app: &mut App, ui: &mut egui::Ui, playlist: &crate::soundcloud::Playlist) {
    ui.horizontal_wrapped(|ui| {
        if theme::soft_button(ui, &app.palette, None, &playlist.title, false).clicked() {
            emit(app, MixAction::OpenPlaylist(playlist.urn.clone()));
        }
        theme::text(
            ui,
            format!(
                "SoundCloud · {} · {} songs",
                playlist.user.username, playlist.track_count
            ),
            theme::regular(12.0),
            app.palette.secondary,
        );
    });
}

fn soundcloud_playlist(app: &mut App, ui: &mut egui::Ui, urn: &str) {
    if app.integration.playlist_loading {
        ui.spinner();
    }
    let Some((playlist, tracks)) = app.integration.playlist.clone() else {
        return;
    };
    heading(app, ui, &playlist.title);
    theme::text(
        ui,
        format!(
            "SoundCloud · {} · {}",
            playlist.user.username, playlist.sharing
        ),
        theme::regular(13.0),
        app.palette.text,
    );
    ui.hyperlink_to("Open on SoundCloud", &playlist.permalink_url);
    if ui
        .add_enabled(
            tracks.iter().any(|track| track.is_playable()),
            egui::Button::new("Play in local mix"),
        )
        .clicked()
    {
        emit(
            app,
            MixAction::Command(Command::Start {
                items: tracks
                    .iter()
                    .filter(|track| track.is_playable())
                    .cloned()
                    .map(MediaItem::from)
                    .collect(),
                index: 0,
                context: Some(crate::mix::MixContext {
                    name: playlist.title.clone(),
                    id: Some(playlist.urn.clone()),
                }),
            }),
        );
    }
    let owned = app
        .integration
        .user
        .as_ref()
        .is_some_and(|user| !user.urn.is_empty() && user.urn == playlist.user.urn);
    if owned {
        ui.add_enabled_ui(!app.integration.mutation_pending, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::TextEdit::singleline(&mut app.integration.draft_remote_name).hint_text("New name").desired_width(220.0));
                if ui.add_enabled(!app.integration.draft_remote_name.trim().is_empty(), egui::Button::new("Rename")).clicked() { emit(app, MixAction::RenameRemote { urn: urn.into(), name: app.integration.draft_remote_name.clone() }); }
                ui.menu_button("Delete playlist", |ui| {
                    ui.label("Delete this playlist from SoundCloud?");
                    if ui.button("Delete permanently").clicked() { emit(app, MixAction::DeleteRemote(urn.into())); ui.close(); }
                });
            });
            ui.small("Use a song's menu to add it to this SoundCloud playlist. Only SoundCloud songs can be added.");
        });
    }
    let expected: Vec<_> = tracks.iter().map(|t| t.urn.clone()).collect();
    let playable: Vec<_> = tracks
        .iter()
        .filter(|track| track.is_playable())
        .cloned()
        .map(MediaItem::from)
        .collect();
    let playing_from = crate::mix::MixContext {
        name: playlist.title.clone(),
        id: Some(playlist.urn.clone()),
    };
    let references: Vec<_> = expected.iter().cloned().map(MediaRef::SoundCloud).collect();
    let mut selection =
        bulk_selection_controls(app, ui, &format!("soundcloud-playlist:{urn}"), &references);
    let mut playable_index = 0;
    for (index, track) in tracks.iter().enumerate() {
        ui.push_id((urn, index), |ui| {
            selected_media_row(
                app,
                ui,
                &MediaItem::from(track.clone()),
                Some((&playable, playable_index, &playing_from)),
                &mut selection,
                index,
            );
            if owned {
                ui.add_enabled_ui(!app.integration.mutation_pending, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(index > 0, egui::Button::new("Move up"))
                            .clicked()
                        {
                            let mut desired = expected.clone();
                            desired.swap(index, index - 1);
                            emit(
                                app,
                                MixAction::ReplaceRemote {
                                    urn: urn.into(),
                                    expected: expected.clone(),
                                    desired,
                                },
                            );
                        }
                        if ui
                            .add_enabled(index + 1 < tracks.len(), egui::Button::new("Move down"))
                            .clicked()
                        {
                            let mut desired = expected.clone();
                            desired.swap(index, index + 1);
                            emit(
                                app,
                                MixAction::ReplaceRemote {
                                    urn: urn.into(),
                                    expected: expected.clone(),
                                    desired,
                                },
                            );
                        }
                        if ui.button("Remove from playlist").clicked() {
                            let mut desired = expected.clone();
                            desired.remove(index);
                            emit(
                                app,
                                MixAction::ReplaceRemote {
                                    urn: urn.into(),
                                    expected: expected.clone(),
                                    desired,
                                },
                            );
                        }
                    });
                });
            }
        });
        if track.is_playable() {
            playable_index += 1;
        }
    }
    selection.save(ui);
}

fn track_list(app: &mut App, ui: &mut egui::Ui, tracks: &[crate::soundcloud::Track], scope: &str) {
    let playable: Vec<_> = tracks
        .iter()
        .filter(|track| track.is_playable())
        .cloned()
        .map(MediaItem::from)
        .collect();
    let playing_from = crate::mix::MixContext {
        name: "SoundCloud songs".into(),
        id: None,
    };
    let references: Vec<_> = tracks
        .iter()
        .map(|track| MediaRef::SoundCloud(track.urn.clone()))
        .collect();
    let mut selection = bulk_selection_controls(app, ui, scope, &references);
    let mut playable_index = 0;
    for (index, track) in tracks.iter().enumerate() {
        ui.push_id((&track.urn, index), |ui| {
            selected_media_row(
                app,
                ui,
                &MediaItem::from(track.clone()),
                Some((&playable, playable_index, &playing_from)),
                &mut selection,
                index,
            )
        });
        if track.is_playable() {
            playable_index += 1;
        }
    }
    selection.save(ui);
}

pub fn media_row(app: &mut App, ui: &mut egui::Ui, item: &MediaItem) {
    media_row_with_context(app, ui, item, None);
}

fn media_row_with_context(
    app: &mut App,
    ui: &mut egui::Ui,
    item: &MediaItem,
    context: Option<(&[MediaItem], usize, &crate::mix::MixContext)>,
) {
    media_row_with_selection(app, ui, item, context, None);
}

fn selected_media_row(
    app: &mut App,
    ui: &mut egui::Ui,
    item: &MediaItem,
    context: Option<(&[MediaItem], usize, &crate::mix::MixContext)>,
    selection: &mut BulkSelection,
    index: usize,
) {
    let mut selected = selection.rows.contains(&index);
    media_row_with_selection(app, ui, item, context, Some(&mut selected));
    selection.set(index, selected);
}

fn media_row_with_selection(
    app: &mut App,
    ui: &mut egui::Ui,
    item: &MediaItem,
    context: Option<(&[MediaItem], usize, &crate::mix::MixContext)>,
    selected: Option<&mut bool>,
) {
    let palette = app.palette;
    egui::Frame::new()
        .fill(palette.panel)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if let Some(selected) = selected {
                    ui.checkbox(selected, "Select");
                }
                let (cover, _) =
                    ui.allocate_exact_size(egui::vec2(42.0, 42.0), egui::Sense::hover());
                super::widgets::paint_cover(
                    ui,
                    &palette,
                    item.artwork(),
                    cover,
                    4.0,
                    crate::theme::Icon::Music,
                    Some(app.backend.art()),
                );
                let play = ui.add_enabled(item.playable(), egui::Button::new("Play"));
                if play.clicked() && !play.double_clicked() {
                    emit(
                        app,
                        MixAction::Command(match context {
                            Some((items, index, context)) => Command::Start {
                                items: items.to_vec(),
                                index,
                                context: Some(context.clone()),
                            },
                            None => Command::Start {
                                items: vec![item.clone()],
                                index: 0,
                                context: None,
                            },
                        }),
                    );
                }
                ui.vertical(|ui| {
                    let title = theme::text(ui, item.title(), theme::semibold(14.0), palette.text)
                        .interact(egui::Sense::click());
                    if title.double_clicked() && item.playable() {
                        emit(
                            app,
                            MixAction::Command(match context {
                                Some((items, index, context)) => Command::Start {
                                    items: items.to_vec(),
                                    index,
                                    context: Some(context.clone()),
                                },
                                None => Command::Start {
                                    items: vec![item.clone()],
                                    index: 0,
                                    context: None,
                                },
                            }),
                        );
                    }
                    theme::text(
                        ui,
                        format!("{} · {}", item.source(), item.creator()),
                        theme::regular(12.0),
                        palette.secondary,
                    );
                    if let MediaItem::SoundCloud(track) = item {
                        theme::text(
                            ui,
                            format!("Uploaded by {}", track.user.username),
                            theme::regular(11.5),
                            palette.secondary,
                        );
                        if !track.is_playable() {
                            ui.weak(if track.access == crate::soundcloud::Access::Preview {
                                "Preview only. Full playback unavailable."
                            } else {
                                "Unavailable through SoundCloud's API"
                            });
                        }
                    }
                });
                if let MediaItem::SoundCloud(track) = item {
                    soundcloud_badge(app, ui, &track.permalink_url);
                }
                let seconds = item.duration_ms() / 1000;
                theme::text(
                    ui,
                    format!("{}:{:02}", seconds / 60, seconds % 60),
                    theme::regular(12.0),
                    app.palette.secondary,
                );
                ui.menu_button("...", |ui| media_menu(app, ui, item));
                if let MediaItem::SoundCloud(track) = item {
                    let liked = app
                        .integration
                        .liked
                        .get(&track.urn)
                        .copied()
                        .unwrap_or(track.user_favorite);
                    if ui.button(if liked { "Unlike" } else { "Like" }).clicked() {
                        emit(
                            app,
                            MixAction::Like {
                                urn: track.urn.clone(),
                                liked: !liked,
                            },
                        );
                    }
                    if app.integration.pending_likes.contains_key(&track.urn) {
                        ui.small("Pending");
                    }
                }
            });
        });
    ui.add_space(3.0);
}

pub fn media_menu(app: &mut App, ui: &mut egui::Ui, item: &MediaItem) {
    if ui
        .add_enabled(item.playable(), egui::Button::new("Play in local mix"))
        .clicked()
    {
        emit(
            app,
            MixAction::Command(Command::Start {
                items: vec![item.clone()],
                index: 0,
                context: None,
            }),
        );
        ui.close();
    }
    if ui
        .add_enabled(item.playable(), egui::Button::new("Add to local mix"))
        .clicked()
    {
        emit(app, MixAction::Command(Command::Add(vec![item.clone()])));
        ui.close();
    }
    if ui
        .add_enabled(item.playable(), egui::Button::new("Play next in local mix"))
        .clicked()
    {
        emit(
            app,
            MixAction::Command(Command::PlayNext(vec![item.clone()])),
        );
        ui.close();
    }
    add_to_local_playlist_menu(app, ui, &[item.media_ref()]);
    if let MediaItem::SoundCloud(track) = item {
        let owned = app
            .integration
            .library
            .as_ref()
            .map(|library| library.playlists.clone())
            .unwrap_or_default();
        ui.menu_button("Add to SoundCloud playlist", |ui| {
            for playlist in owned {
                if ui
                    .add_enabled(
                        !app.integration.mutation_pending,
                        egui::Button::new(&playlist.title),
                    )
                    .clicked()
                {
                    emit(
                        app,
                        MixAction::AppendRemote {
                            urn: playlist.urn,
                            items: vec![track.urn.clone()],
                        },
                    );
                    ui.close();
                }
            }
        });
    }
    let permalink = item.permalink();
    if permalink.starts_with("https://") {
        ui.hyperlink_to(format!("Open on {}", item.source()), permalink);
    }
}

pub fn queue(app: &mut App, ui: &mut egui::Ui) {
    heading(app, ui, "Local mix");
    session_selector(app, ui);
    if app.integration.local_active
        && ui
            .add_enabled(
                app.integration.local_ready
                    && (app.integration.snapshot.current.is_some()
                        || !app.integration.snapshot.manual.is_empty()),
                egui::Button::new("Save as local playlist"),
            )
            .clicked()
    {
        app.actions.push(Action::SaveQueueAsPlaylist);
    }
    if let Some(error) = app.integration.snapshot.error.clone() {
        message(ui, &error);
    }
    if let Some(current) = app.integration.snapshot.current.clone() {
        ui.heading("Now playing");
        media_row(app, ui, &current.item);
    }
    ui.horizontal(|ui| {
        ui.heading("Playing next");
        if !app.integration.snapshot.manual.is_empty() && ui.button("Clear").clicked() {
            emit(app, MixAction::Command(Command::Clear));
        }
    });
    let manual = app.integration.snapshot.manual.clone();
    for (index, entry) in manual.iter().enumerate() {
        ui.push_id(entry.id, |ui| {
            ui.horizontal_wrapped(|ui| {
                if theme::soft_button(
                    ui,
                    &app.palette,
                    None,
                    &format!("{} · {}", entry.item.title(), entry.item.source()),
                    false,
                )
                .clicked()
                {
                    emit(app, MixAction::Command(Command::PlayEntry(entry.id)));
                }
                if ui.add_enabled(index > 0, egui::Button::new("Up")).clicked() {
                    emit(
                        app,
                        MixAction::Command(Command::Move {
                            from: index,
                            to: index - 1,
                        }),
                    );
                }
                if ui
                    .add_enabled(index + 1 < manual.len(), egui::Button::new("Down"))
                    .clicked()
                {
                    emit(
                        app,
                        MixAction::Command(Command::Move {
                            from: index,
                            to: index + 1,
                        }),
                    );
                }
                if ui.button("Remove").clicked() {
                    emit(app, MixAction::Command(Command::Remove(vec![entry.id])));
                }
            });
        });
    }
    if manual.is_empty() {
        ui.weak("Add songs from either service using Add to local mix.");
    }
    ui.add_space(12.0);
    ui.heading("Next up");
    if let Some(context) = &app.integration.snapshot.context_info {
        ui.weak(&context.name);
    }
    for entry in app.integration.snapshot.context.clone() {
        if theme::soft_button(
            ui,
            &app.palette,
            None,
            &format!("{} · {}", entry.item.title(), entry.item.source()),
            false,
        )
        .clicked()
        {
            emit(app, MixAction::Command(Command::PlayEntry(entry.id)));
        }
    }
}

pub fn session_selector(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        if ui
            .selectable_label(!app.integration.local_active, "Spotify Connect")
            .clicked()
        {
            emit(app, MixAction::SelectMix(false));
        }
        if ui
            .selectable_label(app.integration.local_active, "Local mix")
            .clicked()
        {
            emit(app, MixAction::SelectMix(true));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_selection_keeps_duplicate_rows_and_resets_when_its_list_changes() {
        let items = vec![
            MediaRef::SoundCloud("soundcloud:tracks:1".into()),
            MediaRef::Spotify("spotify:track:2".into()),
            MediaRef::SoundCloud("soundcloud:tracks:1".into()),
        ];
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut picked = BulkSelection::load(ui, "local:1", 0, &items);
            picked.set(2, true);
            picked.set(0, true);
            picked.save(ui);

            let restored = BulkSelection::load(ui, "local:1", 0, &items);
            assert_eq!(
                restored.items(&items),
                vec![items[0].clone(), items[2].clone()]
            );
            assert!(
                BulkSelection::load(ui, "local:2", 0, &items)
                    .rows
                    .is_empty()
            );
            assert!(
                BulkSelection::load(ui, "local:1", 1, &items)
                    .rows
                    .is_empty()
            );
            let mut changed = items.clone();
            changed.swap(0, 1);
            assert!(
                BulkSelection::load(ui, "local:1", 0, &changed)
                    .rows
                    .is_empty()
            );
        });
        output.textures_delta.clear();
    }
}
