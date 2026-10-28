//! Local playlists share the collection hero and the native track-table style.

use super::{BulkSelection, add_to_local_playlist_menu, emit, media_menu};
use crate::{
    app::App,
    integration::Action as MixAction,
    mix::{Command, MediaItem, MediaRef, MixContext, MixedPlaylist},
    model::Action,
    theme::{self, Icon},
};
use egui::{Align, Align2, Layout, Rect, Sense, Vec2, pos2, vec2};
use std::collections::HashMap;

#[derive(Clone)]
struct LocalDrag {
    playlist: u64,
    index: usize,
    revision: u64,
}

pub(super) fn show(app: &mut App, ui: &mut egui::Ui, id: u64) {
    let Some(playlist) = app
        .integration
        .local
        .playlists
        .iter()
        .find(|p| p.id == id)
        .cloned()
    else {
        super::super::widgets::empty_state(
            ui,
            &app.palette,
            Icon::Music,
            "Playlist removed",
            "This local playlist no longer exists.",
        );
        return;
    };
    let resolved = app.integration.resolved_items.clone();
    let by_ref: HashMap<_, _> = resolved
        .iter()
        .map(|item| (item.media_ref(), item))
        .collect();
    let mut playable = Vec::new();
    let positions: Vec<_> = playlist
        .items
        .iter()
        .map(|reference| {
            by_ref
                .get(reference)
                .filter(|item| item.playable())
                .map(|item| {
                    let index = playable.len();
                    playable.push((**item).clone());
                    index
                })
        })
        .collect();
    let cover = playlist
        .items
        .iter()
        .find_map(|reference| by_ref.get(reference)?.artwork());
    let duration = playlist
        .items
        .iter()
        .filter_map(|r| by_ref.get(r))
        .map(|i| u64::from(i.duration_ms()))
        .sum();
    super::super::collection::hero(
        app,
        ui,
        super::super::collection::Hero {
            images: super::super::collection::HeroImages {
                image: cover,
                ..Default::default()
            },
            liked: false,
            kind: "Local mixed playlist".into(),
            title: &playlist.name,
            description: None,
            byline: vec![
                ("Saved on this computer".into(), None),
                (
                    super::super::collection::songs_and_duration(
                        app.locale,
                        playlist.items.len() as u32,
                        duration,
                    ),
                    None,
                ),
            ],
            avatar: None,
            round: false,
        },
    );
    controls(app, ui, &playlist);
    let mut selection = BulkSelection::load(
        ui,
        &format!("local-playlist:{id}"),
        app.integration.generation,
        &playlist.items,
    );
    if !playlist.items.is_empty() {
        ui.horizontal(|ui| {
            let mut all = selection.rows.len() == playlist.items.len();
            if ui.checkbox(&mut all, "Select all").changed() {
                selection.rows = if all {
                    (0..playlist.items.len()).collect()
                } else {
                    Default::default()
                };
            }
            if !selection.rows.is_empty() {
                theme::text(
                    ui,
                    format!("{} selected", selection.rows.len()),
                    theme::regular(12.5),
                    app.palette.secondary,
                );
                add_to_local_playlist_menu(app, ui, &selection.items(&playlist.items));
            }
            if app.integration.resolving {
                ui.spinner();
            }
        });
        table_heading(app, ui);
    } else {
        super::super::widgets::empty_state(
            ui,
            &app.palette,
            Icon::Music,
            "Your playlist is ready",
            "Add songs from search or use Add song above.",
        );
    }
    let context = MixContext {
        name: playlist.name.clone(),
        id: Some(format!("local:{id}")),
    };
    let top = ui.cursor().top();
    let mut drop = None;
    super::super::widgets::virtual_rows(
        ui,
        playlist.items.len(),
        theme::ROW_HEIGHT,
        |ui, index| {
            let reference = &playlist.items[index];
            let item = by_ref.get(reference).copied();
            ui.push_id((id, index), |ui| {
                row(
                    app,
                    ui,
                    LocalRow {
                        playlist: &playlist,
                        index,
                        item,
                        playback: RowPlayback {
                            items: &playable,
                            index: positions[index],
                            context: &context,
                        },
                    },
                    &mut selection,
                );
            });
            if let Some(drag) = egui::DragAndDrop::payload::<LocalDrag>(ui.ctx())
                && drag.playlist == id
                && drag.revision == app.integration.local_revision
                && let Some(pointer) = ui
                    .ctx()
                    .pointer_latest_pos()
                    .filter(|p| ui.clip_rect().contains(*p))
            {
                let target = ((pointer.y - top) / theme::ROW_HEIGHT).floor().max(0.0) as usize;
                if target == index {
                    let y = top + index as f32 * theme::ROW_HEIGHT;
                    ui.painter().hline(
                        ui.max_rect().x_range(),
                        y,
                        egui::Stroke::new(2.0, app.palette.accent),
                    );
                    if ui.input(|input| input.pointer.button_released(egui::PointerButton::Primary))
                    {
                        drop = Some((drag.index, index));
                    }
                }
            }
        },
    );
    if let Some((from, to)) = drop {
        egui::DragAndDrop::take_payload::<LocalDrag>(ui.ctx());
        if from != to {
            emit(app, MixAction::MoveLocal { id, from, to });
        }
    }
    selection.save(ui);
}

