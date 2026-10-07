//! The three columns: servers, channels (or DMs), and the open conversation.

mod sign_in;

use crate::app::{self, App, HistoryStatus, ScrollAnchor, Selection, View};
use crate::backend::{Command, Link};
use crate::markdown::{self, Action, Block, Content, Directory, Span, Style};
use crate::media::{self, Media, Picture, Shown};
use crate::model::{
    self, Attachment, Badge, ChannelKind, Embed, EmbedField, Entry, Id, Message, Model,
};
use crate::theme::{self, Icon, Palette};
use egui::cache::{ComputerMut, FrameCache};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Frame, Galley, Margin, Rect, Response,
    Sense, Stroke, Vec2,
};
use std::collections::HashSet;
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
    let (request, bottom) = {
        let Some(model) = &app.model else {
            return;
        };
        let selection = &mut app.selection;
        rail(selection, model, &palette, ui);
        sidebar(selection, model, &palette, ui);
        conversation(selection, model, &palette, status, &mut app.media, ui)
    };
    app.report_bottom(bottom);
    let clicked =
        ui.data_mut(|d| d.remove_temp::<(Id, model::Emoji)>(egui::Id::new(REACTION_CLICK)));
    if let (Some((message, emoji)), Some(channel)) = (clicked, app.selection.channel) {
        app.toggle_reaction(channel, message, emoji);
    }
    viewer(&mut app.media, &palette, ui);
    match request {
        Some(HistoryRequest::Older(channel)) => app.request_history(channel, true),
        Some(HistoryRequest::Retry(channel)) => app.retry_history(channel),
        None => {}
    }
}

/// What the conversation asks of its history.
enum HistoryRequest {
    /// Scrolled to the top: the page before.
    Older(Id),
    Retry(Id),
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
    ui: &mut egui::Ui,
) -> (Option<HistoryRequest>, Option<Id>) {
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
                            request = Some(HistoryRequest::Retry(channel));
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
                                        request = Some(HistoryRequest::Retry(channel));
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
                let mut previous: Option<&Message> = None;
                for message in messages {
                    message_line(ui, &reader, media, previous, message);
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
                request = Some(HistoryRequest::Older(channel));
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

fn message_line(
    ui: &mut egui::Ui,
    reader: &Reader<'_>,
    media: &mut Media,
    previous: Option<&Message>,
    message: &Message,
) {
    let palette = reader.palette;
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
    let body = Body {
        reader,
        message: message.id,
        part: 0,
    };
    body.blocks(ui, &parsed(ui, &message.content));
    attachments(ui, &body, media, &message.attachments);
    embeds(ui, &body, media, &message.embeds);
    reactions(ui, palette, message);
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
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(4.0);
        for reaction in &message.reactions {
            let label = reaction.emoji.label();
            if reaction.count > 0
                && reaction_pill(ui, palette, &label, reaction.count, reaction.me, false)
                    .on_hover_cursor(CursorIcon::PointingHand)
                    .clicked()
            {
                let click = (message.id, reaction.emoji.clone());
                ui.data_mut(|d| d.insert_temp(egui::Id::new(REACTION_CLICK), click));
            }
            if reaction.burst_count > 0 {
                let (count, mine) = (reaction.burst_count, reaction.me_burst);
                reaction_pill(ui, palette, &label, count, mine, true);
            }
        }
    });
}

fn reaction_pill(
    ui: &mut egui::Ui,
    palette: &Palette,
    label: &str,
    count: u32,
    mine: bool,
    burst: bool,
) -> Response {
    let (fill, stroke, text) = match mine {
        true => (
            palette.accent.gamma_multiply(0.15),
            palette.accent,
            palette.text,
        ),
        false => (palette.surface, palette.outline, palette.secondary),
    };
    let stroke = if burst { palette.warning } else { stroke };
    let text = egui::RichText::new(format!("{label} {}", model::badge_count(count)))
        .font(theme::regular(13.0))
        .color(text);
    ui.add(
        egui::Button::new(text)
            .fill(fill)
            .stroke(Stroke::new(1.0, stroke))
            .corner_radius(CornerRadius::same(8)),
    )
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
}
