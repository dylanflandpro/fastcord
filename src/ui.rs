//! The three columns: servers, channels (or DMs), and the open conversation.

mod sign_in;

use crate::app::{self, App, Composer, Editing, HistoryStatus, ScrollAnchor, Selection, View};
use crate::backend::{Command, Link};
use crate::markdown::{self, Action, Block, Content, Directory, Span, Style};
use crate::media::{self, Media, Picture, Shown};
use crate::model::{
    self, Attachment, Badge, ChannelKind, Delivery, Embed, EmbedField, Entry, Id, Message, Model,
    ReactionKind,
};
use crate::theme::{self, Icon, Palette};
use egui::cache::{ComputerMut, FrameCache};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Frame, Galley, Margin, Rect, Response,
    Sense, Stroke, Vec2,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

const RAIL_WIDTH: f32 = 72.0;
const GUILD_SIZE: f32 = 48.0;
const ROW_HEIGHT: f32 = 32.0;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    if app.model.is_none() {
        sign_in::show(app, ui);
        return;
    }
    status_bar(app, ui);
    let status = app.selection.channel.map(|c| app.history_status(c));
    let (request, bottom, composed) = {
        let Some(model) = &app.model else {
            return;
        };
        let selection = &mut app.selection;
        rail(selection, model, &palette, ui);
        sidebar(selection, model, &palette, ui);
        let state = &mut app.composer;
        let composed = selection
            .channel
            .map(|channel| (channel, composer(channel, model, state, &palette, ui)));
        let writing = Writing {
            editing: &mut app.composer.editing,
            notes: &app.notes,
        };
        let (request, bottom) = conversation(
            selection,
            model,
            &palette,
            status,
            &mut app.media,
            writing,
            ui,
        );
        (request, bottom, composed)
    };
    app.report_bottom(bottom);
    let clicked =
        ui.data_mut(|d| d.remove_temp::<(Id, model::Emoji)>(egui::Id::new(REACTION_CLICK)));
    if let (Some((message, emoji)), Some(channel)) = (clicked, app.selection.channel) {
        app.toggle_reaction(channel, message, emoji);
    }
    viewer(&mut app.media, &palette, ui);
    confirm_delete(app, &palette, ui);
    if let Some((channel, composed)) = composed {
        if composed.send {
            app.send_draft(channel);
        }
        if composed.edit_last
            && let Some(model) = &app.model
        {
            app.composer.edit_last(model, channel);
        }
    }
    match request {
        Some(Request::Older(channel)) => app.request_history(channel, true),
        Some(Request::Retry(channel)) => app.retry_history(channel),
        Some(Request::Message(channel, id, act)) => act_on(app, channel, id, act, ui),
        None => {}
    }
}

/// What the conversation asks for.
enum Request {
    /// Scrolled to the top: the page before.
    Older(Id),
    Retry(Id),
    /// Something done to one message: channel, id.
    Message(Id, Id, Act),
}

/// What is done to a message from its line.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Act {
    /// On one that failed.
    Resend,
    Discard,
    Edit,
    /// Delete one of mine; `confirmed` skips the question (Shift held).
    Delete {
        confirmed: bool,
    },
    /// In its editor.
    Save,
    CancelEdit,
}

fn act_on(app: &mut App, channel: Id, id: Id, act: Act, ui: &egui::Ui) {
    match act {
        Act::Resend => app.retry_send(channel, id),
        Act::Discard => app.discard_failed(channel, id),
        Act::Edit => {
            if let Some(model) = &app.model {
                app.composer.edit(model, channel, id);
            }
        }
        Act::Delete { confirmed } => app.delete(channel, id, confirmed),
        Act::Save | Act::CancelEdit => {
            match act {
                Act::Save => app.save_edit(),
                _ => app.composer.editing = None,
            }
            // The cursor goes back to the composer.
            ui.data_mut(|d| d.remove::<Id>(egui::Id::new(OPENED)));
        }
    }
}

/// The question before deleting one of my messages, as the official
/// client asks it. Its buttons are keyed by the message, like the toolbar's.
fn confirm_delete(app: &mut App, palette: &Palette, ui: &mut egui::Ui) {
    let Some((channel, id)) = app.confirm_delete else {
        return;
    };
    let mut answer = None;
    let modal = egui::Modal::new(egui::Id::new(("confirm-delete", channel, id)));
    let modal = modal.show(ui.ctx(), |ui| {
        ui.set_width(400.0);
        let title = egui::RichText::new("Delete Message").font(theme::semibold(18.0));
        ui.label(title.color(palette.text));
        ui.add_space(8.0);
        ui.label("Are you sure you want to delete this message?");
        ui.add_space(16.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let delete = egui::RichText::new("Delete").color(palette.danger);
            if keyed_button(ui, ("delete-yes", channel, id), delete).clicked() {
                answer = Some(true);
            }
            if keyed_button(ui, ("delete-no", channel, id), "Cancel").clicked() {
                answer = Some(false);
            }
        });
    });
    match answer {
        Some(true) => app.delete(channel, id, true),
        Some(false) => app.confirm_delete = None,
        None if modal.should_close() => app.confirm_delete = None,
        None => {}
    }
}

/// What the composer was asked this frame.
#[derive(Debug, Default, PartialEq)]
struct Composed {
    /// Enter.
    send: bool,
    /// Up in an empty draft: edit my last message.
    edit_last: bool,
}

/// Where egui's memory keeps the channel whose composer last took the
/// cursor; removing it gives the cursor back to the composer.
const OPENED: &str = "composer-channel";

/// Discord's words for a channel where I may not write.
const NO_PERMISSION: &str = "You do not have permission to send messages in this channel.";

/// The box under the conversation. Enter sends, Shift+Enter starts a new
/// line, Up in an empty draft edits my last message, and the counter shows
/// near the limit, as in the official client.
fn composer(
    channel: Id,
    model: &Model,
    state: &mut Composer,
    palette: &Palette,
    ui: &mut egui::Ui,
) -> Composed {
    let mut composed = Composed::default();
    let margin = Margin {
        left: 16,
        right: 16,
        top: 0,
        bottom: 20,
    };
    egui::Panel::bottom("composer")
        .show_separator_line(false)
        .frame(Frame::new().fill(palette.window).inner_margin(margin))
        .show(ui, |ui| {
            let field = Frame::new()
                .fill(palette.surface)
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::symmetric(16, 11));
            field.show(ui, |ui| {
                ui.set_width(ui.available_width());
                if !model.can_send(channel) {
                    let text = egui::RichText::new(NO_PERMISSION).color(palette.dim);
                    ui.label(text.font(theme::regular(15.0)));
                    return;
                }
                // Why the draft was not sent (a command fastcord lacks).
                if let Some((_, notice)) = state.notice.as_ref().filter(|(c, _)| *c == channel) {
                    let text = egui::RichText::new(notice).font(theme::regular(13.0));
                    ui.label(text.color(palette.warning));
                }
                let draft = state.drafts.entry(channel).or_default();
                let id = egui::Id::new(("composer", channel));
                let hint = egui::RichText::new(model.placeholder(channel)).color(palette.dim);
                // Grows with the draft up to a part of the window, then
                // scrolls.
                let tallest = ui.ctx().content_rect().height() * 0.4;
                // Tab stays here, and types nothing for now (autocomplete
                // will take it): it would move the cursor to the messages'
                // buttons, where Space or Enter click.
                if ui.memory(|m| m.has_focus(id)) {
                    ui.input_mut(|i| i.events.retain(|e| !is_tab(e)));
                }
                let edit = egui::ScrollArea::vertical()
                    .id_salt(id)
                    .max_height(tallest)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let newline =
                            egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter);
                        ui.add(
                            egui::TextEdit::multiline(draft)
                                .id(id)
                                .hint_text(hint)
                                .font(theme::regular(TEXT_SIZE))
                                .text_color(palette.text)
                                .frame(Frame::NONE)
                                .margin(Margin::ZERO)
                                .desired_rows(1)
                                .desired_width(f32::INFINITY)
                                .lock_focus(true)
                                .return_key(newline),
                        )
                    })
                    .inner;
                // Opening a channel puts the cursor in its composer.
                let opened = egui::Id::new(OPENED);
                if ui.data(|d| d.get_temp::<Id>(opened)) != Some(channel) {
                    ui.data_mut(|d| d.insert_temp(opened, channel));
                    edit.request_focus();
                }
                // Only an Enter typed here sends: not the one that opened the
                // channel as the cursor came, nor one held down.
                let held = egui::Id::new(("composer-held", channel));
                let had_focus = ui.data(|d| d.get_temp::<bool>(held)).unwrap_or(false);
                let focused = edit.has_focus();
                composed.send = had_focus && focused && ui.input(|i| enter_sends(&i.events));
                ui.data_mut(|d| d.insert_temp(held, focused));
                let up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
                composed.edit_last = had_focus && focused && up && draft.is_empty();
                if edit.changed() {
                    state.notice = None;
                }
                if let Some(left) = crate::compose::counter(draft) {
                    let color = if left < 0 {
                        palette.danger
                    } else {
                        palette.secondary
                    };
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        ui.label(
                            egui::RichText::new(left.to_string())
                                .font(theme::semibold(13.0))
                                .color(color),
                        );
                    });
                }
            });
        });
    composed
}

/// The connection's state and the account, along the bottom of the window.
/// Demo runs have no account and no bar.
fn status_bar(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let Some(user) = app.account() else {
        return;
    };
    let name = user.display_name().to_owned();
    let link = match app.link {
        Link::Connected => None,
        Link::Connecting => Some("Connecting…"),
        Link::Reconnecting => Some("Connection lost. Reconnecting…"),
    };
    let mut log_out = false;
    let mut previews = app.notification_content();
    egui::Panel::bottom("status")
        .exact_size(28.0)
        .show_separator_line(false)
        .frame(
            Frame::new()
                .fill(palette.panel)
                .inner_margin(Margin::symmetric(12, 0)),
        )
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                if let Some(link) = link {
                    ui.label(
                        egui::RichText::new(link)
                            .font(theme::regular(12.0))
                            .color(palette.warning),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    log_out = ui.small_button("Log out").clicked();
                    ui.checkbox(&mut previews, "Notification previews")
                        .on_hover_text("Show what messages say in desktop notifications");
                    ui.label(
                        egui::RichText::new(name)
                            .font(theme::regular(12.0))
                            .color(palette.secondary),
                    );
                });
            });
        });
    app.set_notification_content(previews);
    if log_out {
        app.send(Command::LogOut);
    }
}

