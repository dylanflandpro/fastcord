//! The window: the model, what is open, and the palette it is drawn in.

use crate::backend::{Backend, Command, Event, Link, Session};
use crate::events::Update;
use crate::model::{Id, Model};
use crate::theme::{self, Catalog, Palette};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

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
}

impl Selection {
    /// Where the window opens: the first guild, or the DMs without one.
    pub fn initial(model: Option<&Model>) -> Self {
        let mut selection = Self {
            view: View::DirectMessages,
            channel: None,
            last_channels: HashMap::new(),
        };
        if let Some(model) = model {
            match model.guilds.first() {
                Some(guild) => selection.open_guild(model, guild.id),
                None => selection.open_direct_messages(model),
            }
        }
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
    }

    /// Opens a conversation from the list in the middle column.
    pub fn open_channel(&mut self, id: Id) {
        self.channel = Some(id);
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

    pub fn open_direct_messages(&mut self, model: &Model) {
        self.view = View::DirectMessages;
        self.channel = model.dms_by_recency().first().map(|d| d.id);
    }
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
        Some(loaded) => loaded.first().map(|oldest| Some(oldest.id)),
    }
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

        let backend = model.is_none().then(|| Backend::start(ctx.clone()));
        Self {
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
        }
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
        if let Some(backend) = &self.backend {
            // Logging out takes the account off screen at once, not once
            // Discord and the keyring have answered.
            if matches!(command, Command::LogOut) {
                self.model = None;
            }
            backend.send(command);
            self.session = Session::Checking;
            self.qr = None;
        }
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
            backend.send(Command::LoadHistory { channel, before });
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
        for event in backend.events() {
            match event {
                Event::Session(session) => {
                    self.qr = match &session {
                        Session::Qr(url) => qrcode::QrCode::new(url.as_bytes()).ok(),
                        _ => None,
                    };
                    // Signed out, or signing in again: nothing of the last
                    // account stays on screen.
                    if !matches!(session, Session::SignedIn(_)) {
                        self.model = None;
                    }
                    self.session = session;
                }
                Event::Link(link) => self.link = link,
                Event::Ready(mut model) => {
                    // A new session after a reconnect keeps what was open,
                    // and the history already loaded, when it still exists.
                    match self.model.take() {
                        Some(old) => {
                            model.messages = old.messages;
                            self.selection.repair(&model);
                        }
                        None => self.selection = Selection::initial(Some(&model)),
                    }
                    self.model = Some(model);
                }
                Event::HistoryFailed { channel } => {
                    self.loading_history.remove(&channel);
                    self.failed_history.insert(channel);
                }
                Event::Update(update) => {
                    if let Update::History { channel, .. } = &update {
                        self.loading_history.remove(channel);
                    }
                    if let Some(model) = &mut self.model {
                        model.apply(update);
                        self.selection.repair(model);
                    }
                }
            }
        }
    }

    fn set_palette(&mut self, ctx: &egui::Context, palette: Palette) {
        self.palette = palette;
        theme::apply(ctx, &palette);
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.follow_theme(ctx);
        self.follow_backend();
        if let Some(channel) = self.selection.channel {
            self.request_history(channel, false);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        crate::ui::show(self, ui);
        self.transition.paint(ui.ctx());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_asked_for_once_then_page_by_page_to_the_start() {
        let mut model = Model::default();
        assert_eq!(history_wanted(&model, 7, false), Some(None));
        model.apply(Update::History {
            channel: 7,
            messages: crate::demo::model().messages[&111].clone(),
            complete: false,
        });
        assert_eq!(history_wanted(&model, 7, false), None);
        let oldest = model.messages(7)[0].id;
        assert_eq!(history_wanted(&model, 7, true), Some(Some(oldest)));
        model.complete.insert(7);
        assert_eq!(history_wanted(&model, 7, true), None);
        // An empty channel, once loaded, has nothing older either.
        model.apply(Update::History {
            channel: 8,
            messages: vec![],
            complete: false,
        });
        assert_eq!(history_wanted(&model, 8, true), None);
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
}
