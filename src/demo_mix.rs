//! Opt-in SoundCloud and mixed-library fixtures. No credentials or streams.

use crate::api::models::{PlayableItem, SearchResults};
use crate::app::App;
use crate::integration::{LocalPlaylists, Source, State, View};
use crate::mix::{Command, MediaItem, MixContext, MixSnapshot, MixedPlaylist};
use crate::model::{Loadable, Page, SearchFilter};
use crate::soundcloud::{Access, Library, Playlist, Track, User};

pub(super) fn is_surface(surface: &str) -> bool {
    matches!(
        surface,
        "mix-library"
            | "mix-queue"
            | "mix-playlist"
            | "soundcloud-search"
            | "soundcloud-playlist"
            | "soundcloud-only"
            | "mix-error"
            | "mix-loading"
            | "mix-accounts"
    )
}

fn user() -> User {
    User {
        urn: "soundcloud:users:900000000000000001".into(),
        username: "Indigo Arcade".into(),
        permalink_url: "https://soundcloud.com/mix-demo-uploader".into(),
        // No real user image is fetched or written to an artwork cache.
        avatar_url: None,
    }
}

fn tracks() -> Vec<Track> {
    [
        ("Night Walk (Demo Edit)", 232_000, Access::Playable),
        ("Glasshouse Session", 188_000, Access::Playable),
        ("Afterglow Sketch", 271_000, Access::Playable),
        ("Morning Loop", 205_000, Access::Playable),
        ("Preview Demo", 30_000, Access::Preview),
        ("Unavailable Demo", 194_000, Access::Blocked),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (title, duration, access))| Track {
        urn: format!(
            "soundcloud:tracks:{}",
            900_000_000_000_000_001u64 + index as u64
        ),
        title: title.into(),
        duration,
        artwork_url: None,
        permalink_url: format!("https://soundcloud.com/mix-demo-uploader/demo-track-{index}"),
        user: user(),
        metadata_artist: Some("Indigo Arcade".into()),
        streamable: access != Access::Blocked,
        access,
        user_favorite: index < 3,
    })
    .collect()
}

fn spotify(index: usize) -> MediaItem {
    let mut track = super::track(index);
    if let Some(album) = &mut track.album {
        album.images.clear();
    }
    PlayableItem::Track(track).into()
}

/// Populate only when a mixed demo page or surface is explicitly requested.
/// Existing Spotify screenshots keep their existing fixtures unchanged.
pub(super) fn populate(app: &mut App) {
    let tracks = tracks();
    let own = Playlist {
        urn: "soundcloud:playlists:900000000000000001".into(),
        title: "Independent finds".into(),
        description: Some("Invented tracks for the native integration demo.".into()),
        sharing: "private".into(),
        permalink_url: "https://soundcloud.com/mix-demo-uploader/sets/independent-finds".into(),
        artwork_url: None,
        user: user(),
        track_count: 5,
        tracks: vec![
            tracks[0].clone(),
            tracks[1].clone(),
            tracks[0].clone(),
            tracks[2].clone(),
            tracks[3].clone(),
        ],
        last_modified: Some("2026-10-20T12:00:00Z".into()),
    };
    let followed = Playlist {
        urn: "soundcloud:playlists:900000000000000002".into(),
        title: "Night-drive uploads".into(),
        sharing: "public".into(),
        user: User {
            urn: "soundcloud:users:900000000000000002".into(),
            username: "Demo Collective".into(),
            permalink_url: "https://soundcloud.com/mix-demo-collective".into(),
            avatar_url: None,
        },
        track_count: 12,
        ..Playlist::default()
    };
    let items = vec![
        tracks[0].clone().into(),
        spotify(0),
        tracks[1].clone().into(),
        spotify(1),
    ];
    let mut snapshot = MixSnapshot::default();
    snapshot.preview(&Command::Start {
        items: items.clone(),
        index: 0,
        context: Some(MixContext {
            name: "After-hours blend".into(),
            id: Some("local:playlist:1".into()),
        }),
    });
    // Repeated songs are different occurrences in the manual queue.
    snapshot.preview(&Command::Add(vec![
        tracks[2].clone().into(),
        spotify(2),
        tracks[2].clone().into(),
    ]));
    snapshot.preview(&Command::Pause);
    snapshot.preview(&Command::Seek(83_000));
    snapshot.preview(&Command::Volume(45_875));
    snapshot.acknowledged = 1;
    let library = Library {
        user: user(),
        liked_tracks: tracks[..3].to_vec(),
        playlists: vec![own.clone()],
        liked_playlists: vec![followed.clone()],
    };
    app.integration = State {
        source: Source::All,
        user: Some(user()),
        generation: 1,
        serial: 1,
        local_revision: 1,
        persisted_revision: 1,
        mix_serial: 1,
        search_query: "night".into(),
        tracks: tracks.clone(),
        search_playlists: vec![own.clone(), followed],
        library: Some(library),
        playlist: Some((own.clone(), own.tracks.clone())),
        liked: tracks
            .iter()
            .map(|track| (track.urn.clone(), track.user_favorite))
            .collect(),
        local: LocalPlaylists {
            version: 1,
            next_id: 3,
            playlists: vec![
                MixedPlaylist {
                    id: 1,
                    name: "After-hours blend".into(),
                    items: items.iter().map(MediaItem::media_ref).collect(),
                },
                MixedPlaylist {
                    id: 2,
                    name: "Weekend discoveries".into(),
                    items: vec![
                        items[1].media_ref(),
                        items[0].media_ref(),
                        items[0].media_ref(),
                    ],
                },
            ],
        },
        local_ready: true,
        snapshot,
        resolved_items: items,
        ..State::default()
    };
    // Keep both provider sections visible at the representative narrow size.
    if let Loadable::Loaded(playlists) = &mut app.library.playlists {
        playlists.truncate(2);
    }
    app.settings.art_expanded = false;
    app.settings.sidebar_width = 230.0;
}