/// The server rail: direct messages first, then each guild.
fn rail(selection: &mut Selection, model: &Model, palette: &Palette, ui: &mut egui::Ui) {
    egui::Panel::left("rail")
        .resizable(false)
        .exact_size(RAIL_WIDTH)
        .show_separator_line(false)
        .frame(
            Frame::new()
                .fill(palette.panel)
                .inner_margin(Margin::symmetric(12, 12)),
        )
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            let now = jiff::Timestamp::now();
            let dms = selection.view == View::DirectMessages;
            let dm_badge = Badge {
                mentions: model.dm_mentions(),
                ..Badge::default()
            };
            if guild_button(
                ui,
                palette,
                GuildMark::Icon(Icon::MessageCircle),
                dms,
                dm_badge,
            )
            .on_hover_text("Direct messages")
            .clicked()
            {
                selection.open_direct_messages(model);
            }
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for guild in &model.guilds {
                    let selected = selection.view == View::Guild(guild.id);
                    if guild_button(
                        ui,
                        palette,
                        GuildMark::Initials(&initials(&guild.name)),
                        selected,
                        model.guild_badge(guild, now),
                    )
                    .on_hover_text(&guild.name)
                    .clicked()
                    {
                        selection.open_guild(model, guild.id);
                    }
                }
            });
        });
}

enum GuildMark<'a> {
    Icon(Icon),
    Initials(&'a str),
}

fn guild_button(
    ui: &mut egui::Ui,
    palette: &Palette,
    mark: GuildMark<'_>,
    selected: bool,
    badge: Badge,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(GUILD_SIZE), Sense::click());
    // Discord's pill on the rail's edge: tall when selected, half that on
    // hover, a dot when unread.
    let pill = if selected {
        Some(40.0)
    } else if response.hovered() {
        Some(20.0)
    } else {
        badge.unread.then_some(8.0)
    };
    if let Some(height) = pill {
        edge_pill(ui, palette, rect, rect.left() - 12.0, height);
    }
    let active = selected || response.hovered();
    // A circle that squares off when selected or hovered, as Discord's does.
    let radius = if active { 14 } else { (GUILD_SIZE / 2.0) as u8 };
    let (fill, ink) = if active {
        (palette.accent, palette.on_accent)
    } else {
        (palette.surface, palette.text)
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(radius), fill);
    match mark {
        GuildMark::Icon(icon) => {
            icon.image(ink, 22.0)
                .paint_at(ui, Rect::from_center_size(rect.center(), Vec2::splat(22.0)));
        }
        GuildMark::Initials(text) => {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                text,
                theme::semibold(15.0),
                ink,
            );
        }
    }
    if badge.mentions > 0 {
        mention_pill(ui, palette, rect.right_bottom(), badge.mentions, true);
    }
    response
}

/// The white mark that sits on a list's left `edge`, half hidden, beside
/// `row`.
fn edge_pill(ui: &egui::Ui, palette: &Palette, row: Rect, edge: f32, height: f32) {
    let clip = ui.clip_rect();
    let mut painter = ui.painter().clone();
    painter.set_clip_rect(Rect::from_x_y_ranges(
        edge..=row.right(),
        row.top().max(clip.top())..=row.bottom().min(clip.bottom()),
    ));
    let pill = Rect::from_center_size(egui::pos2(edge, row.center().y), Vec2::new(8.0, height));
    painter.rect_filled(pill, CornerRadius::same(4), palette.text);
}

/// Discord's red count of unread mentions, its bottom-right corner at
/// `corner`. `ring` circles it in the panel's colour, which cuts it out of
/// a server icon. Returns the pill's rectangle.
fn mention_pill(
    ui: &egui::Ui,
    palette: &Palette,
    corner: egui::Pos2,
    count: u32,
    ring: bool,
) -> Rect {
    let galley = ui.painter().layout_no_wrap(
        model::badge_count(count),
        theme::bold(12.0),
        palette.on_accent,
    );
    let size = Vec2::new((galley.size().x + 10.0).max(16.0), 16.0);
    let pill = Rect::from_min_max(corner - size, corner);
    if ring {
        ui.painter()
            .rect_filled(pill.expand(3.0), CornerRadius::same(11), palette.panel);
    }
    ui.painter()
        .rect_filled(pill, CornerRadius::same(8), palette.danger);
    ui.painter().galley(
        pill.center() - galley.size() / 2.0,
        galley,
        palette.on_accent,
    );
    pill
}

/// Up to three initials, as Discord shows a server without an icon.
fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|word| word.chars().next())
        .take(3)
        .collect()
}

/// The middle column: a guild's channels, or the DM list.
fn sidebar(selection: &mut Selection, model: &Model, palette: &Palette, ui: &mut egui::Ui) {
    egui::Panel::left("sidebar")
        .resizable(true)
        .default_size(240.0)
        .size_range(180.0..=360.0)
        .show_separator_line(false)
        .frame(
            Frame::new()
                .fill(palette.panel)
                .inner_margin(Margin::symmetric(8, 12)),
        )
        .show(ui, |ui| {
            let title = match selection.view {
                View::DirectMessages => "Direct messages",
                View::Guild(id) => model.guild(id).map_or("", |g| g.name.as_str()),
            };
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(title)
                    .font(theme::semibold(15.0))
                    .color(palette.text),
            );
            ui.add_space(8.0);
            ui.separator();
            // One scroll position per list, kept when switching away.
            egui::ScrollArea::vertical()
                .id_salt(selection.view)
                .auto_shrink(false)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let now = jiff::Timestamp::now();
                    match selection.view {
                        View::DirectMessages => {
                            for dm in model.dms_by_recency() {
                                let selected = selection.channel == Some(dm.id);
                                if row(
                                    ui,
                                    palette,
                                    Some(Icon::MessageCircle),
                                    &dm.title(),
                                    selected,
                                    model.dm_badge(dm, now),
                                )
                                .clicked()
                                {
                                    selection.open_channel(dm.id);
                                }
                            }
                        }
                        View::Guild(id) => {
                            let Some(guild) = model.guild(id) else {
                                return;
                            };
                            for entry in guild.sidebar(model.me) {
                                match entry {
                                    Entry::Category(category) => {
                                        ui.add_space(12.0);
                                        ui.label(
                                            egui::RichText::new(category.name.to_uppercase())
                                                .font(theme::semibold(11.0))
                                                .color(palette.dim),
                                        );
                                    }
                                    Entry::Channel(channel) => {
                                        let icon = match channel.kind {
                                            ChannelKind::Voice => Icon::Volume,
                                            ChannelKind::Announcement => Icon::Megaphone,
                                            ChannelKind::Text | ChannelKind::Category => Icon::Hash,
                                        };
                                        let selected = selection.channel == Some(channel.id);
                                        let badge = model.channel_badge(guild, channel, now);
                                        let response = row(
                                            ui,
                                            palette,
                                            Some(icon),
                                            &channel.name,
                                            selected,
                                            badge,
                                        );
                                        // Voice comes after the first version.
                                        if response.clicked() && channel.kind != ChannelKind::Voice
                                        {
                                            selection.open_channel(channel.id);
                                        }
                                    }
                                }
                            }
                        }
                    }
                });
        });
}

fn row(
    ui: &mut egui::Ui,
    palette: &Palette,
    icon: Option<Icon>,
    text: &str,
    selected: bool,
    badge: Badge,
) -> Response {
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());
    // Unread: a white mark on the sidebar's edge and a bright, bolder name.
    // Muted: a dimmed name. As the official client draws them.
    if badge.unread && !selected {
        edge_pill(ui, palette, rect, rect.left() - 8.0, 8.0);
    }
    let fill = if selected {
        Some(palette.surface_active)
    } else if response.hovered() {
        Some(palette.surface_hover)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
    }
    let ink = if selected || badge.unread {
        palette.text
    } else if badge.muted {
        palette.dim
    } else {
        palette.secondary
    };
    let mut right = rect.right() - 8.0;
    if badge.mentions > 0 {
        let corner = egui::pos2(right, rect.center().y + 8.0);
        right = mention_pill(ui, palette, corner, badge.mentions, false).left() - 6.0;
    }
    let mut x = rect.left() + 8.0;
    if let Some(icon) = icon {
        let size = 18.0;
        icon.image(palette.dim, size).paint_at(
            ui,
            Rect::from_min_size(
                egui::pos2(x, rect.center().y - size / 2.0),
                Vec2::splat(size),
            ),
        );
        x += size + 8.0;
    }
    // One line, cut with "…" at the row's edge; the full text shows on hover.
    let font = if badge.unread {
        theme::semibold(14.0)
    } else {
        theme::regular(14.0)
    };
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, ink);
    job.wrap = TextWrapping::truncate_at_width(right - x);
    let galley = ui.painter().layout_job(job);
    let elided = galley.elided;
    let top = rect.center().y - galley.size().y / 2.0;
    ui.painter().galley(egui::pos2(x, top), galley, ink);
    if elided {
        response = response.on_hover_text(text);
    }
    response
}

