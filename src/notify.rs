//! Desktop notifications for new messages.
//!
//! The decision is made on the backend's thread as each MESSAGE_CREATE
//! arrives, against a copy of the model that keeps no history: a window on
//! a hidden workspace may not draw, and a notification must not wait for
//! it. The window reports what it shows through [`Shared`].
//!
//! Rules follow the web client's notification store (`shouldNotify`): not
//! my own messages, not @silent ones, not from people I blocked or ignored
//! or from flagged spammers, not while my status is Do Not Disturb or quiet
//! mode is on, not the conversation open in a focused window, and otherwise
//! what my notification settings let through ([`Model::notifies`]).
//!
//! A first DM from someone new notifies only once its CHANNEL_CREATE has
//! arrived, which Discord sends before the message.

use crate::events::Update;
use crate::markdown::{self, Directory};
use crate::model::{ChannelKind, Id, Message, Model, Ping, created_at};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the window shows, as far as notifications care.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attention {
    pub focused: bool,
    /// The open conversation.
    pub open: Option<Id>,
}

/// What the window and the backend share about notifications.
#[derive(Debug)]
pub struct Shared {
    attention: Mutex<Attention>,
    /// Whether notifications show what a message says. On by default, as in
    /// the official client. Kept in memory only: fastcord has no settings
    /// file yet, so it is on again at each start.
    show_content: AtomicBool,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            attention: Mutex::default(),
            show_content: AtomicBool::new(true),
        }
    }
}

impl Shared {
    pub fn attention(&self) -> Attention {
        *self.attention.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_attention(&self, attention: Attention) {
        *self.attention.lock().unwrap_or_else(|e| e.into_inner()) = attention;
    }

    pub fn show_content(&self) -> bool {
        self.show_content.load(Ordering::Relaxed)
    }

    pub fn set_show_content(&self, show: bool) {
        self.show_content.store(show, Ordering::Relaxed);
    }
}

/// One notification. A channel shows one at a time: the next replaces it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub channel: Id,
    /// The message it is about.
    pub message: Id,
    pub title: String,
    /// Plain text: the notifier escapes it for servers that read markup.
    pub body: String,
}

/// What the notifier is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Show(Notification),
    /// The channel was read up to `message`: its notification goes unless
    /// it is about a later message, as in the official client.
    Read {
        channel: Id,
        message: Id,
    },
    /// Every notification goes and clicking one no longer does anything:
    /// signed out, or previews turned off.
    ClearAll,
}

/// Whether a new message notifies.
pub fn wanted(
    model: &Model,
    channel: Id,
    guild: Option<Id>,
    message: &Message,
    ping: &Ping,
    attention: Attention,
) -> bool {
    let now = created_at(message.id);
    message.author.id != model.me
        && !ping.silent
        && !ping.from_spammer
        && !model.blocked_or_ignored(message.author.id)
        && !model.quiet_mode
        && !model.do_not_disturb.is_some_and(|dnd| dnd.active(now))
        && !(attention.focused && attention.open == Some(channel))
        && model.unseen(channel, guild, message.id)
        && model.notifies(channel, ping, now)
}

/// Whether a message notifies even through a burst: a DM, or a mention of
/// me by name.
fn urgent(model: &Model, channel: Id, ping: &Ping) -> bool {
    ping.me || model.dm(channel).is_some()
}

/// A name in a title, as the official client sets it: control characters
/// out, and isolated (U+2068…U+2069) so a right-to-left name cannot turn
/// the rest of the title around.
fn isolate(name: &str) -> String {
    let clean: String = name
        .chars()
        .filter(|&c| !c.is_control() && !matches!(c, '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .collect();
    format!("\u{2068}{clean}\u{2069}")
}

/// The official client's title: the author (by their nickname in a
/// guild), then in a guild the channel and its category ("Léa (#général,
/// Discussions)"), in a group DM its members. Without a category the guild
/// goes there instead: the official client shows the guild's icon, which
/// fastcord has not.
pub fn title(model: &Model, channel: Id, message: &Message) -> String {
    let guild = model.guild_of(channel);
    let author = isolate(&model.name_in(guild, &message.author));
    if let Some(dm) = model.dm(channel) {
        return match dm.group {
            false => author,
            true => format!("{author} ({})", isolate(&dm.title())),
        };
    }
    let Some((guild, channel)) = model
        .guilds
        .iter()
        .find_map(|g| g.channel(channel).map(|c| (g, c)))
    else {
        return author;
    };
    let hash = if channel.kind == ChannelKind::Voice {
        ""
    } else {
        "#"
    };
    let place = match channel.parent.and_then(|p| guild.channel(p)) {
        Some(category) => &category.name,
        None => &guild.name,
    };
    let name = isolate(&format!("{hash}{}", channel.name));
    format!("{author} ({name}, {})", isolate(place))
}

/// The longest body shown, in characters.
const BODY_CHARS: usize = 250;

/// What a notification says: the text without its markdown, or, without
/// text, the first embed or attachment, as the official client falls back.
/// With content hidden, only that there is a message.
pub fn body(
    model: &Model,
    channel: Id,
    message: &Message,
    show_content: bool,
    now: jiff::Timestamp,
    tz: &jiff::tz::TimeZone,
) -> String {
    if !show_content {
        return HIDDEN_BODY.to_owned();
    }
    let names = Directory::new(model, channel);
    let mut text = markdown::plain(&markdown::parse(&message.content), &names, now, tz);
    if text.is_empty()
        && let Some(embed) = message.embeds.first()
    {
        let field = embed
            .fields
            .first()
            .map(|f| format!("{} {}", f.name, f.value));
        text = match (&embed.title, &embed.description) {
            (Some(title), Some(description)) => format!("{title} {description}"),
            (None, Some(text)) | (Some(text), None) => text.clone(),
            (None, None) => field.unwrap_or_default(),
        };
    }
    if text.is_empty()
        && let Some(file) = message.attachments.first()
    {
        text = format!("Uploaded {}", file.filename);
    }
    truncate(&text, BODY_CHARS)
}

const HIDDEN_BODY: &str = "New message";

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
        None => text.to_owned(),
    }
}

