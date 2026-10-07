//! The window: the model, what is open, and the palette it is drawn in.

use crate::backend::{Backend, Command, Event, Link, Session};
use crate::events::Update;
use crate::media::{self, Media};
use crate::model::{Ack, ChannelKind, Id, Model};
use crate::notify::{self, Attention};
use crate::theme::{self, Catalog, Palette};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Which list the middle column shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum View {
    DirectMessages,
    Guild(Id),
}

/// What is open: the list in the middle column and the conversation. Kept
/// apart from the model so the interface can change it while drawing from
/// the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub view: View,
    pub channel: Option<Id>,
    /// The channel last opened in each guild during this session, which
    /// opening the guild again brings back, as the official client does.
    last_channels: HashMap<Id, Id>,
    /// Whether the person opened the conversation themselves (a channel or
    /// a guild), rather than the window picking one: only then is it read.
    chosen: bool,
}

impl Selection {
    /// Where the window opens: the first guild, or the DMs without one.
    pub fn initial(model: Option<&Model>) -> Self {
        let mut selection = Self {
            view: View::DirectMessages,
            channel: None,
            last_channels: HashMap::new(),
            chosen: false,
        };
        if let Some(model) = model {
            match model.guilds.first() {
                Some(guild) => selection.open_guild(model, guild.id),
                None => selection.open_direct_messages(model),
            }
        }
        selection.chosen = false;
        selection
    }

    /// Opens a guild on the channel last opened there, while it can still be
    /// viewed, or else on its first channel, which then counts as opened.
    pub fn open_guild(&mut self, model: &Model, id: Id) {
        self.view = View::Guild(id);
        let Some(guild) = model.guild(id) else {
            self.channel = None;
            return;
        };
        let last = self
            .last_channels
            .get(&id)
            .and_then(|&c| guild.channel(c))
            .filter(|c| guild.can_view(c, model.me));
        self.channel = last
            .map(|c| c.id)
            .or_else(|| guild.first_text_channel(model.me));
        if let Some(channel) = self.channel {
            self.last_channels.insert(id, channel);
        }
        self.chosen = true;
    }

    /// Opens a conversation from the list in the middle column.
    pub fn open_channel(&mut self, id: Id) {
        self.channel = Some(id);
        self.chosen = true;
        if let View::Guild(guild) = self.view {
            self.last_channels.insert(guild, id);
        }
    }

    /// Keeps the selection pointing at something that still exists after a
    /// change: a guild that went away gives way to the first one, a deleted
    /// or newly hidden channel to its list's first channel.
    pub fn repair(&mut self, model: &Model) {
        match self.view {
            View::Guild(id) => match model.guild(id) {
                None => *self = Self::initial(Some(model)),
                Some(guild) => {
                    let open = self
                        .channel
                        .and_then(|c| guild.channel(c))
                        .is_some_and(|c| guild.can_view(c, model.me));
                    if !open {
                        self.open_guild(model, id);
                        self.chosen = false;
                    }
                }
            },
            View::DirectMessages => {
                if self.channel.and_then(|c| model.dm(c)).is_none() {
                    self.open_direct_messages(model);
                }
            }
        }
    }

    /// Opens the conversation a notification was about, in its guild or
    /// among the DMs, as if the person had picked it.
    /// Only a conversation I can still read: never a voice channel, a
    /// category, or a channel hidden from me since.
    pub fn reveal(&mut self, model: &Model, channel: Id) {
        let readable = |g: &crate::model::Guild| {
            let c = g.channel(channel)?;
            let text = matches!(c.kind, ChannelKind::Text | ChannelKind::Announcement);
            (text && g.can_view(c, model.me)).then_some(View::Guild(g.id))
        };
        self.view = match model.guilds.iter().find_map(readable) {
            Some(view) => view,
            None if model.dm(channel).is_some() => View::DirectMessages,
            None => return,
        };
        self.open_channel(channel);
    }

    /// Shows the DM list on the most recent conversation, which is not
    /// read until the person opens it.
    pub fn open_direct_messages(&mut self, model: &Model) {
        self.view = View::DirectMessages;
        self.channel = model.dms_by_recency().first().map(|d| d.id);
        self.chosen = false;
    }
}