/// The open channel: its name, then its messages, newest at the bottom.
fn conversation(
    selection: &Selection,
    model: &Model,
    palette: &Palette,
    status: Option<HistoryStatus>,
    media: &mut Media,
    writing: Writing<'_>,
    ui: &mut egui::Ui,
) -> (Option<Request>, Option<Id>) {
    let Some(channel) = selection.channel else {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(palette.window))
            .show(ui, |_| {});
        return (None, None);
    };
    let status = status.unwrap_or(HistoryStatus::Idle);
    let mut request = None;
    // The channel, when its last message is on screen.
    let mut bottom = None;
    egui::Panel::top("conversation-header")
        .exact_size(48.0)
        .show_separator_line(true)
        .frame(
            Frame::new()
                .fill(palette.window)
                .inner_margin(Margin::symmetric(16, 0)),
        )
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(
                    egui::RichText::new(channel_title(model, selection.view, channel))
                        .font(theme::semibold(15.0))
                        .color(palette.text),
                );
            });
        });
    egui::CentralPanel::default()
        .frame(
            Frame::new()
                .fill(palette.window)
                .inner_margin(Margin::symmetric(16, 8)),
        )
        .show(ui, |ui| {
            let Some(messages) = model.messages.get(&channel) else {
                // Nothing loaded yet: the first page is on its way, or failed.
                ui.centered_and_justified(|ui| match status {
                    HistoryStatus::Loading => {
                        ui.spinner();
                    }
                    HistoryStatus::Failed => {
                        if failed(ui, palette, "Couldn't load messages.") {
                            request = Some(Request::Retry(channel));
                        }
                    }
                    HistoryStatus::Idle => {
                        ui.label(egui::RichText::new("No messages yet").color(palette.dim));
                    }
                });
                return;
            };
            let complete = model.complete.contains(&channel);
            let reader = Reader {
                palette,
                clock: Clock::now(),
                names: Directory::new(model, channel),
                revealed: ui
                    .data(|d| d.get_temp::<Arc<RevealedSpoilers>>(egui::Id::new(REVEALED)))
                    .unwrap_or_default(),
            };
            // An older page lands above what is on screen: keep what was on
            // screen in place, as the official client does, instead of
            // jumping to the new page's last message.
            let anchor = egui::Id::new(("history-anchor", channel));
            let mut area = egui::ScrollArea::vertical()
                .id_salt(channel)
                .auto_shrink(false)
                .stick_to_bottom(true);
            if let Some(offset) = ui.data_mut(|d| d.remove_temp::<f32>(anchor.with("offset"))) {
                area = area.vertical_scroll_offset(offset);
            }
            let first = messages.first().map(|m| m.id);
            // One scroll position per channel, so each opens where it was left.
            let output = area.show(ui, |ui| {
                // One row of fixed height for the beginning, the spinner or
                // a failure, so they never change the height the anchor
                // compares.
                let row = egui::vec2(ui.available_width(), TOP_ROW);
                ui.allocate_ui_with_layout(
                    row,
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        ui.set_min_size(row);
                        if complete {
                            beginning(ui, palette, &channel_title(model, selection.view, channel));
                        } else {
                            match status {
                                HistoryStatus::Loading => {
                                    ui.spinner();
                                }
                                HistoryStatus::Failed => {
                                    if failed(ui, palette, "Couldn't load older messages.") {
                                        request = Some(Request::Retry(channel));
                                    }
                                }
                                HistoryStatus::Idle => {}
                            }
                        }
                    },
                );
                if messages.is_empty() && complete {
                    return;
                }
                // Failed messages read in red, as the official client shows them.
                let red = Palette {
                    text: palette.danger,
                    ..*palette
                };
                let failed = |m: &Message| matches!(m.delivery, Delivery::Failed(_));
                let red_reader = messages.iter().any(failed).then(|| Reader {
                    palette: &red,
                    clock: Clock::now(),
                    names: Directory::new(model, channel),
                    revealed: reader.revealed.clone(),
                });
                let mut previous: Option<&Message> = None;
                for message in messages {
                    let body = red_reader.as_ref().filter(|_| failed(message));
                    let body = body.unwrap_or(&reader);
                    let line = Line {
                        mine: model.is_mine(channel, message.id),
                        editing: writing
                            .editing
                            .as_mut()
                            .filter(|e| (e.channel, e.id) == (channel, message.id)),
                        note: writing
                            .notes
                            .get(&(channel, message.id))
                            .map(String::as_str),
                    };
                    let act = message_line(ui, &reader, body, media, previous, message, line);
                    if let Some(act) = act {
                        request = Some(Request::Message(channel, message.id, act));
                    }
                    previous = Some(message);
                }
            });
            let height = output.content_size.y;
            let at_top = output.state.offset.y <= 1.0;
            let viewport = output.inner_rect.height();
            if app::at_bottom(output.state.offset.y, height, viewport) {
                bottom = Some(channel);
            }
            if let Some(before) = ui.data(|d| d.get_temp::<ScrollAnchor>(anchor))
                && let Some(offset) = app::anchored_offset(before, first, height)
            {
                ui.data_mut(|d| d.insert_temp(anchor.with("offset"), offset));
            }
            ui.data_mut(|d| {
                d.insert_temp(
                    anchor,
                    ScrollAnchor {
                        first,
                        height,
                        offset: output.state.offset.y,
                    },
                )
            });
            // At the top, or a first page too short to scroll: the page
            // before, until the channel's first message is in.
            let fills = height > output.inner_rect.height();
            if !complete && status == HistoryStatus::Idle && (at_top || !fills) {
                request = Some(Request::Older(channel));
            }
        });
    (request, bottom)
}

/// The height of the row above the messages: the beginning, a spinner or a
/// failure with its button.
const TOP_ROW: f32 = 56.0;

/// Where a channel's history begins, as the official client marks it.
fn beginning(ui: &mut egui::Ui, palette: &Palette, title: &str) {
    ui.add_space(16.0);
    ui.label(
        egui::RichText::new(format!("This is the beginning of {title}."))
            .font(theme::regular(14.0))
            .color(palette.dim),
    );
    ui.add_space(8.0);
}

/// A failure line with a Try again button; `true` when pressed.
fn failed(ui: &mut egui::Ui, palette: &Palette, text: &str) -> bool {
    ui.vertical_centered(|ui| {
        ui.label(egui::RichText::new(text).color(palette.danger));
        ui.button("Try again").clicked()
    })
    .inner
}

fn channel_title(model: &Model, view: View, channel: Id) -> String {
    match view {
        View::DirectMessages => model.dm(channel).map(|dm| dm.title()).unwrap_or_default(),
        View::Guild(id) => model
            .guild(id)
            .and_then(|g| g.channel(channel))
            .map(|c| format!("# {}", c.name))
            .unwrap_or_default(),
    }
}

/// Whether this frame's keys send the draft: Enter does, Shift+Enter (a
/// new line) and a held Enter repeating do not.
fn enter_sends(events: &[egui::Event]) -> bool {
    events.iter().any(|event| {
        matches!(event, egui::Event::Key { key: egui::Key::Enter, pressed: true, repeat: false, modifiers, .. } if !modifiers.shift)
    })
}

fn is_tab(event: &egui::Event) -> bool {
    matches!(
        event,
        egui::Event::Key {
            key: egui::Key::Tab,
            ..
        }
    )
}

/// How opaque a message's content is drawn: half until Discord has it.
fn opacity(delivery: &Delivery) -> f32 {
    match delivery {
        Delivery::Sending | Delivery::Held(_) | Delivery::Unsure => 0.5,
        Delivery::Sent | Delivery::Failed(_) => 1.0,
    }
}

/// The line under a message that waits: Discord asked to slow down, or its
/// answer was lost.
fn waiting_note(delivery: &Delivery, now: jiff::Timestamp) -> Option<String> {
    match delivery {
        Delivery::Held(until) => {
            let left = until.duration_since(now).as_secs_f64().ceil().max(0.0);
            Some(match left {
                0.0 => "Discord asked to slow down: sending…".into(),
                _ => format!("Discord asked to slow down: sending in {left} s."),
            })
        }
        Delivery::Unsure => Some("Not sure it went through: waiting for Discord to say.".into()),
        _ => None,
    }
}

/// The line under a failed message: Discord's reason, or the official
/// client's words when it gave none (the network was down).
fn failure_text(reason: Option<&str>) -> &str {
    reason.unwrap_or("Message failed to send.")
}

/// A small button whose id comes from `key` (what it acts on), never from
/// its place in the drawing order. egui credits a click to the widget
/// pressed, by id: a layout that shifts between press and release (a page
/// landing above, a message going) must not hand it to another message's.
fn keyed_button(
    ui: &mut egui::Ui,
    key: impl std::hash::Hash + std::fmt::Debug,
    text: impl Into<egui::WidgetText>,
) -> Response {
    // Placed in the row like any widget (a child Ui would not wrap with a
    // wrapped row), then made clickable under its own id, and painted as
    // egui paints a small button.
    let galley = text.into().into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Button,
    );
    let padding = egui::vec2(ui.spacing().button_padding.x, 0.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + 2.0 * padding, Sense::hover());
    // Clicked, never focused: the keyboard must not reach a message's
    // buttons by accident (Tab, then Space).
    let response = ui.interact(rect, egui::Id::new(key), Sense::CLICK);
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let frame = rect.expand(visuals.expansion);
        let painter = ui.painter();
        painter.rect(
            frame,
            visuals.corner_radius,
            visuals.weak_bg_fill,
            visuals.bg_stroke,
            egui::StrokeKind::Inside,
        );
        painter.galley(
            rect.center() - galley.size() / 2.0,
            galley,
            visuals.text_color(),
        );
    }
    response
}

/// Any widget, keyed like [`keyed_button`] but in a child Ui of its own:
/// only outside wrapped rows, where a child Ui would not wrap.
fn keyed(
    ui: &mut egui::Ui,
    key: impl std::hash::Hash + std::fmt::Debug,
    widget: impl egui::Widget,
) -> Response {
    let scope = egui::UiBuilder::new().id(egui::Id::new(key));
    ui.scope_builder(scope, |ui| ui.add(widget)).inner
}

/// What the conversation needs of my writing: the message open for
/// editing, and why my last edits or deletions failed.
struct Writing<'a> {
    editing: &'a mut Option<Editing>,
    notes: &'a HashMap<(Id, Id), String>,
}

/// What a message's line needs to know of my writing.
struct Line<'a> {
    /// Mine, on Discord: I may edit and delete it.
    mine: bool,
    /// Open for editing.
    editing: Option<&'a mut Editing>,
    /// Why my last edit or deletion of it failed.
    note: Option<&'a str>,
}

/// A message, under its author's name when it starts a group. `body` draws
/// its content: red when it failed. One on its way is dimmed; one of mine
/// may be open for editing. Hovered, one of mine offers Edit and Delete, as
/// the official client's toolbar.
fn message_line(
    ui: &mut egui::Ui,
    reader: &Reader<'_>,
    body: &Reader<'_>,
    media: &mut Media,
    previous: Option<&Message>,
    message: &Message,
    line: Line<'_>,
) -> Option<Act> {
    let palette = reader.palette;
    let editing = line.editing.is_some();
    let drawn = ui.scope(|ui| message_body(ui, reader, body, media, previous, message, line));
    // The whole row, as wide as the conversation: a short message is
    // hovered, and its toolbar reached, anywhere along it.
    let row = Rect::from_x_y_ranges(ui.max_rect().x_range(), drawn.response.rect.y_range());
    let offers = !editing && message.delivery == Delivery::Sent && ui.rect_contains_pointer(row);
    let mine = drawn.inner.1;
    let offered = offers
        .then(|| toolbar(ui, palette, row, message.id, mine))
        .flatten();
    drawn.inner.0.or(offered)
}