/// Servers with the `body-markup` capability read the body as markup: a
/// message's `<` and `&` must stay text there.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// At most this many new notifications in [`BURST_WINDOW`]; past it, guild
/// messages notify nothing until the window moves on (their badges still
/// count). A DM or a mention of me always gets through, and so does a
/// message in a channel notified within the window: it replaces that
/// channel's notification rather than adding one.
const BURST: usize = 4;
const BURST_WINDOW: Duration = Duration::from_secs(10);

/// Keeps a burst of messages from flooding the desktop.
#[derive(Debug, Default)]
pub struct Throttle {
    /// When each new notification counted against the burst was shown.
    counted: VecDeque<Instant>,
    /// The channels notified within the window, and when last.
    recent: HashMap<Id, Instant>,
}

impl Throttle {
    /// Whether a notification may show now, counting it if so.
    pub fn admit(&mut self, channel: Id, urgent: bool, now: Instant) -> bool {
        let fresh = |at: &Instant| now.duration_since(*at) < BURST_WINDOW;
        while self.counted.front().is_some_and(|at| !fresh(at)) {
            self.counted.pop_front();
        }
        self.recent.retain(|_, at| fresh(at));
        let replaces = self.recent.contains_key(&channel);
        let admitted = urgent || replaces || self.counted.len() < BURST;
        if admitted {
            if !urgent && !replaces {
                self.counted.push_back(now);
            }
            self.recent.insert(channel, now);
        }
        admitted
    }

    /// The channel was read: its notification is gone.
    pub fn forget(&mut self, channel: Id) {
        self.recent.remove(&channel);
    }
}

/// Follows the gateway and says what to notify. Its copy of the model takes
/// the gateway's updates only, so it never holds a message's history.
#[derive(Debug)]
pub struct Notifications {
    model: Option<Model>,
    throttle: Throttle,
    shared: Arc<Shared>,
}

impl Notifications {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            model: None,
            throttle: Throttle::default(),
            shared,
        }
    }

    /// A new session's model.
    pub fn ready(&mut self, model: &Model) {
        self.model = Some(model.clone());
    }

    /// Takes one update, in the gateway's order, and says what to notify.
    pub fn follow(&mut self, update: &Update, now: Instant) -> Option<Notice> {
        let model = self.model.as_mut()?;
        let notice = match update {
            Update::MessageCreate {
                channel,
                guild,
                message,
                ping,
            } => {
                let attention = self.shared.attention();
                let urgent = urgent(model, *channel, ping);
                (wanted(model, *channel, *guild, message, ping, attention)
                    && self.throttle.admit(*channel, urgent, now))
                .then(|| {
                    let wall = jiff::Timestamp::now();
                    let tz = jiff::tz::TimeZone::system();
                    let show = self.shared.show_content();
                    Notice::Show(Notification {
                        channel: *channel,
                        message: message.id,
                        title: title(model, *channel, message),
                        body: body(model, *channel, message, show, wall, &tz),
                    })
                })
            }
            Update::Acked {
                channel,
                message,
                manual: false,
                ..
            } => {
                self.throttle.forget(*channel);
                Some(Notice::Read {
                    channel: *channel,
                    message: *message,
                })
            }
            _ => None,
        };
        model.apply(update.clone());
        notice
    }
}

/// The notifications on screen, by channel, as the notifier tracks them.
#[derive(Debug, Default)]
pub struct Shown {
    popups: HashMap<Id, Popup>,
}