fn controls(app: &mut App, ui: &mut egui::Ui, playlist: &MixedPlaylist) {
    let palette = app.palette;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 18.0;
        if app.integration.resolving_play {
            theme::circle_spinner(ui, 56.0, palette.accent, palette.on_accent, "Starting...");
        } else if ui
            .add_enabled_ui(!playlist.items.is_empty(), |ui| {
                theme::circle_button(
                    ui,
                    Icon::PlayFilled,
                    56.0,
                    palette.accent,
                    palette.accent_hover,
                    palette.on_accent,
                    "Play in local mix",
                )
            })
            .inner
            .clicked()
        {
            emit(app, MixAction::PlayLocal(playlist.id));
        }
        let shuffle = app.integration.snapshot.shuffle;
        if theme::icon_button(
            ui,
            Icon::Shuffle,
            26.0,
            if shuffle {
                palette.accent
            } else {
                palette.secondary
            },
            palette.text,
            "Shuffle local mix",
        )
        .clicked()
        {
            emit(app, MixAction::Command(Command::Shuffle(!shuffle)));
        }
        let more = theme::icon_button(
            ui,
            Icon::Ellipsis,
            26.0,
            palette.secondary,
            palette.text,
            "Playlist options",
        );
        egui::Popup::menu(&more)
            .frame(super::super::widgets::menu_frame(&palette))
            .show(|ui| {
                ui.set_min_width(220.0);
                ui.set_max_width(320.0);
                ui.menu_button("Rename playlist", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut app.integration.draft_name)
                            .hint_text("New name")
                            .desired_width(200.0),
                    );
                    if ui
                        .add_enabled(
                            !app.integration.draft_name.trim().is_empty(),
                            egui::Button::new("Save"),
                        )
                        .clicked()
                    {
                        emit(
                            app,
                            MixAction::RenameLocal {
                                id: playlist.id,
                                name: app.integration.draft_name.clone(),
                            },
                        );
                        ui.close();
                    }
                });
                ui.menu_button("Delete playlist", |ui| {
                    if super::super::widgets::menu_item(
                        ui,
                        &palette,
                        Some(Icon::Trash),
                        "Delete local playlist",
                    ) {
                        emit(app, MixAction::DeleteLocal(playlist.id));
                    }
                });
            });
        let add = theme::icon_button(
            ui,
            Icon::CirclePlus,
            26.0,
            palette.secondary,
            palette.text,
            "Add song",
        );
        egui::Popup::menu(&add)
            .frame(super::super::widgets::menu_frame(&palette))
            .show(|ui| {
                ui.set_max_width(340.0);
                ui.label("Add song");
                ui.add(
                    egui::TextEdit::singleline(&mut app.integration.draft_link)
                        .hint_text("Spotify or SoundCloud song link")
                        .desired_width(280.0),
                );
                if ui
                    .add_enabled(
                        !app.integration.draft_link.trim().is_empty() && !app.integration.resolving,
                        egui::Button::new("Add song"),
                    )
                    .clicked()
                {
                    let text = app.integration.draft_link.trim().to_string();
                    if let Some(uri) = crate::link::parse(&text).filter(|uri| {
                        uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:")
                    }) {
                        emit(
                            app,
                            MixAction::AddLocal {
                                id: playlist.id,
                                items: vec![MediaRef::Spotify(uri)],
                            },
                        );
                    } else {
                        emit(app, MixAction::OpenLink(text));
                    }
                    ui.close();
                }
            });
        if theme::icon_button(
            ui,
            Icon::Refresh,
            24.0,
            palette.secondary,
            palette.text,
            "Refresh playlist songs",
        )
        .clicked()
        {
            emit(
                app,
                MixAction::Open(crate::integration::View::LocalPlaylist(playlist.id)),
            );
        }
    });
    ui.add_space(14.0);
}