pub(super) fn apply(app: &mut App, surface: &str) -> bool {
    if !is_surface(surface) {
        return false;
    }
    match surface {
        "mix-library" => app.integration.view = View::Library,
        "mix-queue" | "mix-error" => {
            app.integration.view = View::Queue;
            app.integration.local_active = true;
            if surface == "mix-error" {
                app.integration.snapshot.error =
                    Some("SoundCloud could not start this song. Try again.".into());
            }
        }
        "mix-playlist" => {
            app.integration.view = View::LocalPlaylist(1);
            app.integration.local_active = true;
        }
        "soundcloud-playlist" => {
            app.integration.view = View::Playlist("soundcloud:playlists:900000000000000001".into());
        }
        "soundcloud-search" => {
            app.integration.source = Source::All;
            app.search.query = "night".into();
            app.search.committed = "night".into();
            app.search.filter = SearchFilter::Songs;
            app.search.catalogue_pending = false;
            app.search.playlists_pending = false;
            app.search.results = Loadable::Loaded(SearchResults {
                tracks: Some(super::page(vec![super::track(0), super::track(1)])),
                ..SearchResults::default()
            });
            app.open(Page::Search);
            return true;
        }
        "soundcloud-only" => {
            app.user = None;
            app.auth = crate::backend::AuthStatus::SignedOut;
            app.remote = None;
            app.local_ready = false;
            app.local.connected = false;
            app.local_device_id = None;
            app.local_playback = crate::backend::LocalPlayback::Unavailable;
            app.library = Default::default();
            app.library.playlists = Loadable::Loaded(Vec::new());
            app.saved.clear();
            app.integration.view = View::Library;
            app.integration.local_active = true;
        }
        "mix-loading" => {
            app.integration.view = View::Library;
            app.integration.library = None;
            app.integration.library_loading = true;
            app.integration.local_ready = false;
        }
        "mix-accounts" => {
            app.integration.view = View::Accounts;
            app.integration.user = None;
            app.integration.library = None;
            app.integration.client_id.clear();
            app.integration.client_secret.clear();
        }
        _ => return false,
    }
    app.open(Page::Mix);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppOptions;
    use crate::paths::AppDirs;
    use crate::player::Playback;
    use crate::settings::{Settings, ThemeChoice};

    fn fixture(name: &str, dark: bool) -> (egui::Context, App) {
        let root = std::env::temp_dir().join(format!(
            "spotifast-mix-ui-{name}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let waker = crate::backend::Waker::default();
        waker.attach(&ctx);
        let mut app = App::new(
            &waker,
            AppDirs {
                config: root.join("config"),
                state: root.join("state"),
                cache: root.join("cache"),
            },
            Settings {
                theme: if dark {
                    ThemeChoice::Dark
                } else {
                    ThemeChoice::Light
                },
                ..Settings::default()
            },
            AppOptions {
                media_controls: false,
                restore_sign_in: false,
                tray: false,
                autostart: false,
            },
        );
        app.attach(&ctx);
        crate::demo::populate(&mut app);
        populate(&mut app);
        // These tests draw the view directly instead of running App's frame
        // controller, which normally applies the selected palette first.
        app.palette = if dark {
            crate::theme::Palette::dark()
        } else {
            crate::theme::Palette::light()
        };
        crate::theme::apply(&ctx, &app.palette);
        app.reveal_theme_changes = false;
        (ctx, app)
    }

    fn text(shape: &egui::epaint::Shape, found: &mut Vec<String>) {
        match shape {
            egui::epaint::Shape::Text(shape) => found.push(shape.galley.job.text.clone()),
            egui::epaint::Shape::Vec(shapes) => shapes.iter().for_each(|shape| text(shape, found)),
            _ => {}
        }
    }

    fn draw(ctx: &egui::Context, app: &mut App, width: f32) -> Vec<String> {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 800.0),
                )),
                ..Default::default()
            },
            |ui| crate::ui::show(app, ui),
        );
        let mut found = Vec::new();
        output
            .shapes
            .iter()
            .for_each(|shape| text(&shape.shape, &mut found));
        output.textures_delta.clear();
        found
    }

    #[test]
    fn mixed_surfaces_render_at_both_sizes_and_themes_without_live_accounts() {
        let cases: [(&str, &[&str]); 9] = [
            (
                "mix-library",
                &["Your music", "Local mixed playlists", "After-hours blend"],
            ),
            ("mix-queue", &["Local mix", "Playing next", "Next up"]),
            (
                "mix-playlist",
                &[
                    "After-hours blend",
                    "Local mixed playlist",
                    "Saved on this computer",
                    "TITLE",
                ],
            ),
            (
                "soundcloud-search",
                &["SoundCloud", "Night Walk (Demo Edit)"],
            ),
            (
                "soundcloud-playlist",
                &[
                    "Independent finds",
                    "Open on SoundCloud",
                    "Rename",
                    "Delete playlist",
                ],
            ),
            (
                "soundcloud-only",
                &["Your music", "Local mixed playlists", "SoundCloud"],
            ),
            (
                "mix-error",
                &[
                    "Local mix",
                    "SoundCloud could not start this song. Try again.",
                ],
            ),
            ("mix-loading", &["Your music", "Local mixed playlists"]),
            (
                "mix-accounts",
                &[
                    "Accounts",
                    "Client ID",
                    "Client secret",
                    "Connect with SoundCloud",
                ],
            ),
        ];
        for dark in [false, true] {
            for width in [760.0, 1200.0] {
                for (surface, expected) in cases {
                    let (ctx, mut app) = fixture(surface, dark);
                    assert!(apply(&mut app, surface));
                    draw(&ctx, &mut app, width);
                    let found = draw(&ctx, &mut app, width);
                    for expected in expected {
                        assert!(
                            found.iter().any(|text| text.contains(expected)),
                            "{surface}, dark={dark}, width={width}: missing {expected:?} in {found:?}"
                        );
                    }
                    app.backend.shutdown();
                }
            }
        }
    }

    #[test]
    fn mixed_demo_has_separate_occurrences_and_persists_only_soundcloud_references() {
        let (_, mut app) = fixture("references", true);
        let snapshot = &app.integration.snapshot;
        assert_eq!(snapshot.playback, Playback::Paused);
        assert_eq!(snapshot.position_now(), 83_000);
        assert_eq!(snapshot.manual.len(), 3);
        assert_eq!(snapshot.context.len(), 3);
        assert_eq!(
            snapshot.manual[0].item.media_ref(),
            snapshot.manual[2].item.media_ref()
        );
        assert_ne!(snapshot.manual[0].id, snapshot.manual[2].id);
        let saved = snapshot.session().encode().unwrap();
        assert!(saved.contains("soundcloud:tracks:"));
        assert!(!saved.contains("Night Walk") && !saved.contains("Indigo Arcade"));
        assert!(app.integration.client_id.is_empty() && app.integration.client_secret.is_empty());
        app.backend.shutdown();
    }

    #[test]
    fn soundcloud_only_demo_has_no_previous_spotify_library() {
        let (_, mut app) = fixture("soundcloud-without-spotify", true);
        assert!(apply(&mut app, "soundcloud-only"));
        assert!(app.user.is_none());
        assert!(app.library.playlists.get().unwrap().is_empty());
        assert!(app.library.liked.items.is_empty());
        assert!(app.library.liked.total.is_none());
        assert!(app.saved.is_empty());
        assert!(app.integration.user.is_some());
        assert!(
            !app.integration
                .library
                .as_ref()
                .unwrap()
                .liked_tracks
                .is_empty()
        );
        app.backend.shutdown();
    }
}
