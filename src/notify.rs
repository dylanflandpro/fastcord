//! Desktop notifications for new messages.
//!
//! The decision is made on the backend's thread as each MESSAGE_CREATE
//! arrives, against a copy of the model that keeps no history: a window on
//! a hidden workspace may not draw, and a notification must not wait for
//! it. The window reports what it shows through [`Shared`].
//!
//! Rules follow the web client's notification store (`shouldNotify`): not
//! my own messages, not @silent ones, not while my status is Do Not
//! Disturb, not the conversation open in a focused window, and otherwise
//! what my notification settings let through ([`Model::notifies`]).

use crate::events::Update;
use crate::markdown::{self, Directory};
use crate::model::{ChannelKind, Id, Message, Model, Ping, created_at};
use std::collections::VecDeque;
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
    pub title: String,
    /// Plain text, escaped for servers that read markup.
    pub body: String,
}

/// What the notifier is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Show(Notification),
    /// The channel was read: its notification goes, as in the official
    /// client.
    Clear(Id),
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
        && !model.do_not_disturb.is_some_and(|dnd| dnd.active(now))
        && !(attention.focused && attention.open == Some(channel))
        && model.unseen(channel, guild, message.id)
        && model.notifies(channel, ping, now)
}

/// The official client's title: the author, then in a guild the channel and
/// its category ("Léa (#général, Discussions)"), in a group DM its members.
/// The guild's name is not in it: Discord shows the guild's icon instead.
pub fn title(model: &Model, channel: Id, message: &Message) -> String {
    let author = message.author.display_name();
    if let Some(dm) = model.dm(channel) {
        return match dm.recipients.len() {
            0 | 1 => author.to_owned(),
            _ => format!("{author} ({})", dm.title()),
        };
    }
    let Some((guild, channel)) = model
        .guilds
        .iter()
        .find_map(|g| g.channel(channel).map(|c| (g, c)))
    else {
        return author.to_owned();
    };
    let hash = if channel.kind == ChannelKind::Voice {
        ""
    } else {
        "#"
    };
    match channel.parent.and_then(|p| guild.channel(p)) {
        Some(category) => format!("{author} ({hash}{}, {})", channel.name, category.name),
        None => format!("{author} ({hash}{})", channel.name),
    }
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
        return "New message".to_owned();
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
    escape(&truncate(&text, BODY_CHARS))
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
        None => text.to_owned(),
    }
}

/// Notification servers may read the body as markup: a message's `<` and
/// `&` must stay text.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// At most this many notifications in [`BURST_WINDOW`]; past it, messages
/// notify nothing until the window moves on (their badges still count).
/// Each channel already shows one notification at a time, so a busy
/// conversation updates its own instead of piling up.
const BURST: usize = 4;
const BURST_WINDOW: Duration = Duration::from_secs(10);

/// Keeps a burst of messages from flooding the desktop.
#[derive(Debug, Default)]
pub struct Throttle {
    shown: VecDeque<Instant>,
}

impl Throttle {
    /// Whether a notification may show now, counting it if so.
    pub fn admit(&mut self, now: Instant) -> bool {
        while self
            .shown
            .front()
            .is_some_and(|&at| now.duration_since(at) >= BURST_WINDOW)
        {
            self.shown.pop_front();
        }
        let admitted = self.shown.len() < BURST;
        if admitted {
            self.shown.push_back(now);
        }
        admitted
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
                (wanted(model, *channel, *guild, message, ping, attention)
                    && self.throttle.admit(now))
                .then(|| {
                    let wall = jiff::Timestamp::now();
                    let tz = jiff::tz::TimeZone::system();
                    let show = self.shared.show_content();
                    Notice::Show(Notification {
                        channel: *channel,
                        title: title(model, *channel, message),
                        body: body(model, *channel, message, show, wall, &tz),
                    })
                })
            }
            Update::Acked {
                channel,
                manual: false,
                ..
            } => Some(Notice::Clear(*channel)),
            _ => None,
        };
        model.apply(update.clone());
        notice
    }
}

/// The desktop's notifier, on its own thread: a D-Bus round trip never
/// holds up the gateway. `opened` runs when a notification is clicked,
/// with its channel. Never used in tests.
pub fn desktop(opened: impl Fn(Id) + Send + 'static) -> impl Fn(Notice) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let started = std::thread::Builder::new()
        .name("notifications".into())
        .spawn(move || serve_desktop(receiver, opened));
    if let Err(error) = started {
        log::warn!("notifications are off: {error}");
    }
    move |notice| {
        let _ = sender.send(notice);
    }
}

