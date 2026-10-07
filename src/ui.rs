//! The three columns: servers, channels (or DMs), and the open conversation.

mod sign_in;

use crate::app::{App, Selection, View};
use crate::model::{self, ChannelKind, Entry, Id, Message, Model};
use crate::theme::{self, Icon, Palette};
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align2, CornerRadius, Frame, Margin, Rect, Response, Sense, Vec2};

const RAIL_WIDTH: f32 = 72.0;
const GUILD_SIZE: f32 = 48.0;
const ROW_HEIGHT: f32 = 32.0;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let Some(model) = &app.model else {
        sign_in::show(app, ui);
        return;
    };
    let selection = &mut app.selection;
    rail(selection, model, &palette, ui);
    sidebar(selection, model, &palette, ui);
    conversation(*selection, model, &palette, ui);
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
                                    selection.channel = Some(dm.id);
                                }
                            }
                        }
                        View::Guild(id) => {
                            let Some(guild) = model.guild(id) else {
                                return;
                            };
                            for entry in guild.sidebar() {
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
                                            selection.channel = Some(channel.id);
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
fn conversation(selection: Selection, model: &Model, palette: &Palette, ui: &mut egui::Ui) {
    let Some(channel) = selection.channel else {
        egui::CentralPanel::default()
            .frame(Frame::new().fill(palette.window))
            .show(ui, |_| {});
        return;
    };
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
            let messages = model.messages(channel);
            if messages.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("No messages yet").color(palette.dim));
                });
                return;
            }
            let clock = Clock::now();
            // One scroll position per channel, so each opens where it was left.
            egui::ScrollArea::vertical()
                .id_salt(channel)
                .auto_shrink(false)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    let mut previous: Option<&Message> = None;
                    for message in messages {
                        message_line(ui, palette, &clock, previous, message);
                        previous = Some(message);
                    }
                });
        });
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
    palette: &Palette,
    clock: &Clock,
    previous: Option<&Message>,
    message: &Message,
) {
    if model::starts_group(previous, message) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(message.author.display_name())
                    .font(theme::semibold(15.0))
                    .color(palette.text),
            );
            ui.label(
                egui::RichText::new(clock.label(model::created_at(message.id)))
                    .font(theme::regular(12.0))
                    .color(palette.dim),
            );
        });
    }
    ui.add(
        egui::Label::new(
            egui::RichText::new(&message.content)
                .font(theme::regular(15.0))
                .color(palette.text),
        )
        .wrap(),
    );
}

/// The local time zone and date, read once per frame rather than once per
/// message.
struct Clock {
    tz: jiff::tz::TimeZone,
    today: jiff::civil::Date,
}

impl Clock {
    fn now() -> Self {
        Self::at(jiff::Timestamp::now(), jiff::tz::TimeZone::system())
    }

    fn at(now: jiff::Timestamp, tz: jiff::tz::TimeZone) -> Self {
        let today = now.to_zoned(tz.clone()).date();
        Self { tz, today }
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
