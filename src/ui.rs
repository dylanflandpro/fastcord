//! The three columns: servers, channels (or DMs), and the open conversation.

mod sign_in;

use crate::app::{App, HistoryStatus, Selection, View};
use crate::backend::{Command, Link};
use crate::markdown::{self, Action, Block, Content, Directory, Span, Style};
use crate::model::{self, ChannelKind, Entry, Id, Message, Model};
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
    let request = {
        let Some(model) = &app.model else {
            return;
        };
        let selection = &mut app.selection;
        rail(selection, model, &palette, ui);
        sidebar(selection, model, &palette, ui);
        conversation(selection, model, &palette, status, ui)
    };
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
                    ui.label(
                        egui::RichText::new(name)
                            .font(theme::regular(12.0))
                            .color(palette.secondary),
                    );
                });
            });
        });
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
            let dms = selection.view == View::DirectMessages;
            if guild_button(ui, palette, GuildMark::Icon(Icon::MessageCircle), dms)
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
) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(GUILD_SIZE), Sense::click());
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
    response
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
                                        let response =
                                            row(ui, palette, Some(icon), &channel.name, selected);
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
) -> Response {
    let (rect, mut response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());
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
    let ink = if selected {
        palette.text
    } else {
        palette.secondary
    };
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
    let mut job = LayoutJob::simple_singleline(text.to_owned(), theme::regular(14.0), ink);
    job.wrap = TextWrapping::truncate_at_width(rect.right() - 8.0 - x);
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
    ui: &mut egui::Ui,
) -> Option<HistoryRequest> {
    let Some(channel) = selection.channel else {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(palette.window))
            .show(ui, |_| {});
        return None;
    };
    let status = status.unwrap_or(HistoryStatus::Idle);
    let mut request = None;
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
            // One scroll position per channel, so each opens where it was left.
            let output = area.show(ui, |ui| {
                if complete {
                    beginning(ui, palette, &channel_title(model, selection.view, channel));
                } else {
                    match status {
                        HistoryStatus::Loading => {
                            ui.vertical_centered(|ui| ui.spinner());
                        }
                        HistoryStatus::Failed => {
                            if failed(ui, palette, "Couldn't load older messages.") {
                                request = Some(HistoryRequest::Retry(channel));
                            }
                        }
                        HistoryStatus::Idle => {}
                    }
                }
                if messages.is_empty() && complete {
                    return;
                }
                let mut previous: Option<&Message> = None;
                for message in messages {
                    message_line(ui, &reader, previous, message);
                    previous = Some(message);
                }
            });
            let height = output.content_size.y;
            let at_top = output.state.offset.y <= 1.0;
            let was = ui.data(|d| d.get_temp::<f32>(anchor));
            if let Some(was) = was
                && height > was
                && at_top
            {
                ui.data_mut(|d| d.insert_temp(anchor.with("offset"), height - was));
            }
            ui.data_mut(|d| d.insert_temp(anchor, height));
            // At the top, or a first page too short to scroll: the page
            // before, until the channel's first message is in.
            let fills = height > output.inner_rect.height();
            if !complete && status == HistoryStatus::Idle && (at_top || !fills) {
                request = Some(HistoryRequest::Older(channel));
            }
        });
    request
}

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
    // Parsed once while the message stays on screen, not on every frame.
    let blocks = ui.ctx().memory_mut(|memory| {
        memory
            .caches
            .cache::<FrameCache<Arc<[Block]>, Parse>>()
            .get(message.content.as_str())
            .clone()
    });
    let body = Body {
        reader,
        message: message.id,
    };
    body.blocks(ui, &blocks);
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

/// Each revealed spoiler as (message, spoiler index).
type RevealedSpoilers = HashSet<(Id, usize)>;

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

/// One message's text, drawn block by block as the official client lays
/// it out.
struct Body<'a> {
    reader: &'a Reader<'a>,
    message: Id,
}

impl Body<'_> {
    fn revealed(&self) -> impl Fn(usize) -> bool + '_ {
        |spoiler| self.reader.revealed.contains(&(self.message, spoiler))
    }

    fn reveal(&self, ui: &egui::Ui, spoiler: usize) {
        let key = (self.message, spoiler);
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