/// Where a message's toolbar floats: over the top right corner of its row.
fn toolbar_rect(row: Rect, width: f32) -> Rect {
    let size = egui::vec2(width.min(row.width()), 26.0);
    Rect::from_min_size(egui::pos2(row.right() - size.x, row.top()), size)
}

/// The buttons over a hovered message, keyed by it. Shift+Delete skips the
/// question, as in the official client.
fn toolbar(ui: &mut egui::Ui, palette: &Palette, row: Rect, id: Id, mine: bool) -> Option<Act> {
    if !mine {
        return None;
    }
    let area = toolbar_rect(row, 120.0);
    // A child that takes no room: the toolbar floats over the message.
    let layout = egui::Layout::right_to_left(egui::Align::Min);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area).layout(layout));
    let mut act = None;
    let frame = Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(4, 2));
    frame.show(&mut child, |ui| {
        ui.horizontal(|ui| {
            if keyed_button(ui, ("edit", id), "Edit").clicked() {
                act = Some(Act::Edit);
            }
            let delete = egui::RichText::new("Delete").color(palette.danger);
            if keyed_button(ui, ("delete", id), delete).clicked() {
                let confirmed = ui.input(|i| i.modifiers.shift);
                act = Some(Act::Delete { confirmed });
            }
        });
    });
    act
}

/// One of my messages open in place: Enter saves, Shift+Enter breaks the
/// line, Escape cancels, as in the official client. While Discord has not
/// answered, it shows the saved text, saving.
fn editor(ui: &mut egui::Ui, palette: &Palette, editing: &mut Editing) -> Option<Act> {
    let field = Frame::new()
        .fill(palette.surface)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8));
    let newline = egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter);
    let id = editing.id;
    let saving = editing.saving;
    let edit = field
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut editing.draft)
                    .id(egui::Id::new(("editor", id)))
                    .interactive(!saving)
                    .font(theme::regular(TEXT_SIZE))
                    .text_color(palette.text)
                    .frame(Frame::NONE)
                    .margin(Margin::ZERO)
                    .desired_rows(1)
                    .desired_width(f32::INFINITY)
                    .return_key(newline),
            )
        })
        .inner;
    let words = |text: &str| egui::RichText::new(text).font(theme::regular(12.0));
    if saving {
        ui.label(words("Saving…").color(palette.secondary));
        return None;
    }
    // The cursor goes in once, when it opens.
    let open = egui::Id::new("editor-open");
    if ui.data(|d| d.get_temp::<Id>(open)) != Some(id) {
        ui.data_mut(|d| d.insert_temp(open, id));
        edit.request_focus();
    }
    let mut act = None;
    if edit.has_focus() && ui.input(|i| enter_sends(&i.events)) {
        act = Some(Act::Save);
    }
    let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if escape && (edit.has_focus() || edit.lost_focus()) {
        act = Some(Act::CancelEdit);
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.label(words("escape to ").color(palette.secondary));
        let cancel = egui::Button::new(words("cancel").color(palette.accent)).frame(false);
        if keyed(ui, ("edit-cancel", id), cancel).clicked() {
            act = Some(Act::CancelEdit);
        }
        ui.label(words(" • enter to ").color(palette.secondary));
        let save = egui::Button::new(words("save").color(palette.accent)).frame(false);
        if keyed(ui, ("edit-save", id), save).clicked() {
            act = Some(Act::Save);
        }
    });
    if act.is_some() {
        ui.data_mut(|d| d.remove::<Id>(open));
    }
    act
}

/// The inside of a message's line: header, content (or its editor), and
/// what is owed to it; what is asked of it, and whether it is mine.
fn message_body(
    ui: &mut egui::Ui,
    reader: &Reader<'_>,
    body: &Reader<'_>,
    media: &mut Media,
    previous: Option<&Message>,
    message: &Message,
    line: Line<'_>,
) -> (Option<Act>, bool) {
    let palette = reader.palette;
    let mine = line.mine;
    if model::starts_group(previous, message) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(message.author.display_name())
                    .font(theme::semibold(15.0))
                    .color(palette.text),
            );
            ui.label(
                egui::RichText::new(reader.clock.label(model::created_at(message.id)))
                    .font(theme::regular(12.0))
                    .color(palette.dim),
            );
        });
    }
    let mut act = None;
    match line.editing {
        Some(editing) => act = editor(ui, palette, editing),
        None => {
            let body = Body {
                reader: body,
                message: message.id,
                part: 0,
            };
            ui.scope(|ui| {
                ui.multiply_opacity(opacity(&message.delivery));
                body.blocks(ui, &parsed(ui, &message.content));
                attachments(ui, &body, media, &message.attachments);
                embeds(ui, &body, media, &message.embeds);
            });
            if message.edited {
                let text = egui::RichText::new("(edited)").font(theme::regular(11.0));
                ui.label(text.color(palette.dim));
            }
        }
    }
    reactions(ui, palette, message);
    if let Some(note) = line.note {
        let text = egui::RichText::new(note).font(theme::regular(12.0));
        ui.label(text.color(palette.danger));
    }
    if let Some(note) = waiting_note(&message.delivery, reader.clock.now) {
        let text = egui::RichText::new(note).font(theme::regular(12.0));
        ui.label(text.color(palette.secondary));
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    }
    let Delivery::Failed(reason) = &message.delivery else {
        return (act, mine);
    };
    ui.horizontal_wrapped(|ui| {
        let text = egui::RichText::new(failure_text(reason.as_deref()));
        let text = text.font(theme::regular(12.0));
        ui.label(text.color(palette.danger));
        if keyed_button(ui, ("retry", message.id), "Retry").clicked() {
            act = Some(Act::Resend);
        }
        if keyed_button(ui, ("discard", message.id), "Delete").clicked() {
            act = Some(Act::Discard);
        }
    });
    (act, mine)
}

/// Where a click on a reaction waits for `show`, which can change the app.
const REACTION_CLICK: &str = "reaction-click";

/// A message's reactions, in pills under it: the emoji and how many, in the
/// accent colour when one is mine. Clicking a plain one adds mine or takes
/// it back. Super reactions (Nitro) get a pill of their own, outlined in
/// the warning colour, and only show.
fn reactions(ui: &mut egui::Ui, palette: &Palette, message: &Message) {
    if message.reactions.is_empty() {
        return;
    }
    ui.add_space(4.0);
    for (response, reaction, kind) in reaction_row(ui, palette, message) {
        if kind == ReactionKind::Normal && response.clicked() {
            let click = (message.id, reaction.emoji.clone());
            ui.data_mut(|d| d.insert_temp(egui::Id::new(REACTION_CLICK), click));
        }
    }
}

/// Each pill's own id, from what it shows rather than where it is drawn:
/// egui credits a click to the id pressed, so a pill that moves while the
/// pointer is down (a reaction added before it, a message above) must not
/// hand the click to whichever pill takes its place. Snowflakes make the
/// message id unique across channels.
fn pill_id(message: Id, emoji: &model::Emoji, kind: ReactionKind) -> egui::Id {
    let name = emoji.id.is_none().then_some(emoji.name.as_str());
    egui::Id::new((
        "reaction",
        message,
        emoji.id,
        name,
        kind == ReactionKind::Burst,
    ))
}

fn reaction_row<'m>(
    ui: &mut egui::Ui,
    palette: &Palette,
    message: &'m Message,
) -> Vec<(Response, &'m model::Reaction, ReactionKind)> {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(4.0);
        let mut pills = Vec::new();
        for reaction in &message.reactions {
            let shown = [
                (ReactionKind::Normal, reaction.count, reaction.me),
                (ReactionKind::Burst, reaction.burst_count, reaction.me_burst),
            ];
            for (kind, count, mine) in shown.into_iter().filter(|(_, n, _)| *n > 0) {
                let id = pill_id(message.id, &reaction.emoji, kind);
                let response = reaction_pill(ui, palette, id, reaction, kind, count, mine);
                pills.push((response, reaction, kind));
            }
        }
        pills
    })
    .inner
}

/// One pill, laid out in the row (so it wraps with it) but answering to
/// its own stable id.
fn reaction_pill(
    ui: &mut egui::Ui,
    palette: &Palette,
    id: egui::Id,
    reaction: &model::Reaction,
    kind: ReactionKind,
    count: u32,
    mine: bool,
) -> Response {
    const PADDING: Vec2 = Vec2::new(8.0, 3.0);
    let burst = kind == ReactionKind::Burst;
    // Reaction counts are written in full, not shortened like badges.
    let label = format!("{} {count}", reaction.emoji.label());
    let color = if mine {
        palette.text
    } else {
        palette.secondary
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label.clone(), theme::regular(13.0), color);
    let (rect, _) = ui.allocate_exact_size(galley.size() + PADDING * 2.0, Sense::hover());
    // Clicked, never focused: the keyboard must not toggle a reaction by
    // accident (Tab, then Space).
    let sense = if burst { Sense::hover() } else { Sense::CLICK };
    let mut response = ui.interact(rect, id, sense);
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), mine, &label)
    });
    if !burst {
        response = response.on_hover_cursor(CursorIcon::PointingHand);
    }
    let fill = match (mine, !burst && response.hovered()) {
        (true, _) => palette.accent.gamma_multiply(0.15),
        (false, true) => palette.surface_hover,
        (false, false) => palette.surface,
    };
    let stroke = match (burst, mine) {
        (true, _) => palette.warning,
        (false, true) => palette.accent,
        (false, false) => palette.outline,
    };
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let radius = CornerRadius::same(8);
        painter.rect(
            rect,
            radius,
            fill,
            Stroke::new(1.0, stroke),
            egui::StrokeKind::Inside,
        );
        painter.galley(rect.min + PADDING, galley, color);
    }
    response
}