/// How long without a key press or pointer movement before the person
/// counts as away: the web client's idle store uses ten minutes, and its
/// `shouldAutomaticallyAck` refuses to ack while idle.
const IDLE_AFTER: Duration = Duration::from_secs(10 * 60);

/// How close to the end of the conversation counts as at the bottom.
const BOTTOM_SLACK: f32 = 4.0;

/// Whether a conversation scrolled to `offset` shows its last message: the
/// web client acks new messages only then.
pub fn at_bottom(offset: f32, content: f32, viewport: f32) -> bool {
    offset + viewport >= content - BOTTOM_SLACK
}

/// What decides whether the open conversation is read, kept from frame to
/// frame.
struct Reading {
    /// The open channel, when another device marked it unread while open:
    /// the web client leaves it unread until it is left.
    kept_unread: Option<Id>,
    /// The channel whose conversation showed its last message last frame.
    at_bottom: Option<Id>,
    last_input: Instant,
    /// Channels with an ack sent and not yet settled.
    outstanding: HashSet<Id>,
}

impl Reading {
    fn new(now: Instant) -> Self {
        Self {
            kept_unread: None,
            at_bottom: None,
            last_input: now,
            outstanding: HashSet::new(),
        }
    }

    /// Another device marked `channel` unread.
    fn marked_unread(&mut self, channel: Id, open: Option<Id>) {
        if open == Some(channel) {
            self.kept_unread = Some(channel);
        }
    }

    /// Forgets a kept channel once it is no longer open.
    fn follow(&mut self, open: Option<Id>) {
        if self.kept_unread != open {
            self.kept_unread = None;
        }
    }

    fn idle(&self, now: Instant) -> bool {
        now.duration_since(self.last_input) >= IDLE_AFTER
    }
}

/// The read to report for what is on screen, as the official client acks:
/// a conversation the person opened, whose history is shown down to its
/// last message, in a focused window, with someone at it. Not one kept
/// unread.
fn acknowledge(
    model: &mut Model,
    selection: &Selection,
    reading: &Reading,
    focused: bool,
    now: Instant,
) -> Option<Ack> {
    let channel = selection.channel?;
    if !focused
        || reading.idle(now)
        || !selection.chosen
        || reading.kept_unread == Some(channel)
        || reading.at_bottom != Some(channel)
        || !model.messages.contains_key(&channel)
    {
        return None;
    }
    model.mark_read(channel)
}

pub struct App {
    /// `None` until the account is connected.
    pub model: Option<Model>,
    /// Where signing in stands. Demo runs have no backend and stay
    /// [`Session::Checking`], unused, since their model is already there.
    pub session: Session,
    /// The QR code for [`Session::Qr`], built once per code.
    pub qr: Option<qrcode::QrCode>,
    backend: Option<Backend>,
    /// The live connection, while signed in.
    pub link: Link,
    pub selection: Selection,
    /// The pictures in messages, in memory only.
    pub media: Media,
    pub palette: Palette,
    themes: Catalog,
    themes_dir: Option<PathBuf>,
    waker: fastframe_theme::Waker,
    transition: fastframe_theme::Transition,
    /// The palette to draw in once the transition lets it in.
    wanted: Palette,
    /// Whether a desktop palette has arrived yet: the first one is applied at
    /// once, later ones are revealed as Omarchy does.
    first_palette: bool,
    /// History pages on their way, by channel.
    loading_history: HashSet<Id>,
    /// Channels whose last page failed; they wait for Try again.
    failed_history: HashSet<Id>,
    /// Messages that arrived while their channel's first page was loading.
    early_messages: HashMap<Id, Vec<Update>>,
    reading: Reading,
    /// What notifications know of the window, shared with the backend.
    notifications: Arc<notify::Shared>,
    /// A notification was clicked: bring the window forward.
    raise: bool,
}

/// What the conversation shows above or instead of its messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryStatus {
    Idle,
    Loading,
    Failed,
}

/// The page `channel` needs, if any: the latest when nothing is loaded
/// (`Some(None)`), or with `older`, the one before the oldest loaded
/// message until the first message is in.
pub fn history_wanted(model: &Model, channel: Id, older: bool) -> Option<Option<Id>> {
    match model.messages.get(&channel) {
        None => Some(None),
        Some(_) if !older || model.complete.contains(&channel) => None,
        Some(loaded) => model
            .cursors
            .get(&channel)
            .copied()
            .or(loaded.first().map(|m| m.id))
            .map(Some),
    }
}