#[derive(Debug)]
struct Popup {
    /// The notification server's id for it.
    id: u32,
    message: Id,
}

impl Shown {
    /// The notification a new one for `channel` replaces (0: none).
    pub fn replaces(&self, channel: Id) -> u32 {
        self.popups.get(&channel).map_or(0, |p| p.id)
    }

    pub fn shown(&mut self, notification: &Notification, id: u32) {
        let popup = Popup {
            id,
            message: notification.message,
        };
        self.popups.insert(notification.channel, popup);
    }

    /// The server closed it (expired, dismissed or clicked).
    pub fn closed(&mut self, id: u32) {
        self.popups.retain(|_, p| p.id != id);
    }

    /// The channel a clicked notification opens.
    pub fn clicked(&self, id: u32) -> Option<Id> {
        self.popups
            .iter()
            .find_map(|(&channel, p)| (p.id == id).then_some(channel))
    }

    /// The notification to close once `channel` is read up to `message`.
    pub fn read(&mut self, channel: Id, message: Id) -> Option<u32> {
        let popup = self.popups.get(&channel)?;
        (popup.message <= message).then_some(popup.id)?;
        self.popups.remove(&channel).map(|p| p.id)
    }

    /// Every notification, to close; none is tracked after.
    pub fn clear(&mut self) -> Vec<u32> {
        self.popups.drain().map(|(_, p)| p.id).collect()
    }
}

/// Sends notices to the desktop's notifier.
#[derive(Clone, Debug)]
pub struct Notifier {
    sender: tokio::sync::mpsc::UnboundedSender<Notice>,
}

impl Notifier {
    pub fn send(&self, notice: Notice) {
        let _ = self.sender.send(notice);
    }
}

/// The longest a notification server may take to answer before the
/// notifier gives up on that call.
const SERVER_TIMEOUT: Duration = Duration::from_secs(5);

/// Notices waiting for the notifier, with the stale ones dropped: a
/// notification replaced by a later one for its channel, one that a later
/// read or clear would take down at once, and everything before a clear.
/// Order is otherwise kept, so whatever the queue held, the notifier only
/// has as many notifications to show as there are channels.
fn compact(notices: impl IntoIterator<Item = Notice>) -> VecDeque<Notice> {
    let mut kept: VecDeque<Notice> = VecDeque::new();
    for notice in notices {
        match &notice {
            Notice::ClearAll => kept.clear(),
            Notice::Show(new) => {
                kept.retain(|n| !matches!(n, Notice::Show(old) if old.channel == new.channel))
            }
            Notice::Read { channel, message } => kept.retain(|n| match n {
                Notice::Show(old) => old.channel != *channel || old.message > *message,
                Notice::Read { channel: c, .. } => c != channel,
                Notice::ClearAll => true,
            }),
        }
        kept.push_back(notice);
    }
    kept
}

/// The desktop's notifier: one D-Bus connection on its own thread, so a
/// round trip never holds up the gateway. `opened` runs when a notification
/// is clicked, with its channel. Never used in tests.
pub fn desktop(opened: impl Fn(Id) + Send + 'static) -> Notifier {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let started = std::thread::Builder::new()
        .name("notifications".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                log::warn!("notifications are off: no runtime");
                return;
            };
            if let Err(error) = runtime.block_on(serve_desktop(&mut receiver, &opened)) {
                log::warn!("notifications are off: {error}");
            }
        });
    if let Err(error) = started {
        log::warn!("notifications are off: {error}");
    }
    Notifier { sender }
}

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

/// What the notifier knows of the notification server.
#[derive(Debug, Default)]
struct ServerState {
    /// Its unique bus name: signals from anyone else (a server since
    /// replaced) are not about our notifications.
    owner: Option<String>,
    /// Whether it reads bodies as markup; `None` until it said. Unknown
    /// counts as yes: escaped text only looks odd, unescaped text in
    /// markup can break or restyle the notification.
    markup: Option<bool>,
    shown: Shown,
}

impl ServerState {
    fn ours(&self, sender: Option<&str>) -> bool {
        sender.is_some() && sender == self.owner.as_deref()
    }

    /// Another server took the name: what the last one showed is gone, and
    /// the new one may read markup differently. A server starting for the
    /// first time (activated by our first notification) has nothing to
    /// forget.
    fn owner_changed(&mut self, old: Option<String>, new: Option<String>) {
        if old.is_some() {
            self.shown.clear();
        }
        self.owner = new;
        self.markup = None;
    }
}

/// The notification server as the notifier talks to it.
struct Server<'a> {
    connection: zbus::Connection,
    proxy: zbus::Proxy<'a>,
    state: ServerState,
}