/// A message's embeds: pictures and GIF links on their own, the rest as
/// cards.
fn embeds(ui: &mut egui::Ui, body: &Body<'_>, media: &mut Media, embeds: &[Embed]) {
    for (index, embed) in embeds.iter().enumerate() {
        ui.add_space(4.0);
        match media::standalone(embed) {
            Some(image) => {
                let shown = Picture::embed(image);
                picture(ui, body.reader.palette, media, shown, media::ATTACHMENT_BOX);
            }
            None => {
                let body = Body {
                    part: (index + 1) << 8,
                    ..*body
                };
                embed_card(ui, &body, media, embed);
            }
        }
    }
}

/// Markdown parsed once while it stays on screen, not on every frame.
fn parsed(ui: &egui::Ui, text: &str) -> Arc<[Block]> {
    ui.ctx().memory_mut(|memory| {
        memory
            .caches
            .cache::<FrameCache<Arc<[Block]>, Parse>>()
            .get(text)
            .clone()
    })
}

/// The spoiler part of [`RevealedSpoilers`] for attachments, numbered by
/// their place in the message.
const ATTACHMENTS: usize = usize::MAX;

/// The files sent with a message: pictures scaled to fit, the rest as cards.
/// A picture behind a spoiler shows blurred until clicked, as in the
/// official client.
fn attachments(ui: &mut egui::Ui, body: &Body<'_>, media: &mut Media, files: &[Attachment]) {
    let palette = body.reader.palette;
    let body = Body {
        part: ATTACHMENTS,
        ..*body
    };
    for (index, attachment) in files.iter().enumerate() {
        ui.add_space(4.0);
        if !media::is_image(attachment) {
            file_card(ui, palette, attachment);
            continue;
        }
        let shown = Picture::attachment(attachment);
        if media::is_spoiler(attachment) && !body.revealed()(index) {
            if hidden_picture(ui, palette, media, shown).is_some_and(|r| r.clicked()) {
                body.reveal(ui, index);
            }
        } else {
            picture(ui, palette, media, shown, media::ATTACHMENT_BOX);
        }
    }
}

/// A picture scaled into `bounds`. Clicking it, or activating it from the
/// keyboard, opens it full size. One Discord kept no copy of shows its link.
fn picture(
    ui: &mut egui::Ui,
    palette: &Palette,
    media: &mut Media,
    picture: Picture<'_>,
    bounds: [f32; 2],
) {
    let ppp = ui.ctx().pixels_per_point();
    let shown = [bounds[0].min(ui.available_width()), bounds[1]];
    let Some(response) = draw_picture(ui, palette, media, picture, (bounds, ppp), shown) else {
        return;
    };
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if response.clicked() {
        media.viewing = Some(picture.to_owned());
    }
}

/// A spoiler's picture: a small copy stretched into a blur, darkened, with
/// the official client's "SPOILER" label.
fn hidden_picture(
    ui: &mut egui::Ui,
    palette: &Palette,
    media: &mut Media,
    picture: Picture<'_>,
) -> Option<Response> {
    let bounds = media::ATTACHMENT_BOX;
    let ppp = ui.ctx().pixels_per_point() * media::SPOILER_SCALE;
    let shown = [bounds[0].min(ui.available_width()), bounds[1]];
    let response = draw_picture(ui, palette, media, picture, (bounds, ppp), shown)?;
    let rect = response.rect;
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(4), Color32::from_black_alpha(90));
    let label = painter.layout_no_wrap("SPOILER".into(), theme::semibold(13.0), Color32::WHITE);
    let pill = Rect::from_center_size(rect.center(), label.size() + Vec2::new(20.0, 10.0));
    painter.rect_filled(pill, CornerRadius::same(12), Color32::from_black_alpha(200));
    painter.galley(pill.center() - label.size() / 2.0, label, Color32::WHITE);
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    Some(response)
}

/// Lays out a picture within `shown` points, from the copy `request` asks
/// for (its bounds and pixels per point), its space kept while it loads so
/// the conversation does not jump. Nothing is asked for off screen. `None`
/// when Discord kept no copy, after drawing its link instead.
fn draw_picture(
    ui: &mut egui::Ui,
    palette: &Palette,
    media: &mut Media,
    picture: Picture<'_>,
    (bounds, ppp): ([f32; 2], f32),
    shown: [f32; 2],
) -> Option<Response> {
    if picture.source.is_none() {
        ui.hyperlink(picture.link);
        return None;
    }
    // Only a picture Discord did not measure needs its copy to be sized.
    let mut request = None;
    let mut loaded = None;
    if picture.size.is_none() {
        request = picture.request(bounds, ppp);
        loaded = request
            .as_ref()
            .and_then(|r| media.peek(r))
            .map(|texture| texture.size.into());
    }
    let size = media::drawn_size(picture.size, loaded, shown, ppp);
    let (rect, response) = ui.allocate_exact_size(Vec2::from(size), Sense::click());
    if ui.is_rect_visible(rect) {
        let state = match request.or_else(|| picture.request(bounds, ppp)) {
            Some(request) => media.get(ui.ctx(), request),
            None => Shown::Failed,
        };
        paint_picture(ui, palette, state, rect);
        if response.has_focus() {
            focus_ring(ui, palette, rect);
        }
    }
    Some(response)
}

fn paint_picture(ui: &egui::Ui, palette: &Palette, shown: Shown, rect: Rect) {
    match shown {
        Shown::Ready(texture) => egui::Image::from_texture(texture)
            .corner_radius(CornerRadius::same(4))
            .paint_at(ui, rect),
        Shown::Loading => {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), palette.surface);
        }
        Shown::Failed => {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), palette.surface);
            let side = rect.width().min(rect.height()).min(24.0);
            Icon::Alert
                .image(palette.dim, side)
                .paint_at(ui, Rect::from_center_size(rect.center(), Vec2::splat(side)));
        }
    }
}

fn focus_ring(ui: &egui::Ui, palette: &Palette, rect: Rect) {
    ui.painter().rect_stroke(
        rect.expand(2.0),
        CornerRadius::same(4),
        Stroke::new(1.0, palette.accent),
        egui::StrokeKind::Outside,
    );
}

/// The picture opened full size, over everything, until Esc or a click
/// beside it, with a link to the original as in the official client.
fn viewer(media: &mut Media, palette: &Palette, ui: &mut egui::Ui) {
    let Some(viewing) = media.viewing.clone() else {
        return;
    };
    let ctx = ui.ctx().clone();
    let shown = media::viewer_bounds(ctx.content_rect().size().into());
    let ppp = ctx.pixels_per_point();
    // The largest copy, the same whatever the window's size.
    let bounds = [media::MAX_SIDE as f32 / ppp; 2];
    let modal = egui::Modal::new(egui::Id::new("picture-viewer"))
        .frame(Frame::new())
        .backdrop_color(Color32::from_black_alpha(200))
        .show(&ctx, |ui| {
            let picture = viewing.picture();
            draw_picture(ui, palette, media, picture, (bounds, ppp), shown);
            ui.add_space(8.0);
            ui.hyperlink_to(
                egui::RichText::new("Open in Browser")
                    .font(theme::regular(14.0))
                    .color(Color32::WHITE),
                picture.link,
            );
        });
    if modal.should_close() {
        media.viewing = None;
    }
}

const FILE_CARD_WIDTH: f32 = 432.0;

/// A file that is not a picture, as the official client shows it: icon,
/// name and size. Clicking it opens it in the browser, which downloads it.
fn file_card(ui: &mut egui::Ui, palette: &Palette, attachment: &Attachment) {
    let response = Frame::new()
        .fill(palette.panel)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(FILE_CARD_WIDTH.min(ui.available_width() - 24.0).max(120.0));
            ui.horizontal(|ui| {
                ui.add(Icon::File.image(palette.secondary, 30.0));
                ui.vertical(|ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&attachment.filename)
                                .font(theme::regular(15.0))
                                .color(palette.accent),
                        )
                        .truncate(),
                    );
                    ui.label(
                        egui::RichText::new(media::human_size(attachment.size))
                            .font(theme::regular(12.0))
                            .color(palette.dim),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(Icon::Download.image(palette.secondary, 20.0));
                });
            });
        })
        .response
        .interact(Sense::click());
    if response.has_focus() {
        focus_ring(ui, palette, response.rect);
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if response.clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(&attachment.url));
    }
}

/// A link preview or a bot's embed, as the official client lays it out: a
/// coloured bar, then provider, author, title, description and fields, the
/// thumbnail beside them, the picture and the footer below.
fn embed_card(ui: &mut egui::Ui, body: &Body<'_>, media: &mut Media, embed: &Embed) {
    let palette = body.reader.palette;
    let bar = embed.color.map_or(palette.surface_active, |c| {
        let [_, r, g, b] = c.to_be_bytes();
        Color32::from_rgb(r, g, b)
    });
    let layout = media::embed_layout(ui.available_width(), embed.thumbnail.is_some());
    let padding = media::EMBED_PADDING as i8;
    let response = Frame::new()
        .fill(palette.panel)
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin {
            left: padding,
            right: padding,
            top: 10,
            bottom: 14,
        })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_max_width(layout.text);
                    embed_text(ui, body, embed);
                });
                if let Some(thumbnail) = &embed.thumbnail {
                    ui.add_space(media::THUMBNAIL_GAP);
                    let shown = Picture::embed(thumbnail);
                    picture(ui, palette, media, shown, media::THUMBNAIL_BOX);
                }
            });
            if let Some(image) = &embed.image {
                picture(ui, palette, media, Picture::embed(image), layout.image);
            }
            if let Some(footer) = &embed.footer {
                ui.label(
                    egui::RichText::new(footer)
                        .font(theme::regular(12.0))
                        .color(palette.secondary),
                );
            }
        })
        .response;
    let rect = response.rect;
    ui.painter().rect_filled(
        Rect::from_min_size(rect.min, Vec2::new(4.0, rect.height())),
        CornerRadius {
            nw: 4,
            sw: 4,
            ..CornerRadius::ZERO
        },
        bar,
    );
}