/// Where the conversation stood after a frame, to keep it in place when an
/// older page lands above what is on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollAnchor {
    /// The oldest message loaded then.
    pub first: Option<Id>,
    pub height: f32,
    pub offset: f32,
}

/// The offset that keeps what was on screen in place, as the official
/// client does: only when the oldest loaded message changed (an older page
/// landed above) does the offset grow by the height added. A message at the
/// bottom, a re-wrap or the first page leave it alone.
pub fn anchored_offset(before: ScrollAnchor, first: Option<Id>, height: f32) -> Option<f32> {
    let older_page = before.first.is_some() && first < before.first;
    (older_page && height > before.height).then_some(before.offset + height - before.height)
}

impl App {
    /// `themes_dir` is where palette files live; `None` keeps the built-in
    /// palette and never reads the desktop.
    pub fn new(ctx: &egui::Context, model: Option<Model>, themes_dir: Option<PathBuf>) -> Self {
        theme::install(ctx);
        let palette = Palette::dark();
        theme::apply(ctx, &palette);

        let mut themes = Catalog::default();
        let repaint = ctx.clone();
        let waker = fastframe_theme::Waker::new(move || repaint.request_repaint());
        if let Some(dir) = &themes_dir {
            theme::enable_desktop_themes(&mut themes);
            themes.start(dir.clone(), None, &waker);
        }

        let notifications = Arc::new(notify::Shared::default());
        let backend = model
            .is_none()
            .then(|| Backend::start(ctx.clone(), notifications.clone()));
        let source = match backend {
            Some(_) => media::Source::Discord,
            None => media::Source::Demo,
        };
        Self {
            media: Media::new(source),
            selection: Selection::initial(model.as_ref()),
            model,
            session: Session::Checking,
            link: Link::Connecting,
            qr: None,
            backend,
            palette,
            themes,
            themes_dir,
            waker,
            transition: fastframe_theme::Transition::default(),
            wanted: palette,
            first_palette: true,
            loading_history: HashSet::new(),
            failed_history: HashSet::new(),
            early_messages: HashMap::new(),
            reading: Reading::new(Instant::now()),
            notifications,
            raise: false,
        }
    }

    /// Whether notifications show what messages say.
    pub fn notification_content(&self) -> bool {
        self.notifications.show_content()
    }

    /// Turning previews off also takes down the notifications already
    /// showing a message's text.
    pub fn set_notification_content(&mut self, show: bool) {
        if self.notifications.show_content()
            && !show
            && let Some(backend) = &self.backend
        {
            backend.notify(notify::Notice::ClearAll);
        }
        self.notifications.set_show_content(show);
    }

    /// Tells notifications what the window shows, after each frame drawn.
    fn report_attention(&self, ctx: &egui::Context) {
        self.notifications.set_attention(Attention {
            focused: ctx.input(|i| i.focused),
            open: self.selection.channel,
        });
    }

    /// Picks up a new desktop palette and reveals it.
    fn follow_theme(&mut self, ctx: &egui::Context) {
        if self.themes.needs_reload()
            && let Some(dir) = &self.themes_dir
        {
            self.themes.start(dir.clone(), None, &self.waker);
        }
        if self.themes.poll() {
            // A scan without a desktop palette (none at all, or one caught
            // half-written mid-switch) keeps the current one rather than
            // flashing the built-in palette.
            if let Some(theme) = self.themes.system_theme() {
                self.wanted = theme.palette;
            }
            if std::mem::replace(&mut self.first_palette, false) {
                self.set_palette(ctx, self.wanted);
            }
        }
        if self.palette != self.wanted {
            self.transition.begin(ctx);
            if !self.transition.holding(ctx) {
                self.set_palette(ctx, self.wanted);
            }
        }
    }

    /// The signed-in account, when there is one (never in demo runs).
    pub fn account(&self) -> Option<&crate::model::User> {
        match &self.session {
            Session::SignedIn(user) if self.backend.is_some() => Some(user),
            _ => None,
        }
    }