impl Server<'_> {
    async fn markup(&mut self) -> bool {
        if self.state.markup.is_none() {
            let capabilities: zbus::Result<Vec<String>> =
                self.proxy.call("GetCapabilities", &()).await;
            self.state.markup = capabilities
                .ok()
                .map(|c| c.iter().any(|c| c == "body-markup"));
        }
        self.state.markup.unwrap_or(true)
    }

    async fn take(&mut self, notice: Notice) {
        let close = match notice {
            Notice::Show(notification) => {
                let body = match self.markup().await {
                    true => escape(&notification.body),
                    false => notification.body.clone(),
                };
                let replaces = self.state.shown.replaces(notification.channel);
                match notify(&self.proxy, replaces, &notification.title, &body).await {
                    Ok(id) => self.state.shown.shown(&notification, id),
                    Err(error) => log::warn!("a notification could not be shown: {error}"),
                }
                Vec::new()
            }
            Notice::Read { channel, message } => self
                .state
                .shown
                .read(channel, message)
                .into_iter()
                .collect(),
            Notice::ClearAll => self.state.shown.clear(),
        };
        for id in close {
            self.close(id).await;
        }
    }

    /// `CloseNotification` without waiting for an answer: a reply awaited
    /// while signals pile up unread could stall the connection.
    async fn close(&self, id: u32) {
        let message = zbus::Message::method_call(NOTIFICATIONS_PATH, "CloseNotification")
            .and_then(|m| m.destination(NOTIFICATIONS))
            .and_then(|m| m.interface(NOTIFICATIONS))
            .and_then(|m| m.with_flags(zbus::message::Flags::NoReplyExpected))
            .and_then(|m| m.build(&id));
        if let Ok(message) = message {
            let _ = self.connection.send(&message).await;
        }
    }

    fn ours(&self, signal: &zbus::Message) -> bool {
        let header = signal.header();
        self.state.ours(header.sender().map(|s| s.as_str()))
    }
}

/// The freedesktop notification protocol over one session-bus connection.
/// Signals are read between notices, one notice at a time, so they never
/// pile up behind a burst.
async fn serve_desktop(
    notices: &mut tokio::sync::mpsc::UnboundedReceiver<Notice>,
    opened: &dyn Fn(Id),
) -> zbus::Result<()> {
    use futures_util::StreamExt as _;
    let connection = zbus::connection::Builder::session()?
        .method_timeout(SERVER_TIMEOUT)
        .build()
        .await?;
    let proxy = zbus::Proxy::new(
        &connection,
        NOTIFICATIONS,
        NOTIFICATIONS_PATH,
        NOTIFICATIONS,
    )
    .await?;
    let bus = zbus::fdo::DBusProxy::new(&connection).await?;
    let mut owners = bus
        .receive_name_owner_changed_with_args(&[(0, NOTIFICATIONS)])
        .await?;
    let mut invoked = proxy.receive_signal("ActionInvoked").await?;
    let mut closed = proxy.receive_signal("NotificationClosed").await?;
    let owner = bus.get_name_owner(NOTIFICATIONS.try_into()?).await.ok();
    let mut server = Server {
        connection: connection.clone(),
        proxy,
        state: ServerState {
            owner: owner.map(|o| o.to_string()),
            ..ServerState::default()
        },
    };
    let mut pending: VecDeque<Notice> = VecDeque::new();
    loop {
        tokio::select! {
            biased;
            Some(signal) = owners.next() => {
                if let Ok(args) = signal.args() {
                    let name = |o: &zbus::zvariant::Optional<zbus::names::UniqueName<'_>>| {
                        o.as_ref().map(|n| n.to_string())
                    };
                    server.state.owner_changed(name(args.old_owner()), name(args.new_owner()));
                }
            }
            Some(signal) = invoked.next() => {
                if server.ours(&signal)
                    && let Ok((id, action)) = signal.body().deserialize::<(u32, String)>()
                    && action == "default"
                    && let Some(channel) = server.state.shown.clicked(id)
                {
                    opened(channel);
                }
            }
            Some(signal) = closed.next() => {
                if server.ours(&signal)
                    && let Ok((id, _reason)) = signal.body().deserialize::<(u32, u32)>()
                {
                    server.state.shown.closed(id);
                }
            }
            notice = notices.recv(), if pending.is_empty() => {
                let Some(first) = notice else {
                    return Ok(());
                };
                let mut batch = vec![first];
                while let Ok(more) = notices.try_recv() {
                    batch.push(more);
                }
                pending = compact(batch);
            }
            () = std::future::ready(()), if !pending.is_empty() => {
                if let Some(notice) = pending.pop_front() {
                    server.take(notice).await;
                }
            }
        }
    }
}