fn embed_text(ui: &mut egui::Ui, body: &Body<'_>, embed: &Embed) {
    let palette = body.reader.palette;
    if let Some(provider) = &embed.provider {
        ui.label(
            egui::RichText::new(provider)
                .font(theme::regular(12.0))
                .color(palette.secondary),
        );
    }
    if let Some(author) = &embed.author {
        ui.label(
            egui::RichText::new(author)
                .font(theme::semibold(14.0))
                .color(palette.text),
        );
    }
    if let Some(title) = &embed.title {
        let text = egui::RichText::new(title).font(theme::semibold(16.0));
        match &embed.url {
            Some(url) => {
                ui.hyperlink_to(text.color(palette.accent), url);
            }
            None => {
                ui.label(text.color(palette.text));
            }
        }
    }
    if let Some(description) = &embed.description {
        body.blocks(ui, &parsed(ui, description));
    }
    embed_fields(ui, body, &embed.fields, embed.thumbnail.is_some());
}

/// The fields in rows, inline ones side by side in equal columns.
fn embed_fields(ui: &mut egui::Ui, body: &Body<'_>, fields: &[EmbedField], thumbnail: bool) {
    let width = ui.available_width();
    let per_row = media::fields_per_row(thumbnail, width);
    for row in media::field_rows(fields, per_row) {
        let column = media::field_width(width, row.len());
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = media::FIELD_GAP;
            for index in row.clone() {
                let field = &fields[index];
                let body = Body {
                    part: body.part + 1 + index,
                    ..*body
                };
                ui.vertical(|ui| {
                    ui.set_width(column);
                    ui.label(
                        egui::RichText::new(&field.name)
                            .font(theme::semibold(14.0))
                            .color(body.reader.palette.text),
                    );
                    body.blocks(ui, &parsed(ui, &field.value));
                });
            }
        });
    }
}

#[derive(Default)]
struct Parse;

impl ComputerMut<&str, Arc<[Block]>> for Parse {
    fn compute(&mut self, content: &str) -> Arc<[Block]> {
        markdown::parse(content).into()
    }
}

/// Where egui's memory keeps the spoilers clicked open: for the session
/// only, never on disk.
const REVEALED: &str = "revealed-spoilers";

/// Each revealed spoiler as (message, part, spoiler index). The part is the
/// message's text (0), embed `e`'s description (`(e + 1) << 8`), its field
/// `f` (that, plus `1 + f`: Discord allows 25 fields), or [`ATTACHMENTS`].
type RevealedSpoilers = HashSet<(Id, usize, usize)>;

const TEXT_SIZE: f32 = 15.0;

/// How often a relative timestamp ("3 minutes ago") is brought up to date.
const RELATIVE_REFRESH: std::time::Duration = std::time::Duration::from_secs(30);

/// What the open channel's messages are drawn with, gathered once per
/// frame.
struct Reader<'a> {
    palette: &'a Palette,
    clock: Clock,
    names: Directory<'a>,
    revealed: Arc<RevealedSpoilers>,
}

/// How a run of text looks before its own markdown applies.
#[derive(Clone, Copy)]
struct Look {
    size: f32,
    color: Color32,
    bold: bool,
}

/// One message's text, or an embed's, drawn block by block as the official
/// client lays it out.
#[derive(Clone, Copy)]
struct Body<'a> {
    reader: &'a Reader<'a>,
    message: Id,
    part: usize,
}

impl Body<'_> {
    fn revealed(&self) -> impl Fn(usize) -> bool + '_ {
        |spoiler| {
            let key = (self.message, self.part, spoiler);
            self.reader.revealed.contains(&key)
        }
    }

    fn reveal(&self, ui: &egui::Ui, spoiler: usize) {
        let key = (self.message, self.part, spoiler);
        ui.data_mut(|d| {
            Arc::make_mut(
                d.get_temp_mut_or_default::<Arc<RevealedSpoilers>>(egui::Id::new(REVEALED)),
            )
            .insert(key);
        });
    }

    fn blocks(&self, ui: &mut egui::Ui, blocks: &[Block]) {
        let palette = self.reader.palette;
        let text = Look {
            size: TEXT_SIZE,
            color: palette.text,
            bold: false,
        };
        for (index, block) in blocks.iter().enumerate() {
            match block {
                Block::Text(spans) => self.text(ui, spans, text),
                Block::Heading(level, spans) => {
                    if index > 0 {
                        ui.add_space(6.0);
                    }
                    let size = match level {
                        1 => 22.0,
                        2 => 19.0,
                        _ => 16.0,
                    };
                    self.text(
                        ui,
                        spans,
                        Look {
                            size,
                            bold: true,
                            ..text
                        },
                    );
                }
                Block::Subtext(spans) => self.text(
                    ui,
                    spans,
                    Look {
                        size: 12.0,
                        color: palette.dim,
                        bold: false,
                    },
                ),
                Block::Code { code, style, .. } => self.code_block(ui, code, style),
                Block::Quote(inner) => {
                    // A bar down the left, as Discord draws quotes.
                    let response = Frame::new()
                        .inner_margin(Margin {
                            left: 16,
                            ..Margin::ZERO
                        })
                        .show(ui, |ui| self.blocks(ui, inner))
                        .response;
                    let rect = response.rect;
                    ui.painter().rect_filled(
                        Rect::from_min_size(rect.min, Vec2::new(4.0, rect.height())),
                        CornerRadius::same(2),
                        palette.dim,
                    );
                }
                Block::List(list) => self.list(ui, list, 0),
            }
        }
    }

    fn list(&self, ui: &mut egui::Ui, list: &markdown::List, depth: usize) {
        for (index, item) in list.items.iter().enumerate() {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(24.0, 20.0), Sense::hover());
                ui.painter().text(
                    egui::pos2(rect.right() - 6.0, rect.top()),
                    Align2::RIGHT_TOP,
                    list.marker(index, depth),
                    theme::regular(TEXT_SIZE),
                    self.reader.palette.secondary,
                );
                ui.vertical(|ui| {
                    for block in item {
                        match block {
                            Block::List(nested) => self.list(ui, nested, depth + 1),
                            other => self.blocks(ui, std::slice::from_ref(other)),
                        }
                    }
                });
            });
        }
    }

    /// A framed monospace block. Behind a spoiler it is a solid block until
    /// clicked, or activated from the keyboard.
    fn code_block(&self, ui: &mut egui::Ui, code: &str, style: &Style) {
        let palette = self.reader.palette;
        let hidden = style.hidden(self.revealed());
        let (fill, color) = match hidden {
            Some(_) => (palette.surface_active, Color32::TRANSPARENT),
            None => (palette.panel, palette.text),
        };
        let response = Frame::new()
            .fill(fill)
            .stroke(Stroke::new(1.0, palette.outline))
            .corner_radius(CornerRadius::same(4))
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let mut job = LayoutJob::default();
                job.wrap.max_width = ui.available_width();
                let line = Stroke::new(1.0, color);
                job.append(
                    code,
                    0.0,
                    TextFormat {
                        font_id: FontId::monospace(13.0),
                        color,
                        italics: style.italic,
                        underline: if style.underline { line } else { Stroke::NONE },
                        strikethrough: if style.strike { line } else { Stroke::NONE },
                        ..TextFormat::default()
                    },
                );
                ui.add(egui::Label::new(job).wrap());
            })
            .response;
        if let Some(spoiler) = hidden {
            let response = response.interact(Sense::click());
            if response.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
            if response.clicked() {
                self.reveal(ui, spoiler);
            }
        }
    }

    /// Spans laid out as one wrapped paragraph. Links open and hidden
    /// spoilers reveal themselves on click; Return or Space does the same
    /// for the focused paragraph when it holds only one of them.
    fn text(&self, ui: &mut egui::Ui, spans: &[Span], look: Look) {
        let mut job = LayoutJob::default();
        job.wrap.max_width = ui.available_width();
        let mut actions: Vec<(Range<usize>, Action)> = Vec::new();
        let mut chars = 0;
        let mut relative = false;
        for span in spans {
            let text = self.span_text(span);
            let count = text.chars().count();
            if let Some(action) = span.style.action(self.revealed()) {
                actions.push((chars..chars + count, action));
            }
            relative |= matches!(
                span.content,
                Content::Timestamp {
                    style: markdown::TimestampStyle::Relative,
                    ..
                }
            );
            chars += count;
            job.append(&text, 0.0, self.format(span, look));
        }
        if relative {
            ui.ctx().request_repaint_after(RELATIVE_REFRESH);
        }
        let galley = ui.painter().layout_job(job);
        let sense = if actions.is_empty() {
            Sense::hover()
        } else {
            Sense::click()
        };
        let (rect, response) = ui.allocate_exact_size(galley.size(), sense);
        let pointed = response
            .hover_pos()
            .and_then(|pos| char_at(&galley, pos - rect.min))
            .and_then(|at| actions.iter().find(|(range, _)| range.contains(&at)))
            .map(|(_, action)| action);
        ui.painter().galley(rect.min, galley, look.color);
        if response.has_focus() {
            ui.painter().rect_stroke(
                rect.expand(2.0),
                CornerRadius::same(3),
                Stroke::new(1.0, self.reader.palette.accent),
                egui::StrokeKind::Outside,
            );
        }
        let keyboard = response.clicked() && !response.clicked_by(egui::PointerButton::Primary);
        let activated = if keyboard {
            let all: Vec<Action> = actions.iter().map(|(_, a)| a.clone()).collect();
            markdown::keyboard_action(&all).cloned()
        } else if response.clicked() {
            pointed.cloned()
        } else {
            None
        };
        if let Some(action) = pointed {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            if let Action::Open(link) = action
                && link.masked
            {
                response.on_hover_text_at_pointer(&link.url);
            }
        }
        match activated {
            Some(Action::Open(link)) => ui.ctx().open_url(egui::OpenUrl::new_tab(link.url)),
            Some(Action::Reveal(spoiler)) => self.reveal(ui, spoiler),
            None => {}
        }
    }

    /// What a span shows. Pills (mentions, inline code, timestamps) get a
    /// narrow space inside each end, the padding Discord gives them.
    fn span_text(&self, span: &Span) -> String {
        let pill = |label: String| format!("\u{202F}{label}\u{202F}");
        let reader = self.reader;
        match &span.content {
            Content::Text(text) => text.clone(),
            Content::Code(code) => pill(code.clone()),
            Content::Mention(mention) => pill(mention.label(&reader.names)),
            Content::Emoji { name, .. } => format!(":{name}:"),
            Content::Timestamp { at, style } => {
                pill(style.label(*at, reader.clock.now, &reader.clock.tz))
            }
        }
    }

    fn format(&self, span: &Span, look: Look) -> TextFormat {
        let palette = self.reader.palette;
        let style = &span.style;
        let font_id = match span.content {
            Content::Code(_) => FontId::monospace(look.size * 0.85),
            _ if look.bold || style.bold => theme::bold(look.size),
            _ => theme::regular(look.size),
        };
        let mut format = TextFormat {
            font_id,
            color: look.color,
            italics: style.italic,
            ..TextFormat::default()
        };
        if style.link.is_some() {
            format.color = palette.accent;
        }
        match span.content {
            Content::Code(_) | Content::Timestamp { .. } => format.background = palette.surface,
            // Discord's mention pill: the accent, faded, under lighter ink.
            Content::Mention(_) => {
                format.background = palette.accent.gamma_multiply(0.3);
                format.color = if palette.dark {
                    palette.accent.lerp_to_gamma(palette.text, 0.6)
                } else {
                    palette.accent
                };
            }
            Content::Text(_) | Content::Emoji { .. } => {}
        }
        let line = Stroke::new(1.0, format.color);
        if style.underline {
            format.underline = line;
        }
        if style.strike {
            format.strikethrough = line;
        }
        if style.hidden(self.revealed()).is_some() {
            // A solid block until clicked.
            format.background = palette.surface_active;
            format.color = Color32::TRANSPARENT;
            format.underline = Stroke::NONE;
            format.strikethrough = Stroke::NONE;
        } else if style.spoiler.is_some() {
            format.background = palette.surface;
        }
        format
    }
}