    /// Sends a sign-in screen's command (Retry, Log out) and shows the
    /// spinner until the backend answers, so the button cannot be pressed
    /// twice.
    pub fn send(&mut self, command: Command) {
        if self.backend.is_none() {
            return;
        }
        // Logging out takes the account off screen at once, not once
        // Discord and the keyring have answered.
        if matches!(command, Command::LogOut) {
            self.forget_account();
        }
        let session = matches!(command, Command::Retry | Command::LogOut);
        if let Some(backend) = &self.backend {
            backend.send(command);
        }
        if session {
            self.session = Session::Checking;
            self.qr = None;
        }
    }

    /// Drops the account's model and what was on its way for it: requests in
    /// flight die with the session, so nothing would ever clear them.
    fn forget_account(&mut self) {
        self.model = None;
        self.reading.outstanding.clear();
        self.reading.kept_unread = None;
        self.loading_history.clear();
        self.failed_history.clear();
        self.early_messages.clear();
        self.media.clear();
    }

    /// Where the open channel's history stands.
    pub fn history_status(&self, channel: Id) -> HistoryStatus {
        if self.loading_history.contains(&channel) {
            HistoryStatus::Loading
        } else if self.failed_history.contains(&channel) {
            HistoryStatus::Failed
        } else {
            HistoryStatus::Idle
        }
    }

    /// Asks for a page of `channel`'s history when `wanted` says one is
    /// missing and none is on its way. Demo runs have it all already.
    pub fn request_history(&mut self, channel: Id, older: bool) {
        let Some(backend) = &self.backend else {
            return;
        };
        let Some(model) = &self.model else {
            return;
        };
        if self.loading_history.contains(&channel) || self.failed_history.contains(&channel) {
            return;
        }
        if let Some(before) = history_wanted(model, channel, older) {
            let guild = model.guild_of(channel);
            backend.send(Command::LoadHistory {
                channel,
                guild,
                before,
            });
            self.loading_history.insert(channel);
        }
    }

    /// Clears a failure so the next frame asks again.
    pub fn retry_history(&mut self, channel: Id) {
        self.failed_history.remove(&channel);
    }

    fn follow_backend(&mut self) {
        let Some(backend) = &self.backend else {
            return;
        };
        let events: Vec<Event> = backend.events().collect();
        for event in events {
            match event {
                Event::Session(session) => {
                    self.qr = match &session {
                        Session::Qr(url) => qrcode::QrCode::new(url.as_bytes()).ok(),
                        _ => None,
                    };
                    // Signed out, or signing in again: nothing of the last
                    // account stays on screen.
                    if !matches!(session, Session::SignedIn(_)) {
                        self.forget_account();
                    }
                    self.session = session;
                }
                Event::Link(link) => self.link = link,
                Event::Ready(model) => {
                    // A new session after a reconnect missed what happened
                    // meanwhile: histories load again rather than stay
                    // silently incomplete. What was open stays open.
                    match self.model.is_some() {
                        true => self.selection.repair(&model),
                        false => self.selection = Selection::initial(Some(&model)),
                    }
                    self.model = Some(*model);
                }
                Event::AckDone { channel, flags } => {
                    self.reading.outstanding.remove(&channel);
                    if let (Some(model), Some(flags)) = (&mut self.model, flags) {
                        model.save_flags(channel, flags);
                    }
                }
                Event::Open(channel) => {
                    if let Some(model) = &self.model {
                        self.selection.reveal(model, channel);
                        self.raise = true;
                    }
                }
                Event::HistoryFailed { channel } => {
                    self.loading_history.remove(&channel);
                    self.failed_history.insert(channel);
                }
                Event::Update(update) => {
                    // A message arriving while its channel's first page loads
                    // may be newer than the page: keep it for after.
                    if let Update::MessageCreate { channel, .. } = &update
                        && self.loading_history.contains(channel)
                        && self
                            .model
                            .as_ref()
                            .is_some_and(|m| !m.messages.contains_key(channel))
                    {
                        let channel = *channel;
                        self.early_messages.entry(channel).or_default().push(update);
                        continue;
                    }
                    let landed = match &update {
                        Update::History { channel, .. } => Some(*channel),
                        _ => None,
                    };
                    if let Some(channel) = landed {
                        self.loading_history.remove(&channel);
                    }
                    if let Update::Acked {
                        channel,
                        manual: true,
                        ..
                    } = update
                    {
                        self.reading.marked_unread(channel, self.selection.channel);
                    }
                    if let Some(model) = &mut self.model {
                        model.apply(update);
                        if let Some(early) = landed.and_then(|c| self.early_messages.remove(&c)) {
                            early.into_iter().for_each(|update| model.apply(update));
                        }
                        self.selection.repair(model);
                    }
                }
            }
        }
    }