/// `Notify`, as the specification lays it out: the app, the notification
/// it replaces, no icon, the texts, a default action (a click), hints for
/// the server, and the server's own timeout.
async fn notify(
    proxy: &zbus::Proxy<'_>,
    replaces: u32,
    title: &str,
    body: &str,
) -> zbus::Result<u32> {
    let mut hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    hints.insert("category", "im.received".into());
    hints.insert("desktop-entry", "fastcord".into());
    let actions = vec!["default", "Open"];
    let call = ("fastcord", replaces, "", title, body, actions, hints, -1i32);
    proxy.call("Notify", &call).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Attachment, Channel, ChannelSettings, DmChannel, Embed, Guild, GuildSettings, Mute, Notify,
        Permissions, ReadState, Role, User,
    };

    const ME: Id = 1;
    const FRIEND: Id = 2;
    const GUILD: Id = 100;
    const CATEGORY: Id = 10;
    const TEXT: Id = 11;
    const VOICE: Id = 12;
    const DM: Id = 20;
    const GROUP: Id = 21;
    const ROLE: Id = 50;
    /// The newest message every channel starts with.
    const START: Id = 1 << 32;

    fn user(id: Id, name: &str) -> User {
        User {
            id,
            username: name.into(),
            global_name: None,
        }
    }

    fn channel(id: Id, name: &str, kind: ChannelKind, parent: Option<Id>) -> Channel {
        Channel {
            id,
            name: name.into(),
            kind,
            parent,
            position: 0,
            overwrites: vec![],
            last_message_id: Some(START),
        }
    }

    fn model() -> Model {
        let guild = Guild {
            id: GUILD,
            name: "Rust".into(),
            channels: vec![
                channel(CATEGORY, "Talk", ChannelKind::Category, None),
                channel(TEXT, "general", ChannelKind::Text, Some(CATEGORY)),
                channel(VOICE, "Lounge", ChannelKind::Voice, Some(CATEGORY)),
            ],
            owner_id: FRIEND,
            roles: vec![Role {
                id: GUILD,
                name: "@everyone".into(),
                position: 0,
                permissions: Permissions::VIEW_CHANNEL,
            }],
            my_roles: vec![ROLE],
            joined_at: None,
            default_notify: Notify::Mentions,
        };
        let dm = |id, recipients: Vec<User>| DmChannel {
            id,
            group: recipients.len() > 1,
            recipients,
            last_message_id: Some(START),
        };
        let read = ReadState {
            last_read: Some(START),
            ..ReadState::default()
        };
        Model {
            me: ME,
            guilds: vec![guild],
            dms: vec![
                dm(DM, vec![user(FRIEND, "lea")]),
                dm(GROUP, vec![user(FRIEND, "lea"), user(3, "marc")]),
            ],
            read_states: [TEXT, VOICE, DM, GROUP].map(|c| (c, read)).into(),
            ..Model::default()
        }
    }

    fn message(id: Id, author: Id, content: &str) -> Message {
        Message {
            id,
            author: user(author, if author == ME { "me" } else { "lea" }),
            content: content.into(),
            attachments: vec![],
            embeds: vec![],
            reactions: vec![],
        }
    }

    fn ping(me: bool, everyone: bool, roles: &[Id]) -> Ping {
        Ping {
            me,
            everyone,
            roles: roles.to_vec(),
            ..Ping::default()
        }
    }

    fn away() -> Attention {
        Attention::default()
    }

    /// Whether a fresh message from a friend in `channel` notifies.
    fn notifies(model: &Model, channel: Id, ping: &Ping) -> bool {
        let guild = model.guild_of(channel);
        let message = message(START + 1, FRIEND, "hi");
        wanted(model, channel, guild, &message, ping, away())
    }

    fn settings(model: &mut Model, guild: Option<Id>) -> &mut GuildSettings {
        model.guild_settings.entry(guild).or_default()
    }

    #[test]
    fn dms_notify_on_every_message_and_guilds_follow_their_level() {
        let mut model = model();
        let quiet = Ping::default();
        assert!(notifies(&model, DM, &quiet));
        assert!(notifies(&model, GROUP, &quiet));
        assert!(
            !notifies(&model, TEXT, &quiet),
            "the guild's default: mentions"
        );
        assert!(notifies(&model, TEXT, &ping(true, false, &[])));

        settings(&mut model, Some(GUILD)).notify = Some(Notify::All);
        assert!(notifies(&model, TEXT, &quiet));
        assert!(
            !notifies(&model, VOICE, &quiet),
            "a voice chat needs a mention"
        );
        assert!(notifies(&model, VOICE, &ping(true, false, &[])));

        let category = ChannelSettings {
            notify: Some(Notify::Nothing),
            ..ChannelSettings::default()
        };
        settings(&mut model, Some(GUILD))
            .channels
            .insert(CATEGORY, category);
        assert!(
            !notifies(&model, TEXT, &ping(true, false, &[])),
            "nothing at all"
        );
        let channel = ChannelSettings {
            notify: Some(Notify::Mentions),
            ..ChannelSettings::default()
        };
        settings(&mut model, Some(GUILD))
            .channels
            .insert(TEXT, channel);
        assert!(
            notifies(&model, TEXT, &ping(true, false, &[])),
            "the channel wins"
        );
        assert!(!notifies(&model, TEXT, &quiet));

        settings(&mut model, None).channels.insert(
            DM,
            ChannelSettings {
                notify: Some(Notify::Mentions),
                ..ChannelSettings::default()
            },
        );
        assert!(!notifies(&model, DM, &quiet));
        assert!(notifies(&model, DM, &ping(true, false, &[])));
    }

    #[test]
    fn mutes_silence_everything_mentions_included_until_they_end() {
        let mut model = model();
        let mention = ping(true, false, &[]);
        let muted = |until| ChannelSettings {
            muted: Some(Mute { until }),
            ..ChannelSettings::default()
        };
        settings(&mut model, Some(GUILD))
            .channels
            .insert(CATEGORY, muted(None));
        assert!(!notifies(&model, TEXT, &mention), "category mute");

        let mut model = self::model();
        let ended = Some(jiff::Timestamp::UNIX_EPOCH);
        settings(&mut model, Some(GUILD))
            .channels
            .insert(TEXT, muted(ended));
        assert!(notifies(&model, TEXT, &mention), "a mute that ended");
        settings(&mut model, Some(GUILD)).muted = Some(Mute { until: None });
        assert!(!notifies(&model, TEXT, &mention), "guild mute");

        settings(&mut model, None).channels.insert(DM, muted(None));
        assert!(!notifies(&model, DM, &mention), "DM mute");
        assert!(notifies(&model, GROUP, &Ping::default()));
    }

    #[test]
    fn everyone_and_role_mentions_obey_their_suppression() {
        let mut model = model();
        let everyone = ping(false, true, &[]);
        let role = ping(false, false, &[ROLE]);
        assert!(notifies(&model, TEXT, &everyone));
        assert!(notifies(&model, TEXT, &role));
        assert!(
            !notifies(&model, TEXT, &ping(false, false, &[99])),
            "not my role"
        );
        settings(&mut model, Some(GUILD)).suppress_everyone = true;
        settings(&mut model, Some(GUILD)).suppress_roles = true;
        assert!(!notifies(&model, TEXT, &everyone));
        assert!(!notifies(&model, TEXT, &role));
        assert!(notifies(&model, TEXT, &ping(true, true, &[ROLE])));
    }

    #[test]
    fn never_mine_silent_blocked_spam_seen_or_while_quiet_or_do_not_disturb() {
        let mut model = model();
        let hi = |id, author| message(id, author, "hi");
        let quiet = Ping::default();
        assert!(!wanted(
            &model,
            DM,
            None,
            &hi(START + 1, ME),
            &quiet,
            away()
        ));
        let silent = Ping {
            silent: true,
            ..ping(true, false, &[])
        };
        assert!(!wanted(
            &model,
            DM,
            None,
            &hi(START + 1, FRIEND),
            &silent,
            away()
        ));
        assert!(
            !wanted(&model, DM, None, &hi(START, FRIEND), &quiet, away()),
            "a repeated event"
        );
        assert!(!wanted(
            &model,
            999,
            None,
            &hi(START + 1, FRIEND),
            &quiet,
            away()
        ));

        let fresh = hi(START + 1, FRIEND);
        let spam = Ping {
            from_spammer: true,
            ..Ping::default()
        };
        assert!(!wanted(&model, DM, None, &fresh, &spam, away()));
        model.ignored.insert(FRIEND);
        assert!(!wanted(&model, DM, None, &fresh, &quiet, away()));
        model.ignored.clear();
        model.quiet_mode = true;
        assert!(!wanted(&model, DM, None, &fresh, &quiet, away()));
        model.quiet_mode = false;

        // A temporary status ends on its own.
        let fresh = hi(START + 1, FRIEND);
        let at = created_at(fresh.id);
        model.do_not_disturb = Some(Mute { until: None });
        assert!(!wanted(&model, DM, None, &fresh, &quiet, away()));
        model.do_not_disturb = Some(Mute { until: Some(at) });
        assert!(wanted(&model, DM, None, &fresh, &quiet, away()));
    }

    #[test]
    fn the_conversation_on_screen_notifies_only_out_of_focus() {
        let model = model();
        let fresh = message(START + 1, FRIEND, "hi");
        let quiet = Ping::default();
        let reading = |focused, open| Attention { focused, open };
        assert!(!wanted(
            &model,
            DM,
            None,
            &fresh,
            &quiet,
            reading(true, Some(DM))
        ));
        assert!(wanted(
            &model,
            DM,
            None,
            &fresh,
            &quiet,
            reading(false, Some(DM))
        ));
        assert!(wanted(
            &model,
            DM,
            None,
            &fresh,
            &quiet,
            reading(true, Some(GROUP))
        ));
    }

    /// A title with its isolation marks shown as brackets.
    fn shown_title(model: &Model, channel: Id, message: &Message) -> String {
        title(model, channel, message)
            .replace('\u{2068}', "[")
            .replace('\u{2069}', "]")
    }

    #[test]
    fn titles_follow_the_official_client() {
        let mut model = model();
        let from = message(START + 1, FRIEND, "");
        assert_eq!(shown_title(&model, DM, &from), "[lea]");
        assert_eq!(shown_title(&model, GROUP, &from), "[lea] ([lea, marc])");
        assert_eq!(
            shown_title(&model, TEXT, &from),
            "[lea] ([#general], [Talk])"
        );
        assert_eq!(
            shown_title(&model, VOICE, &from),
            "[lea] ([Lounge], [Talk])"
        );
        model.guilds[0].channels[1].parent = None;
        assert_eq!(
            shown_title(&model, TEXT, &from),
            "[lea] ([#general], [Rust])",
            "the guild, without a category"
        );
        model
            .nicknames
            .insert(GUILD, [(FRIEND, "Léa ✨".to_owned())].into());
        assert_eq!(
            shown_title(&model, TEXT, &from),
            "[Léa ✨] ([#general], [Rust])"
        );
        assert_eq!(
            shown_title(&model, DM, &from),
            "[lea]",
            "no nickname in a DM"
        );
    }

    #[test]
    fn titles_keep_names_from_turning_the_line_around() {
        let mut model = model();
        let mut from = message(START + 1, FRIEND, "");
        from.author.username = "\u{202e}evil\u{2069}\n\u{7}name".into();
        model.guilds[0].channels[1].name = "\u{200f}rtl\u{2066}".into();
        assert_eq!(
            shown_title(&model, TEXT, &from),
            "[evilname] ([#rtl], [Talk])"
        );
    }

    #[test]
    fn bodies_are_plain_short_and_hideable() {
        let model = model();
        let now = jiff::Timestamp::UNIX_EPOCH;
        let tz = jiff::tz::TimeZone::UTC;
        let say = |message: &Message, show| body(&model, DM, message, show, now, &tz);
        let mut text = message(START + 1, FRIEND, "**look** <@2> a<b && ||x||");
        assert_eq!(say(&text, true), "look @lea a<b && (spoiler)");
        assert_eq!(
            escape("a<b> && c"),
            "a&lt;b&gt; &amp;&amp; c",
            "for markup servers"
        );
        assert_eq!(say(&text, false), "New message");
        text.content = "é".repeat(300);
        assert_eq!(say(&text, true), format!("{}…", "é".repeat(250)));

        let mut empty = message(START + 1, FRIEND, "");
        empty.attachments.push(Attachment {
            id: 1,
            filename: "cat.png".into(),
            url: String::new(),
            proxy_url: String::new(),
            content_type: None,
            size: 0,
            width: None,
            height: None,
            flags: 0,
        });
        assert_eq!(say(&empty, true), "Uploaded cat.png");
        empty.embeds.push(Embed {
            kind: None,
            title: Some("Title".into()),
            description: Some("text".into()),
            url: None,
            color: None,
            author: None,
            footer: None,
            provider: None,
            fields: vec![],
            thumbnail: None,
            image: None,
        });
        assert_eq!(say(&empty, true), "Title text");
    }

    #[test]
    fn a_burst_notifies_four_new_channels_in_ten_seconds() {
        let mut throttle = Throttle::default();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let admitted: Vec<bool> = [(0, 1), (1, 2), (2, 3), (3, 4), (4, 5), (9, 6), (10, 7)]
            .into_iter()
            .map(|(s, channel)| throttle.admit(channel, false, at(s)))
            .collect();
        assert_eq!(admitted, [true, true, true, true, false, false, true]);
    }

    #[test]
    fn a_burst_never_starves_dms_mentions_or_replacements() {
        let mut throttle = Throttle::default();
        let now = Instant::now();
        for channel in 1..=4 {
            assert!(throttle.admit(channel, false, now));
        }
        assert!(!throttle.admit(5, false, now), "the burst is full");
        assert!(throttle.admit(1, false, now), "replaces channel 1's own");
        assert!(throttle.admit(DM, true, now), "a DM or a mention");
        assert!(throttle.admit(6, true, now));
        assert!(!throttle.admit(5, false, now), "neither took a slot");
        throttle.forget(1);
        assert!(!throttle.admit(1, false, now), "read: a new one again");
    }

    fn show(channel: Id, message: Id) -> Notice {
        Notice::Show(Notification {
            channel,
            message,
            title: String::new(),
            body: String::new(),
        })
    }

    #[test]
    fn stale_notices_are_dropped_before_the_server_sees_them() {
        let read = |channel, message| Notice::Read { channel, message };
        let queued = [
            show(DM, 10),
            show(TEXT, 11),
            show(DM, 12),
            read(TEXT, 11),
            read(TEXT, 13),
            show(GROUP, 14),
        ];
        assert_eq!(
            compact(queued),
            [show(DM, 12), read(TEXT, 13), show(GROUP, 14)],
            "replaced, read at once, and older reads merged"
        );
        let queued = [
            show(DM, 10),
            read(GROUP, 1),
            Notice::ClearAll,
            show(TEXT, 11),
        ];
        assert_eq!(compact(queued), [Notice::ClearAll, show(TEXT, 11)]);
        // However long the backlog, one notification per channel is left.
        let backlog = (0..1000).map(|n| show(n % 3, n));
        assert_eq!(compact(backlog).len(), 3);
    }

    #[test]
    fn a_new_notification_server_starts_from_nothing() {
        let mut state = ServerState {
            owner: Some(":1.5".into()),
            markup: Some(false),
            ..ServerState::default()
        };
        assert!(state.ours(Some(":1.5")));
        assert!(!state.ours(Some(":1.9")), "another sender");
        assert!(!state.ours(None));
        let Notice::Show(note) = show(DM, 10) else {
            unreachable!()
        };
        state.shown.shown(&note, 7);
        state.owner_changed(Some(":1.5".into()), Some(":1.9".into()));
        assert_eq!(state.shown.clicked(7), None, "the old server's ids");
        assert_eq!(state.markup, None, "asked again");
        assert!(state.ours(Some(":1.9")));

        // Activated by our first notification: what it shows stays ours.
        let mut fresh = ServerState::default();
        fresh.shown.shown(&note, 1);
        fresh.owner_changed(None, Some(":1.2".into()));
        assert_eq!(fresh.shown.clicked(1), Some(DM));
    }

    #[test]
    fn shown_notifications_go_when_read_up_to_them() {
        let mut shown = Shown::default();
        let note = |channel, message| Notification {
            channel,
            message,
            title: String::new(),
            body: String::new(),
        };
        shown.shown(&note(DM, 50), 7);
        shown.shown(&note(TEXT, 60), 8);
        assert_eq!(shown.replaces(DM), 7);
        assert_eq!(shown.clicked(8), Some(TEXT));
        assert_eq!(shown.read(DM, 49), None, "an older message read");
        assert_eq!(shown.read(DM, 50), Some(7));
        assert_eq!(shown.replaces(DM), 0);
        shown.closed(8);
        assert_eq!(shown.clicked(8), None, "expired: forgotten");
        shown.shown(&note(DM, 51), 9);
        assert_eq!(shown.clear(), [9]);
        assert_eq!(shown.clicked(9), None, "a click after logging out");
    }

    fn create(id: Id, channel: Id, author: Id) -> Update {
        Update::MessageCreate {
            channel,
            guild: None,
            message: message(id, author, "hi"),
            ping: Ping::default(),
        }
    }

    #[test]
    fn follows_the_gateway_with_its_own_model() {
        let shared = Arc::new(Shared::default());
        let mut notifications = Notifications::new(shared.clone());
        let now = Instant::now();
        assert_eq!(
            notifications.follow(&create(START + 1, DM, FRIEND), now),
            None
        );
        notifications.ready(&model());

        let shown = notifications.follow(&create(START + 1, DM, FRIEND), now);
        let Some(Notice::Show(shown)) = shown else {
            panic!("expected a notification, got {shown:?}");
        };
        assert_eq!((shown.channel, shown.message), (DM, START + 1));
        assert_eq!(shown.body, "hi");
        assert_eq!(
            notifications.follow(&create(START + 1, DM, FRIEND), now),
            None,
            "the same message again"
        );

        shared.set_show_content(false);
        let hidden = notifications.follow(&create(START + 2, GROUP, FRIEND), now);
        assert!(matches!(hidden, Some(Notice::Show(n)) if n.body == "New message"));

        shared.set_attention(Attention {
            focused: true,
            open: Some(DM),
        });
        assert_eq!(
            notifications.follow(&create(START + 3, DM, FRIEND), now),
            None
        );

        let read = Update::Acked {
            channel: DM,
            message: START + 3,
            manual: false,
            mentions: None,
            flags: None,
        };
        let read_up_to = Notice::Read {
            channel: DM,
            message: START + 3,
        };
        assert_eq!(notifications.follow(&read, now), Some(read_up_to));
        let status = Update::DoNotDisturb(Some(Mute { until: None }));
        assert_eq!(notifications.follow(&status, now), None);
        shared.set_attention(away());
        assert_eq!(
            notifications.follow(&create(START + 4, DM, FRIEND), now),
            None
        );
        assert!(notifications.model.as_ref().unwrap().messages.is_empty());
    }
}