fn table_heading(app: &App, ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    let painter = ui.painter();
    let font = theme::regular(12.0);
    let color = app.palette.secondary;
    painter.text(
        pos2(rect.left() + 30.0, rect.center().y),
        Align2::CENTER_CENTER,
        "#",
        font.clone(),
        color,
    );
    painter.text(
        pos2(rect.left() + 104.0, rect.center().y),
        Align2::LEFT_CENTER,
        "TITLE",
        font.clone(),
        color,
    );
    if rect.width() > 560.0 {
        painter.text(
            pos2(rect.right() - 228.0, rect.center().y),
            Align2::LEFT_CENTER,
            "SOURCE",
            font,
            color,
        );
    }
    theme::paint_icon(
        ui,
        Icon::Clock,
        Rect::from_center_size(
            pos2(rect.right() - 72.0, rect.center().y),
            Vec2::splat(16.0),
        ),
        16.0,
        color,
    );
    painter.hline(
        rect.x_range(),
        rect.bottom(),
        egui::Stroke::new(1.0, app.palette.outline),
    );
}

struct RowPlayback<'a> {
    items: &'a [MediaItem],
    index: Option<usize>,
    context: &'a MixContext,
}

struct LocalRow<'a> {
    playlist: &'a MixedPlaylist,
    index: usize,
    item: Option<&'a MediaItem>,
    playback: RowPlayback<'a>,
}

fn play(app: &mut App, playback: &RowPlayback<'_>) {
    if let Some(index) = playback.index {
        emit(
            app,
            MixAction::Command(Command::Start {
                items: playback.items.to_vec(),
                index,
                context: Some(playback.context.clone()),
            }),
        );
    }
}

