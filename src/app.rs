//! The window: the model, what is open, and the palette it is drawn in.

use crate::api::Place;
use crate::backend::{Backend, Change, Command, Event, Link, Outgoing, Session, Write};
use crate::compose::Unsent;
use crate::events::Update;
use crate::media::{self, Media};
use crate::model::{
    Ack, ChannelKind, Delivery, Emoji, Id, Message, Model, ReactionRequest, ReplyTo,
};
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
    /// `true` when it opened.
    pub fn reveal(&mut self, model: &Model, channel: Id) -> bool {
        let readable = |g: &crate::model::Guild| {
            let c = g.channel(channel)?;
            let text = matches!(c.kind, ChannelKind::Text | ChannelKind::Announcement);
            (text && g.can_view(c, model.me)).then_some(View::Guild(g.id))
        };
        self.view = match model.guilds.iter().find_map(readable) {
            Some(view) => view,
            None if model.dm(channel).is_some() => View::DirectMessages,
            None => return false,
        };
        self.open_channel(channel);
        true
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
/// Where a request about `channel` is made from.
fn place(model: &Model, channel: Id) -> Place {
    Place {
        channel,
        guild: model.guild_of(channel),
    }
}

/// A message I sent that Discord has not confirmed: kept here as well as
/// in its conversation, which a new READY replaces, so it is never lost.
#[derive(Clone, Debug, PartialEq)]
struct Posted {
    channel: Id,
    content: String,
    /// What it answers, with my ping choice.
    reply: Option<ReplyTo>,
    /// On its way to Discord.
    flying: bool,
    /// Its answer was lost: it may be on Discord.
    unsure: bool,
    /// A new READY took its copy while it was unsure: it waits for its
    /// channel's history to say whether it arrived.
    checking: bool,
}

/// What the composer says of a message put back in the draft that may have
/// gone through after all.
pub const MAYBE_SENT: &str = "A message put back here may already have been sent: check the conversation before sending it again.";

/// What I write: a draft per channel, kept while I look elsewhere as the
/// official client keeps them, the messages Discord has not confirmed yet,
/// and the nonces handed out.
#[derive(Default)]
pub struct Composer {
    pub drafts: HashMap<Id, String>,
    /// Why the channel's draft was not sent, until it changes.
    pub notice: Option<(Id, String)>,
    /// The message each channel's draft answers, until sent or dropped.
    pub replies: HashMap<Id, ReplyTo>,
    /// The message of mine open for editing.
    pub editing: Option<Editing>,
    posted: HashMap<Id, Posted>,
    last_nonce: Id,
}

/// One of my messages, open for editing in place.
#[derive(Debug, PartialEq)]
pub struct Editing {
    pub channel: Id,
    pub id: Id,
    pub draft: String,
    /// Saved, waiting for Discord's answer: as in the official client, the
    /// message changes once Discord has the edit, and the editor closes then.
    pub saving: bool,
}

/// What saving an edit comes to.
#[derive(Debug, PartialEq)]
pub enum Saved {
    /// Nothing to send: unchanged (the editor closes), too long or already
    /// saving (it stays).
    Nothing,
    Edit(Write),
    /// Emptied, with nothing else in it: the official client asks whether
    /// to delete it instead.
    ConfirmDelete(Id, Id),
}

impl Composer {
    /// Sends `channel`'s draft: it shows at once, pending, and the backend
    /// gets what to post. Nothing happens (the draft stays) when the draft
    /// is blank, too long or something fastcord does not do (which the
    /// notice says), when I may not write there, or before the channel's
    /// history is shown.
    pub fn send(
        &mut self,
        model: &mut Model,
        channel: Id,
        now: jiff::Timestamp,
    ) -> Option<Outgoing> {
        let draft = self.drafts.get(&channel)?;
        if !model.can_send(channel) {
            return None;
        }
        let content = match crate::compose::prepare(draft) {
            Ok(content) => content,
            Err(Unsent::Unsupported(why)) => {
                self.notice = Some((channel, why));
                return None;
            }
            Err(Unsent::Blank | Unsent::TooLong) => return None,
        };
        let nonce = crate::model::next_nonce(self.last_nonce, now);
        // A reply to a message deleted meanwhile goes as a plain message.
        let reply = self.replies.get(&channel).copied();
        let reply = reply.filter(|r| model.message(channel, r.message).is_some());
        if !model.add_pending(channel, nonce, content.clone(), reply) {
            return None;
        }
        self.last_nonce = nonce;
        self.drafts.remove(&channel);
        self.replies.remove(&channel);
        self.notice = None;
        Some(self.posting(model, channel, nonce, content, reply))
    }

    /// Keeps a message until Discord confirms it, and what to post.
    fn posting(
        &mut self,
        model: &Model,
        channel: Id,
        nonce: Id,
        content: String,
        reply: Option<ReplyTo>,
    ) -> Outgoing {
        let posted = Posted {
            channel,
            content: content.clone(),
            reply,
            flying: true,
            unsure: false,
            checking: false,
        };
        self.posted.insert(nonce, posted);
        let place = place(model, channel);
        Outgoing {
            place,
            nonce,
            content,
            reply,
        }
    }

    /// Reply on a message: the draft will answer it, pinging its author
    /// unless switched off, as the official client starts a reply. Only
    /// where I may write, and on a message Discord has.
    pub fn reply(&mut self, model: &Model, channel: Id, id: Id) {
        let sent = model
            .message(channel, id)
            .is_some_and(|m| m.delivery == Delivery::Sent);
        if sent && model.can_send(channel) {
            self.replies.insert(
                channel,
                ReplyTo {
                    message: id,
                    ping: true,
                },
            );
        }
    }

    /// Edit on one of my messages: its text opens in place.
    pub fn edit(&mut self, model: &Model, channel: Id, id: Id) -> bool {
        let Some(message) = model
            .message(channel, id)
            .filter(|_| model.is_mine(channel, id))
        else {
            return false;
        };
        let draft = message.content.clone();
        self.editing = Some(Editing {
            channel,
            id,
            draft,
            saving: false,
        });
        true
    }

    /// Up in an empty composer edits my latest message there.
    pub fn edit_last(&mut self, model: &Model, channel: Id) -> bool {
        let empty = self.drafts.get(&channel).is_none_or(String::is_empty);
        empty
            && model
                .last_mine(channel)
                .is_some_and(|id| self.edit(model, channel, id))
    }

    /// Enter in the editor.
    pub fn save_edit(&mut self, model: &Model) -> Saved {
        let Some(editing) = self.editing.as_mut().filter(|e| !e.saving) else {
            return Saved::Nothing;
        };
        let (channel, id) = (editing.channel, editing.id);
        let Some(message) = model.message(channel, id) else {
            self.editing = None;
            return Saved::Nothing;
        };
        // Unchanged comes first: opening and closing a message that is
        // only a picture deletes nothing.
        let Ok(content) = crate::compose::prepare_edit(&editing.draft) else {
            return Saved::Nothing;
        };
        if content == message.content.trim() {
            self.editing = None;
            return Saved::Nothing;
        }
        // Emptied, a message with a file or an embed keeps those.
        if content.is_empty() && message.attachments.is_empty() && message.embeds.is_empty() {
            self.editing = None;
            return Saved::ConfirmDelete(channel, id);
        }
        editing.saving = true;
        // A reply that did not ping stays quiet, as the web client keeps
        // it. Whatever is not known of it (an original not loaded) counts
        // as not pinged: an edit never pings by guess.
        let quiet = message.reply.as_deref().is_some_and(|r| !r.ping);
        Saved::Edit(Write::Edit {
            place: place(model, channel),
            id,
            content,
            quiet,
        })
    }

    /// Discord answered an edit: the editor closes, as in the official
    /// client, whether it saved or not.
    pub fn edit_done(&mut self, channel: Id, id: Id) {
        if self
            .editing
            .as_ref()
            .is_some_and(|e| (e.channel, e.id) == (channel, id))
        {
            self.editing = None;
        }
    }

    /// Retry on a message that failed, while I may still write there.
    pub fn retry(&mut self, model: &mut Model, channel: Id, nonce: Id) -> Option<Outgoing> {
        if !model.can_send(channel) {
            return None;
        }
        // A reply goes again as it went, pinging or not; one whose original
        // is gone meanwhile goes as a plain message, as `send` does.
        let failed = model.message(channel, nonce).and_then(|m| m.reply.as_ref());
        let reply = failed.map(|r| ReplyTo {
            message: r.id,
            ping: r.ping,
        });
        let reply = reply.filter(|r| model.message(channel, r.message).is_some());
        let content = model.resend(channel, nonce)?;
        Some(self.posting(model, channel, nonce, content, reply))
    }

    /// Delete on a message that failed.
    pub fn discard(&mut self, model: &mut Model, channel: Id, nonce: Id) {
        if model.discard(channel, nonce) {
            self.posted.remove(&nonce);
        }
    }

    /// Discord confirmed the message sent with `nonce`.
    pub fn confirmed(&mut self, nonce: Id) {
        self.posted.remove(&nonce);
    }

    /// What became of a message, shown on its copy. Without one (a new
    /// READY replaced the conversation), it shows again; before the
    /// conversation is back, its text returns to the channel's draft.
    pub fn settled(&mut self, model: &mut Model, channel: Id, nonce: Id, delivery: Delivery) {
        let Some(posted) = self.posted.get_mut(&nonce) else {
            model.send_settled(channel, nonce, delivery);
            return;
        };
        posted.flying = delivery.in_flight();
        posted.unsure = delivery == Delivery::Unsure;
        if model.send_settled(channel, nonce, delivery.clone()) {
            return;
        }
        if model.add_pending(channel, nonce, posted.content.clone(), posted.reply) {
            model.send_settled(channel, nonce, delivery);
        } else if !posted.flying
            && let Some(posted) = self.posted.remove(&nonce)
        {
            self.restore(posted);
        }
    }

    /// A new READY replaced the conversations and the copies in them: what
    /// is no longer on its way goes back to its draft, oldest first.
    pub fn after_ready(&mut self) {
        let mut landed: Vec<Id> = self
            .posted
            .iter()
            .filter(|(_, p)| !p.flying && !p.checking)
            .map(|(&n, _)| n)
            .collect();
        landed.sort_unstable();
        for nonce in landed {
            let Some(posted) = self.posted.get_mut(&nonce) else {
                continue;
            };
            // Maybe on Discord: the reloaded history will say.
            if posted.unsure {
                posted.checking = true;
            } else if let Some(posted) = self.posted.remove(&nonce) {
                self.restore(posted);
            }
        }
    }

    /// `channel`'s history is back: an unsure message whose copy went with
    /// a READY is there, by me with its text, or it goes back to the draft
    /// with a word that it may have been sent after all.
    pub fn history_loaded(&mut self, model: &Model, channel: Id) {
        let mut checked: Vec<Id> = self
            .posted
            .iter()
            .filter(|(_, p)| p.checking && p.channel == channel)
            .map(|(&n, _)| n)
            .collect();
        checked.sort_unstable();
        for nonce in checked {
            let Some(posted) = self.posted.remove(&nonce) else {
                continue;
            };
            let mine = model
                .messages(channel)
                .iter()
                .filter(|m| m.author.id == model.me);
            if mine
                .into_iter()
                .any(|m| m.delivery == Delivery::Sent && m.content == posted.content)
            {
                continue;
            }
            self.restore(posted);
            self.notice = Some((channel, MAYBE_SENT.into()));
        }
    }

    fn restore(&mut self, posted: Posted) {
        let draft = self.drafts.entry(posted.channel).or_default();
        if !draft.is_empty() {
            draft.push('\n');
        }
        draft.push_str(&posted.content);
        // It answered a message: the draft answers it again, as it did.
        if let Some(reply) = posted.reply {
            self.replies.entry(posted.channel).or_insert(reply);
        }
    }

    fn next_id(&mut self, now: jiff::Timestamp) -> Id {
        self.last_nonce = crate::model::next_nonce(self.last_nonce, now);
        self.last_nonce
    }
}

/// Whether a message whose answer was lost may be retried: once the gateway
/// showed it was alive after the loss. A resume replays what it missed
/// before RESUMED, and a heartbeat's acknowledgement comes after what was
/// already on its way: had the message arrived, its copy would be here.
pub fn unsure_settled(since: Instant, alive_at: Option<Instant>) -> bool {
    alive_at.is_some_and(|at| at > since)
}

/// How long a demo message takes to "reach Discord": long enough to see it
/// pending.
const DEMO_DELIVERY: std::time::Duration = std::time::Duration::from_millis(700);

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
    pub composer: Composer,
    /// Demo runs have no Discord: what they send arrives here, by when.
    demo_outbox: Vec<(std::time::Instant, Outgoing)>,
    /// My message whose deletion waits for a yes, as channel and id.
    pub confirm_delete: Option<(Id, Id)>,
    /// Why an edit or a deletion of mine did not go through, under the
    /// message, by channel and id, until the next try on it.
    pub notes: HashMap<(Id, Id), String>,
    /// Who is typing where, and when to say that I am.
    pub typing: crate::typing::Others,
    my_typing: crate::typing::Mine,
    /// Messages whose answer was lost, as channel and nonce, since when.
    unsure: HashMap<(Id, Id), Instant>,
    /// When the gateway last showed it was alive (connected, or answered a
    /// heartbeat), while it is connected.
    alive_at: Option<Instant>,
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
            composer: Composer::default(),
            demo_outbox: Vec::new(),
            unsure: HashMap::new(),
            alive_at: None,
            confirm_delete: None,
            notes: HashMap::new(),
            typing: Default::default(),
            my_typing: Default::default(),
        }
    }

    /// Adds my reaction to a message, or removes it if it is there: at once
    /// on screen, then on Discord. Demo runs only change what is shown.
    /// Nothing checks permissions first: joining an existing reaction needs
    /// only Read Message History (Add Reactions is for a new emoji, which
    /// fastcord does not offer), and Discord's refusal undoes it.
    pub fn toggle_reaction(&mut self, channel: Id, message: Id, emoji: Emoji) {
        let Some(model) = &mut self.model else {
            return;
        };
        let Some(add) = model.toggle_reaction(channel, message, &emoji) else {
            return;
        };
        if let Some(backend) = &self.backend {
            backend.send(Command::React(ReactionRequest {
                channel,
                guild: model.guild_of(channel),
                message,
                emoji,
                add,
            }));
        }
    }

    /// Whether notifications show what messages say.
    pub fn notification_content(&self) -> bool {
        self.notifications.show_content()
    }

    /// Turning previews off also takes down the notifications already
    /// showing a message's text.
    pub fn set_notification_content(&mut self, show: bool) {
        let was = self.notifications.show_content();
        // Stored first: a message arriving meanwhile is not shown in full.
        self.notifications.set_show_content(show);
        if was
            && !show
            && let Some(backend) = &self.backend
        {
            backend.notify(notify::Notice::ClearAll);
        }
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
        self.composer = Composer::default();
        self.unsure.clear();
        self.confirm_delete = None;
        self.notes.clear();
        self.typing = Default::default();
        self.my_typing.stop();
    }

    /// Enter in the composer.
    pub fn send_draft(&mut self, channel: Id) {
        let Some(model) = &mut self.model else {
            return;
        };
        if let Some(outgoing) = self.composer.send(model, channel, jiff::Timestamp::now()) {
            self.my_typing.stop();
            self.post(outgoing);
        }
    }

    /// The draft changed: typing goes on, or stops once it is empty.
    pub fn typed(&mut self, channel: Id) {
        let empty = self
            .composer
            .drafts
            .get(&channel)
            .is_none_or(String::is_empty);
        match empty {
            true => self.my_typing.stop(),
            false => self.my_typing.keystroke(channel, Instant::now()),
        }
    }

    /// Tells Discord I am typing when it is due (not in a crowd, as the web
    /// client), and wakes up for the next change in who is typing.
    fn follow_typing(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.my_typing.follow(self.selection.channel);
        self.typing.prune(now);
        if let Some(channel) = self.my_typing.due(now)
            && let (Some(model), Some(backend)) = (&self.model, &self.backend)
            && self.typing.who(channel, now).len() <= crate::typing::CROWD
        {
            backend.send(Command::Typing(place(model, channel)));
        }
        let next = [self.typing.next_change(now), self.my_typing.next_change()];
        if let Some(next) = next.into_iter().flatten().min() {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
    }

    /// Keeps who is typing: someone starts (never me), or their message
    /// arrives. `true` when the update was only about typing.
    fn follow_typists(&mut self, update: &Update) -> bool {
        let now = Instant::now();
        match update {
            Update::TypingStart { channel, user } => {
                if let Some(model) = &mut self.model
                    && user.id != model.me
                {
                    self.typing.start(*channel, user.id, now);
                    // Someone who has not written yet still gets a name.
                    model.users.entry(user.id).or_insert_with(|| user.clone());
                }
                true
            }
            Update::MessageCreate {
                channel, message, ..
            } => {
                self.typing.stop(*channel, message.author.id);
                false
            }
            _ => false,
        }
    }

    /// Enter in a message's editor.
    pub fn save_edit(&mut self) {
        let Some(model) = &self.model else {
            return;
        };
        match self.composer.save_edit(model) {
            Saved::Nothing => {}
            Saved::Edit(write) => self.write(write),
            Saved::ConfirmDelete(channel, id) => self.confirm_delete = Some((channel, id)),
        }
    }

    /// Delete on one of my messages: asked first, unless `confirmed` (the
    /// dialog's button, or Shift held, as in the official client). It goes
    /// once Discord has deleted it.
    pub fn delete(&mut self, channel: Id, id: Id, confirmed: bool) {
        self.confirm_delete = None;
        let Some(model) = &self.model else {
            return;
        };
        if !model.is_mine(channel, id) {
            return;
        }
        if confirmed {
            let place = place(model, channel);
            self.write(Write::Delete { place, id });
        } else {
            self.confirm_delete = Some((channel, id));
        }
    }

    /// Discord answered an edit or a deletion of mine.
    fn changed(&mut self, channel: Id, id: Id, change: Change, result: Result<(), Option<String>>) {
        if change == Change::Edit {
            self.composer.edit_done(channel, id);
        }
        let Err(reason) = result else {
            return;
        };
        let what = match change {
            Change::Edit => "Couldn't edit this message",
            Change::Delete => "Couldn't delete this message",
        };
        let note = match reason {
            Some(reason) => format!("{what}: {reason}"),
            None => format!("{what}."),
        };
        self.notes.insert((channel, id), note);
    }

    /// Retry on a message that failed.
    pub fn retry_send(&mut self, channel: Id, nonce: Id) {
        let Some(model) = &mut self.model else {
            return;
        };
        if let Some(outgoing) = self.composer.retry(model, channel, nonce) {
            self.post(outgoing);
        }
    }

    /// Delete on a message that failed.
    pub fn discard_failed(&mut self, channel: Id, nonce: Id) {
        if let Some(model) = &mut self.model {
            self.composer.discard(model, channel, nonce);
        }
    }

    /// What the backend says of a message I sent.
    fn settled(&mut self, channel: Id, nonce: Id, delivery: Delivery) {
        if delivery == Delivery::Unsure {
            self.unsure.insert((channel, nonce), Instant::now());
        }
        if let Some(model) = &mut self.model {
            self.composer.settled(model, channel, nonce, delivery);
        }
    }

    /// My own message came back from Discord: it is confirmed.
    fn confirm(&mut self, update: &Update) {
        let me = self.model.as_ref().map(|m| m.me);
        if let Update::MessageCreate {
            message,
            nonce: Some(nonce),
            ..
        } = update
            && Some(message.author.id) == me
        {
            self.composer.confirmed(*nonce);
            self.unsure.retain(|&(_, n), _| n != *nonce);
        }
    }

    /// Offers Retry on the messages whose answer was lost, once the gateway
    /// has had its chance to confirm them.
    fn settle_unsure(&mut self) {
        let alive_at = self.alive_at;
        let (settled, waiting): (HashMap<_, _>, _) = std::mem::take(&mut self.unsure)
            .into_iter()
            .partition(|&(_, since)| unsure_settled(since, alive_at));
        self.unsure = waiting;
        for (channel, nonce) in settled.into_keys() {
            self.settled(channel, nonce, Delivery::Failed(None));
        }
    }

    fn post(&mut self, outgoing: Outgoing) {
        self.write(Write::Send(outgoing));
    }

    /// Hands a change to the backend. Demo runs make it themselves: a sent
    /// message after a moment, an edit or a deletion at once.
    fn write(&mut self, write: Write) {
        if let Write::Edit { place, id, .. } | Write::Delete { place, id } = &write {
            self.notes.remove(&(place.channel, *id));
        }
        if let Some(backend) = &self.backend {
            backend.send(Command::Write(write));
            return;
        }
        let (channel, update, change) = match write {
            Write::Send(outgoing) => {
                let due = std::time::Instant::now() + DEMO_DELIVERY;
                self.demo_outbox.push((due, outgoing));
                return;
            }
            Write::Edit {
                place, id, content, ..
            } => {
                let channel = place.channel;
                let content = Some(content);
                let (attachments, embeds, edited) = (None, None, true);
                let update = Update::MessageEdit {
                    channel,
                    id,
                    content,
                    attachments,
                    embeds,
                    edited,
                };
                (channel, update, (id, Change::Edit))
            }
            Write::Delete { place, id } => {
                let (channel, ids) = (place.channel, vec![id]);
                (
                    channel,
                    Update::MessageDelete { channel, ids },
                    (id, Change::Delete),
                )
            }
        };
        if let Some(model) = &mut self.model {
            model.apply(update);
        }
        self.changed(channel, change.0, change.1, Ok(()));
    }

    /// Demo runs confirm what was sent, as Discord would, once it is due,
    /// or refuse it where the demo says Discord would.
    fn deliver_demo(&mut self, ctx: &egui::Context) {
        let now = std::time::Instant::now();
        let (due, waiting) = std::mem::take(&mut self.demo_outbox)
            .into_iter()
            .partition(|(at, _)| *at <= now);
        self.demo_outbox = waiting;
        if let Some((at, _)) = self.demo_outbox.iter().min_by_key(|(at, _)| *at) {
            ctx.request_repaint_after(*at - now);
        }
        for (_, Outgoing { place, nonce, .. }) in due {
            let channel = place.channel;
            if let Some(reason) = crate::demo::refusal(channel) {
                self.settled(channel, nonce, Delivery::Failed(Some(reason.into())));
                continue;
            }
            let Some(model) = &mut self.model else {
                return;
            };
            let pending = model.messages(channel).iter().find(|m| m.id == nonce);
            let Some(pending) = pending.cloned() else {
                continue;
            };
            let message = Message {
                id: self.composer.next_id(jiff::Timestamp::now()),
                delivery: Delivery::Sent,
                ..pending
            };
            let update = Update::MessageCreate {
                channel,
                guild: place.guild,
                message,
                ping: crate::model::Ping::default(),
                nonce: Some(nonce),
            };
            self.confirm(&update);
            if let Some(model) = &mut self.model {
                model.apply(update);
            }
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
            self.take(event);
        }
    }

    /// Takes one thing the backend reported.
    fn take(&mut self, event: Event) {
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
            Event::Link(link) => {
                self.link = link;
                self.alive_at = (link == Link::Connected).then(Instant::now);
            }
            Event::Ready(model) => {
                // A new session after a reconnect missed what happened
                // meanwhile: histories load again rather than stay
                // silently incomplete. What was open stays open.
                match self.model.is_some() {
                    true => self.selection.repair(&model),
                    false => self.selection = Selection::initial(Some(&model)),
                }
                self.model = Some(*model);
                // The copies of my unconfirmed messages went with the old
                // conversations.
                self.composer.after_ready();
                self.unsure.clear();
            }
            Event::AckDone { channel, flags } => {
                self.reading.outstanding.remove(&channel);
                if let (Some(model), Some(flags)) = (&mut self.model, flags) {
                    model.save_flags(channel, flags);
                }
            }
            Event::ReactionFailed(reaction) => {
                if let Some(model) = &mut self.model {
                    let undo = !reaction.add;
                    model.react(reaction.channel, reaction.message, &reaction.emoji, undo);
                }
            }
            Event::Open(channel) => {
                if let Some(model) = &self.model {
                    self.raise = self.selection.reveal(model, channel);
                }
            }
            Event::HistoryFailed { channel } => {
                self.loading_history.remove(&channel);
                self.failed_history.insert(channel);
            }
            Event::SendFailed {
                channel,
                nonce,
                reason,
            } => self.settled(channel, nonce, Delivery::Failed(reason)),
            Event::Changed {
                channel,
                id,
                change,
                result,
            } => self.changed(channel, id, change, result),
            Event::SendUnsure { channel, nonce } => self.settled(channel, nonce, Delivery::Unsure),
            Event::SendHeld {
                channel,
                nonce,
                wait,
            } => {
                let wait =
                    jiff::SignedDuration::try_from(wait).unwrap_or(jiff::SignedDuration::MAX);
                let now = jiff::Timestamp::now();
                let until = now.checked_add(wait).unwrap_or(now);
                self.settled(channel, nonce, Delivery::Held(until));
            }
            Event::Alive => {
                if self.link == Link::Connected {
                    self.alive_at = Some(Instant::now());
                }
            }
            Event::Update(update) => {
                if self.follow_typists(&update) {
                    return;
                }
                self.confirm(&update);
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
                    return;
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
                    if let Some(channel) = landed {
                        self.composer.history_loaded(model, channel);
                    }
                    self.selection.repair(model);
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
        self.deliver_demo(ctx);
        self.settle_unsure();
        self.follow_typing(ctx);
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
    use crate::model::Original;

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
        assert!(selection.reveal(&model, 201));
        assert_eq!(
            (selection.view, selection.channel),
            (View::Guild(200), Some(201))
        );
        assert!(selection.chosen, "opened as if picked: it is read");
        selection.reveal(&model, 900);
        assert_eq!(selection.view, View::DirectMessages);
        assert_eq!(selection.channel, Some(900));
        assert!(!selection.reveal(&model, 121));
        assert_eq!(selection.channel, Some(900), "a voice channel");
        selection.reveal(&model, 131);
        assert_eq!(selection.channel, Some(900), "a channel hidden from me");
        selection.reveal(&model, 12345);
        assert_eq!(selection.channel, Some(900), "a channel gone since");
        selection.open_guild(&model, 200);
        assert_eq!(selection.channel, Some(201), "remembered in its guild");
    }

    #[test]
    fn a_reaction_discord_refused_is_undone_on_screen() {
        let ctx = egui::Context::default();
        let mut app = App::new(&ctx, Some(crate::demo::model()), None);
        let (channel, message) = (101, app.model.as_ref().unwrap().messages[&101][2].id);
        let shown = |app: &App| {
            let reactions = &app.model.as_ref().unwrap().messages[&channel][2].reactions;
            reactions
                .iter()
                .map(|r| (r.emoji.label(), r.count, r.me))
                .collect::<Vec<_>>()
        };
        let before = shown(&app);
        let party = before[0].0.clone();
        let emoji = crate::model::Emoji {
            id: None,
            name: party.clone(),
            animated: false,
        };
        // Demo runs change only what is shown.
        app.toggle_reaction(channel, message, emoji.clone());
        assert_eq!(shown(&app)[0], (party.clone(), before[0].1 - 1, false));
        app.take(Event::ReactionFailed(ReactionRequest {
            channel,
            guild: Some(100),
            message,
            emoji,
            add: false,
        }));
        assert_eq!(shown(&app), before);
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

    /// The demo's messages are dated from the real clock.
    fn now() -> jiff::Timestamp {
        jiff::Timestamp::now()
    }

    #[test]
    fn a_draft_is_sent_once_and_shows_at_once() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        composer.drafts.insert(111, " :wave: salut ".into());
        let outgoing = composer.send(&mut model, 111, now()).unwrap();
        assert_eq!(outgoing.content, "👋 salut");
        assert_eq!(outgoing.place.guild, Some(100));
        let shown = model.messages(111).last().unwrap();
        assert_eq!(
            (shown.id, &shown.delivery),
            (outgoing.nonce, &Delivery::Sending)
        );
        assert_eq!(shown.author.id, model.me);
        assert!(!composer.drafts.contains_key(&111));
        assert_eq!(composer.send(&mut model, 111, now()), None, "nothing left");
    }

    #[test]
    fn some_drafts_stay_unsent() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        // No permission in #annonces, blank, too long, history not loaded.
        model.messages.remove(&112);
        for (channel, draft) in [
            (101, "salut".to_owned()),
            (111, "  \n".to_owned()),
            (113, "a".repeat(2001)),
            (112, "salut".to_owned()),
        ] {
            composer.drafts.insert(channel, draft);
            assert_eq!(composer.send(&mut model, channel, now()), None);
            assert!(composer.drafts.contains_key(&channel));
        }
    }

    /// A message sent in `channel`, and its nonce.
    fn sent(model: &mut Model, composer: &mut Composer, channel: Id, text: &str) -> Id {
        composer.drafts.insert(channel, text.into());
        composer.send(model, channel, now()).unwrap().nonce
    }

    fn delivery(model: &Model, channel: Id, nonce: Id) -> Option<Delivery> {
        let message = model.messages(channel).iter().find(|m| m.id == nonce);
        message.map(|m| m.delivery.clone())
    }

    #[test]
    fn a_retry_sends_the_same_message_again() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        let nonce = sent(&mut model, &mut composer, 900, "un");
        assert_eq!(composer.retry(&mut model, 900, nonce), None, "on its way");
        composer.settled(&mut model, 900, nonce, Delivery::Failed(None));
        let retried = composer.retry(&mut model, 900, nonce).unwrap();
        // The same nonce: a copy that arrived after all confirms it.
        assert_eq!((retried.nonce, retried.content.as_str()), (nonce, "un"));
        assert_eq!(delivery(&model, 900, nonce), Some(Delivery::Sending));
        // Retry asks again whether I may still write there.
        composer.settled(&mut model, 900, nonce, Delivery::Failed(None));
        model.dms.retain(|d| d.id != 900);
        assert_eq!(composer.retry(&mut model, 900, nonce), None);
    }

    #[test]
    fn what_fastcord_does_not_do_stays_in_the_draft_with_a_notice() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        composer.drafts.insert(111, "/nick Dyl".into());
        assert_eq!(composer.send(&mut model, 111, now()), None);
        let notice = composer.notice.clone().unwrap();
        assert_eq!(notice, (111, crate::compose::SLASH.into()));
        assert!(composer.drafts.contains_key(&111));
        // A command the web client runs as text goes, and the notice with it.
        composer.drafts.insert(111, "/shrug".into());
        assert_eq!(
            composer.send(&mut model, 111, now()).unwrap().content,
            "¯\\_(ツ)_/¯"
        );
        assert_eq!(composer.notice, None);
    }

    #[test]
    fn a_message_whose_copy_is_gone_is_never_lost() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        let first = sent(&mut model, &mut composer, 111, "un");
        let second = sent(&mut model, &mut composer, 111, "deux");
        let third = sent(&mut model, &mut composer, 111, "trois");
        composer.settled(&mut model, 111, first, Delivery::Failed(None));
        composer.settled(&mut model, 111, second, Delivery::Failed(None));
        // A new READY: the conversation is replaced, its history not back.
        model.messages.clear();
        composer.drafts.insert(111, "brouillon".into());
        composer.after_ready();
        assert_eq!(
            composer.drafts[&111], "brouillon\nun\ndeux",
            "failed ones, oldest first"
        );
        // The one still on its way fails later: its text comes back too.
        composer.settled(&mut model, 111, third, Delivery::Failed(None));
        assert_eq!(composer.drafts[&111], "brouillon\nun\ndeux\ntrois");
        assert!(composer.posted.is_empty());
    }

    #[test]
    fn an_unsure_message_waits_for_its_history_after_ready() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        let arrived = sent(&mut model, &mut composer, 111, "arrivé");
        let lost = sent(&mut model, &mut composer, 111, "perdu");
        composer.settled(&mut model, 111, arrived, Delivery::Unsure);
        composer.settled(&mut model, 111, lost, Delivery::Unsure);
        let mut history = model.messages.remove(&111).unwrap();
        composer.after_ready();
        assert!(
            !composer.drafts.contains_key(&111),
            "nothing yet: it may be there"
        );
        // The reloaded history holds the first, by me.
        history.retain(|m| m.delivery == Delivery::Sent);
        let mut there = history[0].clone();
        (there.id, there.author.id, there.content) = (arrived + 7, model.me, "arrivé".into());
        history.push(there);
        model.messages.insert(111, history);
        composer.history_loaded(&model, 111);
        assert_eq!(composer.drafts[&111], "perdu");
        assert_eq!(composer.notice, Some((111, MAYBE_SENT.into())));
        assert!(composer.posted.is_empty());
    }

    #[test]
    fn a_failure_shows_again_once_the_conversation_is_back() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        let nonce = sent(&mut model, &mut composer, 111, "un");
        let history = model.messages.remove(&111).unwrap();
        composer.after_ready();
        model.messages.insert(111, history);
        let reason = Some("Slowmode is enabled.".to_owned());
        composer.settled(&mut model, 111, nonce, Delivery::Failed(reason.clone()));
        assert_eq!(delivery(&model, 111, nonce), Some(Delivery::Failed(reason)));
        composer.confirmed(nonce);
        assert!(composer.posted.is_empty());
    }

    /// Dylan's last message in #général, and Sam's question before it.
    fn general(model: &Model) -> (Id, Id) {
        let messages = model.messages(111);
        (messages[8].id, messages[7].id)
    }

    #[test]
    fn edits_wait_for_discord_and_an_emptied_one_asks_to_delete() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        let (mine, sam) = general(&model);
        assert!(!composer.edit(&model, 111, sam), "not mine");
        composer.drafts.insert(111, "brouillon".into());
        assert!(!composer.edit_last(&model, 111), "the draft is not empty");
        composer.drafts.clear();
        assert!(composer.edit_last(&model, 111));
        assert_eq!(composer.editing.as_ref().unwrap().id, mine);
        let original = model.message(111, mine).unwrap().content.clone();
        // Unchanged: it just closes.
        assert_eq!(composer.save_edit(&model), Saved::Nothing);
        assert!(composer.editing.is_none());
        let draft = |composer: &mut Composer, model: &Model, text: &str| {
            composer.edit(model, 111, mine);
            composer.editing.as_mut().unwrap().draft = text.into();
        };
        draft(&mut composer, &model, "Oui :tada:");
        let place = Place {
            channel: 111,
            guild: Some(100),
        };
        let content = "Oui 🎉".to_owned();
        let write = Write::Edit {
            place,
            id: mine,
            content,
            quiet: false,
        };
        assert_eq!(composer.save_edit(&model), Saved::Edit(write));
        // Not shown before Discord has it, and not saved twice.
        assert_eq!(model.message(111, mine).unwrap().content, original);
        assert_eq!(composer.save_edit(&model), Saved::Nothing);
        composer.edit_done(111, mine);
        assert!(composer.editing.is_none());
        // Too long: the editor stays open.
        draft(&mut composer, &model, &"a".repeat(2001));
        assert_eq!(composer.save_edit(&model), Saved::Nothing);
        assert!(composer.editing.is_some());
        draft(&mut composer, &model, " ");
        assert_eq!(composer.save_edit(&model), Saved::ConfirmDelete(111, mine));
        // With a file, emptied text leaves the file.
        let pictured = model.messages.get_mut(&111).unwrap();
        pictured[8].attachments = model_attachment();
        draft(&mut composer, &model, "");
        assert!(matches!(
            composer.save_edit(&model),
            Saved::Edit(Write::Edit { content, .. }) if content.is_empty()
        ));
    }

    fn model_attachment() -> Vec<crate::model::Attachment> {
        crate::demo::model().messages(101)[1].attachments.clone()
    }

    fn demo_app() -> App {
        App::new(&egui::Context::default(), Some(crate::demo::model()), None)
    }

    #[test]
    fn a_reply_answers_its_message_and_pings_unless_switched_off() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        let (_, sam) = general(&model);
        composer.reply(&model, 111, sam);
        composer.replies.get_mut(&111).unwrap().ping = false;
        composer.drafts.insert(111, "oui".into());
        let outgoing = composer.send(&mut model, 111, now()).unwrap();
        let quiet = Some(ReplyTo {
            message: sam,
            ping: false,
        });
        assert_eq!(outgoing.reply, quiet);
        assert!(composer.replies.is_empty(), "one message answers it");
        let shown = model.messages(111).last().unwrap().reply.clone().unwrap();
        assert!(matches!(shown.original, Original::Shown(ref author, _) if author.id == 4));
        assert!(!shown.ping);
        // Retried, it answers the same way.
        composer.settled(&mut model, 111, outgoing.nonce, Delivery::Failed(None));
        let retried = composer.retry(&mut model, 111, outgoing.nonce).unwrap();
        assert_eq!(retried.reply, quiet);
        // A pending message cannot be answered, nor anything where I may not write.
        composer.reply(&model, 111, outgoing.nonce);
        let annonce = model.messages(101)[0].id;
        composer.reply(&model, 101, annonce);
        assert!(composer.replies.is_empty());
    }

    #[test]
    fn a_reply_put_back_or_retried_keeps_what_it_answers() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        let (_, sam) = general(&model);
        let quiet = ReplyTo {
            message: sam,
            ping: false,
        };
        composer.replies.insert(111, quiet);
        composer.drafts.insert(111, "oui".into());
        let first = composer.send(&mut model, 111, now()).unwrap().nonce;
        composer.settled(&mut model, 111, first, Delivery::Failed(None));
        // Put back in the draft by a new READY: it answers Sam again.
        let history = model.messages.remove(&111).unwrap();
        composer.after_ready();
        assert_eq!(composer.drafts[&111], "oui");
        assert_eq!(composer.replies[&111], quiet);
        // Retried after Sam's message went: a plain message, as send does.
        model.messages.insert(111, history);
        composer.replies.clear();
        composer.drafts.insert(111, "encore".into());
        composer.replies.insert(111, quiet);
        let second = composer.send(&mut model, 111, now()).unwrap().nonce;
        composer.settled(&mut model, 111, second, Delivery::Failed(None));
        model.apply(Update::MessageDelete {
            channel: 111,
            ids: vec![sam],
        });
        assert_eq!(composer.retry(&mut model, 111, second).unwrap().reply, None);
    }

    #[test]
    fn editing_a_quiet_reply_keeps_it_quiet() {
        let mut model = crate::demo::model();
        let mut composer = Composer::default();
        let (mine, _) = general(&model);
        let edited = |composer: &mut Composer, model: &Model| {
            composer.edit(model, 111, mine);
            composer.editing.as_mut().unwrap().draft = "autre".into();
            match composer.save_edit(model) {
                Saved::Edit(Write::Edit { quiet, .. }) => quiet,
                saved => panic!("{saved:?}"),
            }
        };
        // Dylan's demo reply pinged Sam.
        assert!(!edited(&mut composer, &model));
        composer.editing = None;
        let reply = model.messages.get_mut(&111).unwrap()[8]
            .reply
            .as_mut()
            .unwrap();
        reply.ping = false;
        assert!(edited(&mut composer, &model));
        composer.editing = None;
        // Its original not loaded: still quiet, never a ping by guess.
        let reply = model.messages.get_mut(&111).unwrap()[8]
            .reply
            .as_mut()
            .unwrap();
        reply.original = Original::Unknown;
        assert!(edited(&mut composer, &model));
    }

    #[test]
    fn others_typing_show_until_their_message_never_me() {
        let mut app = demo_app();
        let user = |id| crate::model::User {
            id,
            ..Default::default()
        };
        let start = |id| Update::TypingStart {
            channel: 111,
            user: user(id),
        };
        assert!(app.follow_typists(&start(1)), "me");
        assert!(app.follow_typists(&start(2)));
        assert!(app.follow_typists(&start(77)));
        let model = app.model.as_ref().unwrap();
        assert!(
            model.users.contains_key(&77),
            "a typist not met yet gets a name"
        );
        let now = Instant::now();
        assert_eq!(app.typing.who(111, now), [2, 77]);
        let message = model.messages(111)[2].clone();
        assert_eq!(message.author.id, 2);
        let created = Update::MessageCreate {
            channel: 111,
            guild: Some(100),
            message,
            ping: crate::model::Ping::default(),
            nonce: None,
        };
        assert!(
            !app.follow_typists(&created),
            "the message itself still applies"
        );
        assert_eq!(app.typing.who(111, now), [77]);
    }

    #[test]
    fn deleting_asks_first_unless_confirmed_and_only_for_mine() {
        let mut app = demo_app();
        let (mine, sam) = general(app.model.as_ref().unwrap());
        app.delete(111, sam, true);
        assert!(app.model.as_ref().unwrap().message(111, sam).is_some());
        app.delete(111, mine, false);
        assert_eq!(app.confirm_delete, Some((111, mine)));
        assert!(app.model.as_ref().unwrap().message(111, mine).is_some());
        app.delete(111, mine, true);
        assert_eq!(app.confirm_delete, None);
        assert!(app.model.as_ref().unwrap().message(111, mine).is_none());
    }

    #[test]
    fn a_refused_edit_or_deletion_is_told_under_its_message() {
        let mut app = demo_app();
        let (mine, _) = general(app.model.as_ref().unwrap());
        app.composer.edit(app.model.as_ref().unwrap(), 111, mine);
        let reason = Some("Missing Permissions".to_owned());
        app.changed(111, mine, Change::Edit, Err(reason));
        assert!(
            app.composer.editing.is_none(),
            "closed, as the official client does"
        );
        let note = &app.notes[&(111, mine)];
        assert_eq!(note, "Couldn't edit this message: Missing Permissions");
        app.changed(111, mine, Change::Delete, Err(None));
        assert_eq!(app.notes[&(111, mine)], "Couldn't delete this message.");
        // The next try clears it.
        app.delete(111, mine, true);
        assert!(app.notes.is_empty());
    }

    #[test]
    fn retry_waits_for_the_gateway_after_a_lost_answer() {
        let since = Instant::now();
        let at = |s| Some(since + Duration::from_secs(s));
        assert!(!unsure_settled(since, None), "offline: no way to know");
        // Alive before the loss says nothing of it, however long ago.
        assert!(!unsure_settled(
            since,
            since.checked_sub(Duration::from_secs(60))
        ));
        // Reconnected, or a heartbeat answered, after the loss.
        assert!(unsure_settled(since, at(1)));
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