/// The index of the character under `pos` (relative to the galley), if
/// the pointer is on one rather than beside the text.
fn char_at(galley: &Galley, pos: Vec2) -> Option<usize> {
    let mut chars = 0;
    for row in &galley.rows {
        if (row.rect().top()..row.rect().bottom()).contains(&pos.y) {
            return row
                .glyphs
                .iter()
                .position(|glyph| {
                    let left = row.pos.x + glyph.pos.x;
                    (left..left + glyph.advance_width).contains(&pos.x)
                })
                .map(|index| chars + index);
        }
        chars += row.glyphs.len() + usize::from(row.ends_with_newline);
    }
    None
}

/// The local time zone and date, read once per frame rather than once per
/// message.
struct Clock {
    now: jiff::Timestamp,
    tz: jiff::tz::TimeZone,
    today: jiff::civil::Date,
}

impl Clock {
    fn now() -> Self {
        Self::at(jiff::Timestamp::now(), jiff::tz::TimeZone::system())
    }

    fn at(now: jiff::Timestamp, tz: jiff::tz::TimeZone) -> Self {
        let today = now.to_zoned(tz.clone()).date();
        Self { now, tz, today }
    }

    /// "14:05" today, "07/10/2026 14:05" before.
    fn label(&self, at: jiff::Timestamp) -> String {
        let at = at.to_zoned(self.tz.clone());
        if at.date() == self.today {
            at.strftime("%H:%M").to_string()
        } else {
            at.strftime("%d/%m/%Y %H:%M").to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each pill's label, id and place, drawn `before` widgets down in a
    /// panel `width` wide.
    fn pills(message: &Message, before: usize, width: f32) -> Vec<(String, egui::Id, Rect)> {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 600.0),
            )),
            ..egui::RawInput::default()
        };
        let mut pills = Vec::new();
        let mut output = ctx.run_ui(input, |ui| {
            // Whatever is drawn before moves the pills but must not
            // change their ids.
            for _ in 0..before {
                let _ = ui.button("above");
            }
            for (response, reaction, kind) in reaction_row(ui, &Palette::dark(), message) {
                let label = format!("{} {kind:?}", reaction.emoji.label());
                pills.push((label, response.id, response.rect));
            }
        });
        output.textures_delta.clear();
        pills.sort_by(|a, b| a.0.cmp(&b.0));
        pills
    }

    fn pill_ids(message: &Message, before: usize) -> Vec<(String, egui::Id)> {
        let drawn = pills(message, before, 800.0);
        drawn
            .into_iter()
            .map(|(label, id, _)| (label, id))
            .collect()
    }

    #[test]
    fn reaction_pills_wrap_within_a_narrow_panel() {
        let reaction = |name: &str| model::Reaction {
            emoji: model::Emoji {
                id: Some(1),
                name: name.into(),
                animated: false,
            },
            count: 12,
            ..model::Reaction::default()
        };
        let names = ["party", "laugh", "heart", "rocket", "eyes", "ferris"];
        let message = Message {
            id: 50,
            author: model::User::default(),
            content: String::new(),
            attachments: vec![],
            embeds: vec![],
            reactions: names.map(reaction).to_vec(),
            ..Default::default()
        };
        let drawn = pills(&message, 0, 200.0);
        assert_eq!(drawn.len(), 6);
        for (label, _, rect) in &drawn {
            assert!(rect.right() <= 200.0, "{label} at {rect:?}");
            assert!(rect.width() > rect.height(), "{label} squashed: {rect:?}");
        }
        let rows = drawn.iter().map(|(_, _, r)| r.top() as i32);
        assert!(rows.collect::<HashSet<_>>().len() > 1, "wrapped onto rows");
    }

    #[test]
    fn reaction_pills_keep_their_ids_wherever_they_are_drawn() {
        let reaction = |name: &str, count, burst_count| model::Reaction {
            emoji: model::Emoji {
                id: None,
                name: name.into(),
                animated: false,
            },
            count,
            burst_count,
            ..model::Reaction::default()
        };
        let mut message = Message {
            id: 50,
            author: model::User::default(),
            content: String::new(),
            attachments: vec![],
            embeds: vec![],
            reactions: vec![reaction("🎉", 1, 1), reaction("😂", 2, 0)],
            ..Default::default()
        };
        let first = pill_ids(&message, 0);
        message.reactions.reverse();
        assert_eq!(pill_ids(&message, 3), first);
        assert_eq!(first.len(), 3, "a plain and a super 🎉, a plain 😂");
        message.id = 51;
        assert!(
            pill_ids(&message, 0)
                .iter()
                .all(|(_, id)| !first.iter().any(|(_, f)| f == id))
        );
    }

    #[test]
    fn initials_take_the_first_letter_of_up_to_three_words() {
        assert_eq!(initials("Rust Francophone"), "RF");
        assert_eq!(initials("Omarchy"), "O");
        assert_eq!(initials("a b c d"), "abc");
        assert_eq!(initials("Équipe Ops"), "ÉO");
    }

    #[test]
    fn time_labels_drop_the_date_for_today() {
        let paris = jiff::tz::TimeZone::get("Europe/Paris").unwrap();
        let clock = Clock::at("2026-10-07T20:00:00Z".parse().unwrap(), paris);
        assert_eq!(
            clock.label("2026-10-07T12:05:00Z".parse().unwrap()),
            "14:05"
        );
        // Yesterday in UTC, but already today in Paris.
        assert_eq!(
            clock.label("2026-10-06T22:30:00Z".parse().unwrap()),
            "00:30"
        );
        assert_eq!(
            clock.label("2026-10-06T12:05:00Z".parse().unwrap()),
            "06/10/2026 14:05"
        );
    }

    fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// Draws one frame, headless, with these events. Each key pressed is
    /// released at the end, as a tap: egui counts a key pressed again
    /// before its release as held, repeating.
    fn frame(ctx: &egui::Context, mut events: Vec<egui::Event>, draw: impl FnMut(&mut egui::Ui)) {
        let released: Vec<egui::Event> = events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    repeat: false,
                    modifiers,
                    ..
                } => Some(egui::Event::Key {
                    key: *key,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers: *modifiers,
                }),
                _ => None,
            })
            .collect();
        events.extend(released);
        let input = egui::RawInput {
            events,
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, draw);
        // Nothing renders: the font atlas is never uploaded.
        output.textures_delta.clear();
    }

    /// Draws the composer for one frame, headless, with these events.
    fn compose(
        ctx: &egui::Context,
        model: &Model,
        state: &mut Composer,
        channel: Id,
        events: Vec<egui::Event>,
    ) -> bool {
        compose_all(ctx, model, state, channel, events).send
    }

    fn compose_all(
        ctx: &egui::Context,
        model: &Model,
        state: &mut Composer,
        channel: Id,
        events: Vec<egui::Event>,
    ) -> Composed {
        let mut composed = Composed::default();
        frame(ctx, events, |ui| {
            composed = composer(channel, model, state, &Palette::dark(), ui);
        });
        composed
    }

    #[test]
    fn the_composer_sends_on_enter_and_breaks_lines_on_shift_enter() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let model = crate::demo::model();
        let mut state = Composer::default();
        // Opening the channel focuses the composer: typing goes there.
        assert!(!compose(&ctx, &model, &mut state, 111, vec![]));
        let typed = vec![egui::Event::Text("salut".into())];
        assert!(!compose(&ctx, &model, &mut state, 111, typed));
        let shift = key(egui::Key::Enter, egui::Modifiers::SHIFT);
        assert!(!compose(&ctx, &model, &mut state, 111, vec![shift]));
        assert_eq!(state.drafts[&111], "salut\n");
        let enter = key(egui::Key::Enter, egui::Modifiers::NONE);
        assert!(compose(&ctx, &model, &mut state, 111, vec![enter.clone()]));
        // A notice goes once the draft changes.
        state.notice = Some((111, "Slash commands aren't supported yet.".into()));
        compose(&ctx, &model, &mut state, 111, vec![]);
        assert!(state.notice.is_some());
        compose(
            &ctx,
            &model,
            &mut state,
            111,
            vec![egui::Event::Text("!".into())],
        );
        assert_eq!(state.notice, None);
        // Where I may not write, there is nothing to type in.
        assert!(!compose(&ctx, &model, &mut state, 101, vec![]));
        assert!(!compose(&ctx, &model, &mut state, 101, vec![enter]));
        assert!(!state.drafts.contains_key(&101));
    }

    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        ctx
    }

    /// The demo app on #général, one of its messages with a reaction.
    fn demo_app(ctx: &egui::Context) -> App {
        let mut model = crate::demo::model();
        let emoji = model::Emoji {
            id: None,
            name: "👍".into(),
            animated: false,
        };
        model.messages.get_mut(&111).unwrap()[9].reactions = vec![model::Reaction {
            emoji,
            count: 1,
            ..model::Reaction::default()
        }];
        let mut app = App::new(ctx, Some(model), None);
        app.selection.open_channel(111);
        app
    }

    fn show(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>) {
        frame(ctx, events, |ui| super::show(app, ui));
    }

    fn reactions(app: &App) -> Vec<Vec<model::Reaction>> {
        let messages = app.model.as_ref().unwrap().messages(111);
        messages.iter().map(|m| m.reactions.clone()).collect()
    }

    #[test]
    fn tab_and_space_never_reach_the_messages() {
        let ctx = context();
        let mut app = demo_app(&ctx);
        let before = reactions(&app);
        show(&ctx, &mut app, vec![]);
        show(&ctx, &mut app, vec![egui::Event::Text(":smi".into())]);
        // Tab then Space, again and again: each time, a focused message
        // widget would take Space as a click.
        for _ in 0..8 {
            show(
                &ctx,
                &mut app,
                vec![key(egui::Key::Tab, egui::Modifiers::NONE)],
            );
            let space = key(egui::Key::Space, egui::Modifiers::NONE);
            show(&ctx, &mut app, vec![space, egui::Event::Text(" ".into())]);
        }
        assert_eq!(reactions(&app), before, "no reaction toggled");
        let draft = &app.composer.drafts[&111];
        assert!(
            draft.starts_with(":smi") && !draft.contains('\t'),
            "{draft:?}"
        );
        let composer = egui::Id::new(("composer", 111_u64));
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(composer),
            "the cursor stays"
        );
    }

    #[test]
    fn the_enter_that_opens_a_channel_does_not_send_its_draft() {
        let ctx = context();
        let mut app = demo_app(&ctx);
        app.composer.drafts.insert(112, "brouillon".into());
        show(&ctx, &mut app, vec![]);
        show(&ctx, &mut app, vec![]);
        let count = |app: &App| app.model.as_ref().unwrap().messages(112).len();
        let before = count(&app);
        // The Enter that chose the channel arrives as its composer takes
        // the cursor, and stays down a while, repeating.
        app.selection.open_channel(112);
        for _ in 0..3 {
            show(&ctx, &mut app, vec![held(egui::Key::Enter)]);
        }
        let draft = app.composer.drafts.get(&112).map(String::as_str);
        assert_eq!(draft, Some("brouillon"));
        assert_eq!(count(&app), before);
        // Released, then a fresh Enter: that one sends.
        show(&ctx, &mut app, vec![released(egui::Key::Enter)]);
        show(
            &ctx,
            &mut app,
            vec![key(egui::Key::Enter, egui::Modifiers::NONE)],
        );
        assert_eq!(count(&app), before + 1);
    }

    fn released(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// A key pressed and kept down: egui counts each press after the first
    /// as a repeat.
    fn held(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: true,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn up_in_an_empty_composer_edits_my_last_message() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let model = crate::demo::model();
        let mut state = Composer::default();
        let up = || vec![key(egui::Key::ArrowUp, egui::Modifiers::NONE)];
        compose(&ctx, &model, &mut state, 111, vec![]);
        assert!(compose_all(&ctx, &model, &mut state, 111, up()).edit_last);
        state.drafts.insert(111, "brouillon".into());
        assert!(!compose_all(&ctx, &model, &mut state, 111, up()).edit_last);
    }

    #[test]
    fn the_editor_saves_on_enter_and_cancels_on_escape() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let palette = Palette::dark();
        let mut editing = Editing {
            channel: 111,
            id: 5,
            draft: "salut".into(),
            saving: false,
        };
        let edit = |editing: &mut Editing, events| {
            let mut act = None;
            frame(&ctx, events, |ui| act = editor(ui, &palette, editing));
            act
        };
        assert_eq!(
            edit(&mut editing, vec![]),
            None,
            "opens with the cursor in it"
        );
        assert_eq!(
            edit(&mut editing, vec![egui::Event::Text("!".into())]),
            None
        );
        let enter = || key(egui::Key::Enter, egui::Modifiers::NONE);
        let shift = key(egui::Key::Enter, egui::Modifiers::SHIFT);
        assert_eq!(edit(&mut editing, vec![shift]), None);
        assert_eq!(edit(&mut editing, vec![enter()]), Some(Act::Save));
        assert_eq!(editing.draft, "salut!\n");
        // Saving: nothing more until Discord answers.
        editing.saving = true;
        let typed = vec![egui::Event::Text("x".into()), enter()];
        assert_eq!(edit(&mut editing, typed), None);
        assert_eq!(editing.draft, "salut!\n");
        editing.saving = false;
        assert_eq!(edit(&mut editing, vec![]), None, "opened again");
        let escape = key(egui::Key::Escape, egui::Modifiers::NONE);
        assert_eq!(edit(&mut editing, vec![escape]), Some(Act::CancelEdit));
    }

    #[test]
    fn the_toolbar_sits_on_the_rows_right_end() {
        let row = Rect::from_min_max(egui::pos2(16.0, 100.0), egui::pos2(800.0, 140.0));
        let bar = toolbar_rect(row, 120.0);
        assert_eq!((bar.right(), bar.top(), bar.width()), (800.0, 100.0, 120.0));
        // Never wider than the row.
        let narrow = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(80.0, 20.0));
        assert_eq!(toolbar_rect(narrow, 120.0).left(), 0.0);
    }

    #[test]
    fn a_conversation_draws_edits_failures_and_the_editor() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut model = crate::demo::model();
        let mine = model.messages(111)[8].id;
        let pending = model::next_nonce(0, jiff::Timestamp::now());
        model.add_pending(111, pending, "en route".into());
        model.add_pending(111, pending + 1, "refusé".into());
        model.send_settled(111, pending + 1, Delivery::Failed(None));
        let mut selection = Selection::initial(Some(&model));
        selection.open_channel(111);
        let mut media = Media::new(media::Source::Demo);
        let draft = "Oui".to_owned();
        let mut editing = Some(Editing {
            channel: 111,
            id: mine,
            draft,
            saving: false,
        });
        let notes = HashMap::from([((111, mine), "Couldn't edit this message.".to_owned())]);
        let palette = Palette::dark();
        // Twice, the pointer over the first message the second time.
        for events in [
            vec![],
            vec![egui::Event::PointerMoved(egui::pos2(400.0, 120.0))],
        ] {
            frame(&ctx, events, |ui| {
                let writing = Writing {
                    editing: &mut editing,
                    notes: &notes,
                };
                let (request, _) =
                    conversation(&selection, &model, &palette, None, &mut media, writing, ui);
                assert!(request.is_none());
            });
        }
    }

    #[test]
    fn enter_sends_but_shift_enter_does_not() {
        assert!(enter_sends(&[key(egui::Key::Enter, egui::Modifiers::NONE)]));
        assert!(!enter_sends(&[key(
            egui::Key::Enter,
            egui::Modifiers::SHIFT
        )]));
        assert!(!enter_sends(&[key(egui::Key::A, egui::Modifiers::NONE)]));
        assert!(!enter_sends(&[held(egui::Key::Enter)]));
    }

    #[test]
    fn a_keyed_button_keeps_its_id_whatever_is_drawn_before_it() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let ids = |before: usize| {
            let mut ids = None;
            frame(&ctx, vec![], |ui| {
                for _ in 0..before {
                    let _ = ui.small_button("autre");
                }
                let keyed = keyed_button(ui, ("retry", 5_u64), "Retry").id;
                let plain = ui.small_button("Retry").id;
                ids = Some((keyed, plain));
            });
            ids.unwrap()
        };
        let (keyed, plain) = ids(0);
        let (moved_keyed, moved_plain) = ids(2);
        assert_eq!(keyed, moved_keyed);
        assert_ne!(plain, moved_plain, "an auto id follows the drawing order");
    }

    #[test]
    fn keyed_buttons_wrap_with_their_row() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut rects = None;
        frame(&ctx, vec![], |ui| {
            ui.allocate_ui(egui::vec2(140.0, 200.0), |ui| {
                ui.horizontal_wrapped(|ui| {
                    let bound = ui.max_rect().right();
                    let note = ui.label("Message failed to send.").rect;
                    let retry = keyed_button(ui, ("retry", 5_u64), "Retry").rect;
                    let delete = keyed_button(ui, ("discard", 5_u64), "Delete").rect;
                    rects = Some((bound, note, retry, delete));
                });
            });
        });
        let (bound, note, retry, delete) = rects.unwrap();
        // Narrow: the buttons go to the next line rather than off the edge.
        assert!(retry.right() <= bound + 0.5 && delete.right() <= bound + 0.5);
        assert!(delete.top() > note.top(), "wrapped");
        assert!(
            retry.width() > 10.0 && delete.width() > 10.0,
            "not squashed"
        );
    }

    #[test]
    fn waiting_messages_say_why() {
        let now: jiff::Timestamp = "2026-10-07T12:00:00Z".parse().unwrap();
        let held = |s| Delivery::Held(now + jiff::SignedDuration::from_millis(s));
        let note = |delivery| waiting_note(&delivery, now);
        assert_eq!(
            note(held(12_300)).unwrap(),
            "Discord asked to slow down: sending in 13 s."
        );
        assert_eq!(
            note(held(-1)).unwrap(),
            "Discord asked to slow down: sending…"
        );
        assert!(
            note(Delivery::Unsure)
                .unwrap()
                .starts_with("Not sure it went through")
        );
        assert_eq!(note(Delivery::Sending), None);
        assert_eq!(opacity(&held(0)), 0.5);
        assert_eq!(opacity(&Delivery::Unsure), 0.5);
    }

    #[test]
    fn pending_messages_are_dimmed_and_failures_explained() {
        assert_eq!(opacity(&Delivery::Sending), 0.5);
        assert_eq!(opacity(&Delivery::Failed(None)), 1.0);
        assert_eq!(opacity(&Delivery::Sent), 1.0);
        assert_eq!(failure_text(None), "Message failed to send.");
        assert_eq!(
            failure_text(Some("Slowmode is enabled.")),
            "Slowmode is enabled."
        );
    }
}