fn row(app: &mut App, ui: &mut egui::Ui, row: LocalRow<'_>, selection: &mut BulkSelection) {
    let LocalRow {
        playlist,
        index,
        item,
        playback,
    } = row;
    let play_index = playback.index;
    let palette = app.palette;
    let (rect, response) = ui.allocate_exact_size(
        vec2(ui.available_width(), theme::ROW_HEIGHT),
        Sense::click_and_drag(),
    );
    let selected = selection.rows.contains(&index);
    let hovered = ui.rect_contains_pointer(rect) || response.has_focus();
    let title = item
        .map(MediaItem::title)
        .unwrap_or(if app.integration.resolving {
            "Loading song..."
        } else {
            "Unavailable song"
        });
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), selected, title)
    });
    if selected || hovered {
        ui.painter().rect_filled(
            rect,
            6.0,
            if selected {
                palette.secondary.gamma_multiply(0.2)
            } else {
                palette
                    .surface_hover
                    .gamma_multiply(if palette.dark { 0.7 } else { 1.0 })
            },
        );
    }
    let number = Rect::from_center_size(
        pos2(rect.left() + 30.0, rect.center().y),
        vec2(36.0, theme::ROW_HEIGHT),
    );
    let play_button = ui.interact(number, ui.id().with("play"), Sense::click());
    play_button.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            play_index.is_some(),
            format!("Play {title}"),
        )
    });
    if hovered && play_index.is_some() {
        theme::paint_icon(ui, Icon::PlayFilled, number, 14.0, palette.text);
    } else {
        ui.painter().text(
            number.center(),
            Align2::CENTER_CENTER,
            (index + 1).to_string(),
            theme::regular(14.0),
            palette.secondary,
        );
    }
    if play_button.clicked() && !play_button.double_clicked() {
        play(app, &playback);
    }
    let cover =
        Rect::from_center_size(pos2(rect.left() + 74.0, rect.center().y), Vec2::splat(40.0));
    super::super::widgets::paint_cover(
        ui,
        &palette,
        item.and_then(MediaItem::artwork),
        cover,
        4.0,
        Icon::Music,
        Some(app.backend.art()),
    );
    let source_width = if rect.width() > 560.0 { 140.0 } else { 0.0 };
    let left = rect.left() + 104.0;
    let right = (rect.right() - 96.0 - source_width).max(left);
    let color = if play_index.is_some() {
        palette.text
    } else {
        palette.secondary
    };
    crate::bidi::paint_line(
        ui.painter(),
        left,
        right,
        rect.center().y - 9.0,
        title,
        theme::medium(14.0),
        color,
    );
    let source = match &playlist.items[index] {
        MediaRef::Spotify(_) => "Spotify",
        MediaRef::SoundCloud(_) => "SoundCloud",
    };
    let subtitle = match item {
        Some(MediaItem::SoundCloud(track)) => format!(
            "{} • SoundCloud{}",
            track.user.username,
            if track.access == crate::soundcloud::Access::Preview {
                " • Preview only"
            } else {
                ""
            }
        ),
        Some(item) => format!("{} • {}", item.creator(), item.source()),
        None => format!("{source} • Metadata unavailable"),
    };
    crate::bidi::paint_line(
        ui.painter(),
        left,
        right,
        rect.center().y + 10.0,
        &subtitle,
        theme::regular(12.5),
        palette.secondary,
    );
    if source_width > 0.0 {
        let source_rect = Rect::from_min_max(
            pos2(rect.right() - 228.0, rect.top()),
            pos2(rect.right() - 96.0, rect.bottom()),
        );
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(source_rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        if let Some(item) = item {
            if theme::link(&mut child, source, theme::regular(12.5), palette.secondary)
                .on_hover_text(format!("Open on {source}"))
                .clicked()
            {
                let link = item.permalink();
                if link.starts_with("https://") {
                    app.actions.push(Action::OpenUrl(link));
                }
            }
        } else {
            theme::text(&mut child, source, theme::regular(12.5), palette.secondary);
        }
    }
    let duration = item
        .filter(|item| item.duration_ms() > 0)
        .map(|i| {
            let seconds = i.duration_ms() / 1000;
            format!("{}:{:02}", seconds / 60, seconds % 60)
        })
        .unwrap_or_else(|| "--:--".into());
    ui.painter().text(
        pos2(rect.right() - 54.0, rect.center().y),
        Align2::RIGHT_CENTER,
        duration,
        theme::regular(13.0),
        palette.secondary,
    );
    let menu_rect = Rect::from_center_size(
        pos2(rect.right() - 18.0, rect.center().y),
        vec2(36.0, theme::ROW_HEIGHT),
    );
    let more = ui.interact(menu_rect, ui.id().with("more"), Sense::click());
    if hovered || more.has_focus() {
        theme::paint_icon(ui, Icon::Ellipsis, menu_rect, 18.0, palette.secondary);
    }
    egui::Popup::menu(&more)
        .frame(super::super::widgets::menu_frame(&palette))
        .show(|ui| row_menu(app, ui, playlist, index, item));
    egui::Popup::context_menu(&response)
        .frame(super::super::widgets::menu_frame(&palette))
        .show(|ui| row_menu(app, ui, playlist, index, item));
    if response.double_clicked() {
        play(app, &playback);
    }
    if response.clicked() && !play_button.clicked() && !more.clicked() {
        let modifiers = ui.input(|input| input.modifiers);
        let anchor_id = selection.id.with("anchor");
        if modifiers.shift {
            let anchor = ui
                .data(|data| data.get_temp::<usize>(anchor_id))
                .unwrap_or(index);
            selection.rows.extend(anchor.min(index)..=anchor.max(index));
        } else if modifiers.command || modifiers.ctrl {
            selection.set(index, !selected);
        } else {
            selection.rows.clear();
            selection.set(index, true);
        }
        if !modifiers.shift {
            ui.data_mut(|data| data.insert_temp(anchor_id, index));
        }
    }
    if response.drag_started_by(egui::PointerButton::Primary) {
        egui::DragAndDrop::set_payload(
            ui.ctx(),
            LocalDrag {
                playlist: playlist.id,
                index,
                revision: app.integration.local_revision,
            },
        );
    }
    response.on_hover_text(subtitle);
}

fn row_menu(
    app: &mut App,
    ui: &mut egui::Ui,
    playlist: &MixedPlaylist,
    index: usize,
    item: Option<&MediaItem>,
) {
    let palette = app.palette;
    ui.set_min_width(200.0);
    ui.set_max_width(300.0);
    if let Some(item) = item {
        media_menu(app, ui, item);
    }
    super::super::widgets::menu_separator(ui, &palette);
    if super::super::widgets::menu_item_enabled(
        ui,
        &palette,
        Some(Icon::ChevronUp),
        "Move up",
        index > 0,
    ) {
        emit(
            app,
            MixAction::MoveLocal {
                id: playlist.id,
                from: index,
                to: index - 1,
            },
        );
    }
    if super::super::widgets::menu_item_enabled(
        ui,
        &palette,
        Some(Icon::ChevronDown),
        "Move down",
        index + 1 < playlist.items.len(),
    ) {
        emit(
            app,
            MixAction::MoveLocal {
                id: playlist.id,
                from: index,
                to: index + 1,
            },
        );
    }
    if super::super::widgets::menu_item(
        ui,
        &palette,
        Some(Icon::Trash),
        "Remove from this playlist",
    ) {
        emit(
            app,
            MixAction::RemoveLocal {
                id: playlist.id,
                index,
            },
        );
    }
}