fn serve_desktop(
    mut notices: tokio::sync::mpsc::UnboundedReceiver<Notice>,
    opened: impl Fn(Id) + 'static,
) {
    use notify_rust::{Hint, NotificationHandle, NotificationResponse};
    use std::rc::Rc;
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        log::warn!("notifications are off: no runtime");
        return;
    };
    let opened = Rc::new(opened);
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        // Per channel, its notification and the task waiting for a click.
        let mut shown: std::collections::HashMap<
            Id,
            (Rc<NotificationHandle>, tokio::task::JoinHandle<()>),
        > = Default::default();
        while let Some(notice) = notices.recv().await {
            match notice {
                Notice::Show(notification) => {
                    let channel = notification.channel;
                    let mut desktop = notify_rust::Notification::new();
                    desktop
                        .appname("fastcord")
                        .summary(&notification.title)
                        .body(&notification.body)
                        .action("default", "Open")
                        .hint(Hint::Category("im.received".into()))
                        .hint(Hint::DesktopEntry("fastcord".into()));
                    if let Some((previous, waiting)) = shown.remove(&channel) {
                        waiting.abort();
                        desktop.id(previous.id());
                    }
                    let handle = match desktop.show_async().await {
                        Ok(handle) => Rc::new(handle),
                        Err(error) => {
                            log::warn!("a notification could not be shown: {error}");
                            continue;
                        }
                    };
                    let (clicked, waited) = (opened.clone(), handle.clone());
                    let waiting = tokio::task::spawn_local(async move {
                        waited
                            .wait_for_action_async(|response| {
                                if matches!(response, NotificationResponse::Default) {
                                    clicked(channel);
                                }
                            })
                            .await;
                    });
                    shown.insert(channel, (handle, waiting));
                }
                Notice::Clear(channel) => {
                    if let Some((handle, waiting)) = shown.remove(&channel) {
                        waiting.abort();
                        handle.close_async().await;
                    }
                }
            }
        }
    });
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
        let dm = |id, recipients| DmChannel {
            id,
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
        }
    }

    fn ping(me: bool, everyone: bool, roles: &[Id]) -> Ping {
        Ping {
            me,
            everyone,
            roles: roles.to_vec(),
            silent: false,
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
    fn never_mine_silent_seen_or_while_do_not_disturb() {
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

    #[test]
    fn titles_follow_the_official_client() {
        let model = model();
        let from = message(START + 1, FRIEND, "");
        assert_eq!(title(&model, DM, &from), "lea");
        assert_eq!(title(&model, GROUP, &from), "lea (lea, marc)");
        assert_eq!(title(&model, TEXT, &from), "lea (#general, Talk)");
        assert_eq!(title(&model, VOICE, &from), "lea (Lounge, Talk)");
    }

    #[test]
    fn bodies_are_plain_escaped_short_and_hideable() {
        let model = model();
        let now = jiff::Timestamp::UNIX_EPOCH;
        let tz = jiff::tz::TimeZone::UTC;
        let say = |message: &Message, show| body(&model, DM, message, show, now, &tz);
        let mut text = message(START + 1, FRIEND, "**look** <@2> a<b && ||x||");
        assert_eq!(say(&text, true), "look @lea a&lt;b &amp;&amp; (spoiler)");
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
    fn a_burst_notifies_four_times_in_ten_seconds() {
        let mut throttle = Throttle::default();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let admitted: Vec<bool> = [0, 1, 2, 3, 4, 9, 10, 11]
            .into_iter()
            .map(|s| throttle.admit(at(s)))
            .collect();
        assert_eq!(admitted, [true, true, true, true, false, false, true, true]);
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
        assert_eq!((shown.channel, shown.title.as_str()), (DM, "lea"));
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
        assert_eq!(notifications.follow(&read, now), Some(Notice::Clear(DM)));
        let status = Update::DoNotDisturb(Some(Mute { until: None }));
        assert_eq!(notifications.follow(&status, now), None);
        shared.set_attention(away());
        assert_eq!(
            notifications.follow(&create(START + 4, DM, FRIEND), now),
            None
        );
        assert!(notifications.model.as_ref().unwrap().messages.is_empty());
    }

    #[test]
    fn a_burst_across_channels_is_cut_short() {
        let mut notifications = Notifications::new(Arc::new(Shared::default()));
        notifications.ready(&model());
        let now = Instant::now();
        let shown = (1..=6)
            .filter(|&n| {
                let channel = if n % 2 == 0 { DM } else { GROUP };
                notifications
                    .follow(&create(START + n, channel, FRIEND), now)
                    .is_some()
            })
            .count();
        assert_eq!(shown, BURST);
    }
}