    /// The conversation drawn this frame showed its last message.
    pub fn report_bottom(&mut self, channel: Option<Id>) {
        self.reading.at_bottom = channel;
    }

    /// Marks what is on screen read and tells Discord. Run after drawing,
    /// which is when a conversation opens or reaches its end, with a
    /// repaint to show it read.
    fn read_open_conversation(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if ctx.input(|i| i.events.iter().any(is_input)) {
            self.reading.last_input = now;
        }
        self.reading.follow(self.selection.channel);
        let Some(model) = &mut self.model else {
            return;
        };
        let focused = ctx.input(|i| i.focused);
        let Some(ack) = acknowledge(model, &self.selection, &self.reading, focused, now) else {
            return;
        };
        ctx.request_repaint();
        if let Some(backend) = &self.backend {
            backend.send(Command::Ack(ack));
            self.reading.outstanding.insert(ack.channel);
        }
    }

    /// Drops the acks waiting for channels no longer mine to read (deleted,
    /// or hidden by a role change).
    fn drop_unreadable_acks(&mut self) {
        let (Some(model), Some(backend)) = (&self.model, &self.backend) else {
            return;
        };
        self.reading.outstanding.retain(|&channel| {
            let readable = model.can_read(channel);
            if !readable {
                backend.send(Command::DropAck(channel));
            }
            readable
        });
    }

    fn set_palette(&mut self, ctx: &egui::Context, palette: Palette) {
        self.palette = palette;
        theme::apply(ctx, &palette);
    }
}

/// Something the person did in the window, which shows they are at it.
fn is_input(event: &egui::Event) -> bool {
    matches!(
        event,
        egui::Event::PointerMoved(_)
            | egui::Event::PointerButton { .. }
            | egui::Event::MouseWheel { .. }
            | egui::Event::Key { .. }
            | egui::Event::Text(_)
            | egui::Event::Touch { .. }
    )
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.follow_theme(ctx);
        self.follow_backend();
        if std::mem::take(&mut self.raise) {
            // Wayland compositors may refuse a window that asks for focus
            // without an activation token.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.drop_unreadable_acks();
        if let Some(channel) = self.selection.channel {
            self.request_history(channel, false);
        }
        self.media.poll(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(backend) = self.backend.take() {
            backend.close();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        crate::ui::show(self, ui);
        self.read_open_conversation(ui.ctx());
        self.report_attention(ui.ctx());
        self.transition.paint(ui.ctx());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_asked_for_once_then_page_by_page_to_the_start() {
        let mut model = crate::demo::model();
        let messages = model.messages.remove(&111).unwrap();
        model.complete.clear();
        assert_eq!(history_wanted(&model, 111, false), Some(None));
        let oldest = messages[0].id;
        model.apply(Update::History {
            channel: 111,
            messages,
            oldest: Some(oldest - 5),
            complete: false,
        });
        assert_eq!(history_wanted(&model, 111, false), None);
        // The page before starts below the oldest fetched entry, shown or not.
        assert_eq!(history_wanted(&model, 111, true), Some(Some(oldest - 5)));
        model.complete.insert(111);
        assert_eq!(history_wanted(&model, 111, true), None);
    }

    #[test]
    fn an_older_page_keeps_the_view_in_place() {
        let before = ScrollAnchor {
            first: Some(100),
            height: 1000.0,
            offset: 12.0,
        };
        assert_eq!(anchored_offset(before, Some(40), 1600.0), Some(612.0));
        // A message at the bottom, a re-wrap: nothing moves.
        assert_eq!(anchored_offset(before, Some(100), 1100.0), None);
        // The first page.
        let empty = ScrollAnchor {
            first: None,
            ..before
        };
        assert_eq!(anchored_offset(empty, Some(40), 1600.0), None);
    }

    #[test]
    fn a_page_of_system_messages_still_moves_on() {
        let mut model = crate::demo::model();
        model.messages.remove(&112);
        model.complete.clear();
        model.apply(Update::History {
            channel: 112,
            messages: vec![],
            oldest: Some(500),
            complete: false,
        });
        assert_eq!(history_wanted(&model, 112, true), Some(Some(500)));
    }

    #[test]
    fn opens_on_the_first_guild_and_its_first_text_channel() {
        let model = crate::demo::model();
        let selection = Selection::initial(Some(&model));
        assert_eq!(selection.view, View::Guild(100));
        assert_eq!(selection.channel, Some(101));
    }

    #[test]
    fn switching_lists_opens_a_conversation() {
        let model = crate::demo::model();
        let mut selection = Selection::initial(Some(&model));
        selection.open_guild(&model, 200);
        assert_eq!(selection.channel, Some(201));
        // The DM with the most recent message.
        selection.open_direct_messages(&model);
        assert_eq!(selection.view, View::DirectMessages);
        assert_eq!(selection.channel, Some(900));
    }

    #[test]
    fn a_clicked_notification_opens_its_conversation() {
        let model = crate::demo::model();
        let mut selection = Selection::initial(Some(&model));
        selection.reveal(&model, 201);
        assert_eq!(
            (selection.view, selection.channel),
            (View::Guild(200), Some(201))
        );
        assert!(selection.chosen, "opened as if picked: it is read");
        selection.reveal(&model, 900);
        assert_eq!(selection.view, View::DirectMessages);
        assert_eq!(selection.channel, Some(900));
        selection.reveal(&model, 121);
        assert_eq!(selection.channel, Some(900), "a voice channel");
        selection.reveal(&model, 131);
        assert_eq!(selection.channel, Some(900), "a channel hidden from me");
        selection.reveal(&model, 12345);
        assert_eq!(selection.channel, Some(900), "a channel gone since");
        selection.open_guild(&model, 200);
        assert_eq!(selection.channel, Some(201), "remembered in its guild");
    }

    #[test]
    fn a_guild_reopens_on_its_last_channel() {
        let model = crate::demo::model();
        let mut selection = Selection::initial(Some(&model));
        selection.open_channel(112);
        selection.open_guild(&model, 200);
        assert_eq!(selection.channel, Some(201));
        selection.open_direct_messages(&model);
        selection.open_guild(&model, 100);
        assert_eq!(selection.channel, Some(112));
    }

    #[test]
    fn a_guild_skips_its_last_channel_once_hidden_or_deleted() {
        use crate::model::{Overwrite, OverwriteKind, Permissions};
        let mut model = crate::demo::model();
        let mut selection = Selection::initial(Some(&model));
        selection.open_channel(112);
        let rust = &mut model.guilds[0];
        rust.channels
            .iter_mut()
            .find(|c| c.id == 112)
            .unwrap()
            .overwrites
            .push(Overwrite {
                id: model.me,
                kind: OverwriteKind::Member,
                allow: Permissions::default(),
                deny: Permissions::VIEW_CHANNEL,
            });
        selection.open_guild(&model, 100);
        assert_eq!(selection.channel, Some(101));

        selection.open_channel(113);
        model.guilds[0].channels.retain(|c| c.id != 113);
        selection.open_guild(&model, 100);
        assert_eq!(selection.channel, Some(101));
    }

    #[test]
    fn a_guild_first_opened_remembers_its_first_channel() {
        let mut model = crate::demo::model();
        let mut selection = Selection::initial(Some(&model));
        // A channel ahead of #annonces appears: the open one stays.
        let mut new = model.guilds[0].channel(111).unwrap().clone();
        (new.id, new.parent, new.position) = (99, None, -1);
        model.guilds[0].channels.push(new);
        selection.open_guild(&model, 100);
        assert_eq!(selection.channel, Some(101));
    }

    #[test]
    fn repair_follows_removed_guilds_and_channels() {
        let mut model = crate::demo::model();
        let mut selection = Selection::initial(Some(&model));
        selection.open_guild(&model, 200);
        model.apply(Update::ChannelRemove {
            guild: 200,
            channel: 201,
        });
        selection.repair(&model);
        assert_eq!(selection.channel, Some(202));
        model.apply(Update::GuildRemove(200));
        selection.repair(&model);
        assert_eq!(selection, Selection::initial(Some(&model)));
        selection.open_direct_messages(&model);
        model.apply(Update::DmRemove(900));
        selection.repair(&model);
        assert_eq!(selection.channel, Some(901));
    }

    #[test]
    fn without_an_account_nothing_is_open() {
        let selection = Selection::initial(None);
        assert_eq!(selection.view, View::DirectMessages);
        assert_eq!(selection.channel, None);
    }

    /// Someone at the window, scrolled to the end of `channel`.
    fn watching(channel: Id, now: Instant) -> Reading {
        Reading {
            at_bottom: Some(channel),
            ..Reading::new(now)
        }
    }

    #[test]
    fn only_what_the_person_opened_and_sees_is_read() {
        let now = Instant::now();
        let mut model = crate::demo::model();
        let mut selection = Selection::initial(Some(&model));
        let reading = watching(101, now);
        // The window opened on Rust Francophone by itself.
        assert_eq!(
            acknowledge(&mut model, &selection, &reading, true, now),
            None
        );
        // #général is unread.
        selection.open_channel(111);
        let mut reading = watching(111, now);
        let ack = |model: &mut Model, reading: &Reading, focused, now| {
            acknowledge(model, &selection, reading, focused, now)
        };
        assert_eq!(ack(&mut model, &reading, false, now), None, "unfocused");
        let later = now + IDLE_AFTER;
        assert_eq!(ack(&mut model, &reading, true, later), None, "idle");
        let history = model.messages.remove(&111).unwrap();
        assert_eq!(ack(&mut model, &reading, true, now), None, "not shown");
        model.messages.insert(111, history);
        reading.at_bottom = None;
        assert_eq!(ack(&mut model, &reading, true, now), None, "scrolled up");
        reading.at_bottom = Some(111);
        reading.kept_unread = Some(111);
        assert_eq!(ack(&mut model, &reading, true, now), None, "kept unread");
        reading.kept_unread = None;
        let read = ack(&mut model, &reading, true, now).unwrap();
        assert_eq!((read.channel, read.immediate), (111, true));
        assert_eq!(ack(&mut model, &reading, true, now), None, "once");
    }

    #[test]
    fn the_window_picking_a_conversation_does_not_read_it() {
        let now = Instant::now();
        let mut model = crate::demo::model();
        let mut selection = Selection::initial(Some(&model));
        selection.open_direct_messages(&model);
        selection.channel = Some(901);
        let reading = watching(901, now);
        assert_eq!(
            acknowledge(&mut model, &selection, &reading, true, now),
            None
        );
        selection.open_channel(901);
        assert!(acknowledge(&mut model, &selection, &reading, true, now).is_some());
        // A repair falling back to another channel is the window's pick.
        selection.open_guild(&model, 200);
        assert!(selection.chosen);
        model.apply(Update::ChannelRemove {
            guild: 200,
            channel: 201,
        });
        selection.repair(&model);
        assert_eq!(selection.channel, Some(202));
        assert!(!selection.chosen);
    }

    #[test]
    fn marked_unread_elsewhere_stays_unread_until_left() {
        let now = Instant::now();
        let mut model = crate::demo::model();
        let mut selection = Selection::initial(Some(&model));
        selection.open_channel(111);
        let mut reading = watching(111, now);
        let read = acknowledge(&mut model, &selection, &reading, true, now).unwrap();
        // Another device marks it unread from an older message.
        model.apply(Update::Acked {
            channel: 111,
            message: read.message - 1,
            manual: true,
            mentions: Some(0),
            flags: None,
        });
        reading.marked_unread(111, selection.channel);
        reading.follow(selection.channel);
        assert_eq!(
            acknowledge(&mut model, &selection, &reading, true, now),
            None
        );
        // Left, then opened again: read.
        selection.open_channel(112);
        reading.follow(selection.channel);
        assert_eq!(reading.kept_unread, None);
        selection.open_channel(111);
        assert!(acknowledge(&mut model, &selection, &reading, true, now).is_some());
        // Marked unread while another channel is open: nothing to keep.
        reading.marked_unread(113, selection.channel);
        assert_eq!(reading.kept_unread, None);
    }

    #[test]
    fn the_bottom_allows_a_few_pixels() {
        assert!(at_bottom(600.0, 1000.0, 400.0));
        assert!(at_bottom(597.0, 1000.0, 400.0));
        assert!(!at_bottom(500.0, 1000.0, 400.0));
        // Too short to scroll.
        assert!(at_bottom(0.0, 300.0, 400.0));
    }
}
