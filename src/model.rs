//! What the client knows about the account, kept in memory only.
//!
//! The shapes follow Discord's own objects closely enough that the gateway
//! can fill them later, and no further than the interface needs.

use std::collections::{HashMap, HashSet};

/// A Discord snowflake: guilds, channels, users and messages all use one.
pub type Id = u64;

/// Milliseconds between the Unix epoch and Discord's (2015-01-01).
pub const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

/// When a snowflake was created. Every message carries its time this way.
pub fn created_at(id: Id) -> jiff::Timestamp {
    let ms = (id >> 22) as i64 + DISCORD_EPOCH_MS;
    jiff::Timestamp::from_millisecond(ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

/// The snowflake for a moment, with the other bits zero. Demo data uses it
/// to give messages believable times.
pub fn id_at(timestamp: jiff::Timestamp, sequence: u64) -> Id {
    let ms = (timestamp.as_millisecond() - DISCORD_EPOCH_MS).max(0) as u64;
    (ms << 22) | (sequence & 0xfff)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct User {
    pub id: Id,
    pub username: String,
    /// The name people chose to show, when it differs from the username.
    pub global_name: Option<String>,
}

impl User {
    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().unwrap_or(&self.username)
    }
}

/// A permission bitfield, as Discord sends it (a decimal string on the wire).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Permissions(pub u64);

impl Permissions {
    pub const ADMINISTRATOR: Self = Self(1 << 3);
    pub const VIEW_CHANNEL: Self = Self(1 << 10);
    pub const ALL: Self = Self(u64::MAX);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Denies, then allows: within one overwrite, allow wins.
    fn overwrite(self, allow: Self, deny: Self) -> Self {
        self.difference(deny).union(allow)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Role {
    /// The @everyone role has the guild's id.
    pub id: Id,
    /// What role mentions show.
    pub name: String,
    pub position: i32,
    pub permissions: Permissions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverwriteKind {
    Role,
    Member,
}

/// A channel's exception to a role's or a member's permissions.
#[derive(Clone, Debug, PartialEq)]
pub struct Overwrite {
    pub id: Id,
    pub kind: OverwriteKind,
    pub allow: Permissions,
    pub deny: Permissions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelKind {
    Text,
    Voice,
    Announcement,
    Category,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    pub id: Id,
    pub name: String,
    pub kind: ChannelKind,
    /// The category it sits under, if any.
    pub parent: Option<Id>,
    pub position: i32,
    pub overwrites: Vec<Overwrite>,
    /// The newest message, which tells whether the channel is unread.
    pub last_message_id: Option<Id>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Guild {
    pub id: Id,
    pub name: String,
    pub channels: Vec<Channel>,
    pub owner_id: Id,
    pub roles: Vec<Role>,
    /// The signed-in member's roles.
    pub my_roles: Vec<Id>,
    /// When I joined: a channel I never read is unread from then on.
    pub joined_at: Option<jiff::Timestamp>,
    /// What notifies members who left the guild's setting alone.
    pub default_notify: Notify,
}

/// One line of a guild's channel list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Entry<'a> {
    Category(&'a Channel),
    Channel(&'a Channel),
}

impl Guild {
    /// What `me` may do across the guild, before any channel overwrite: the
    /// @everyone role and each of my roles combined. The owner and
    /// administrators may do everything.
    fn base_permissions(&self, me: Id) -> Permissions {
        if self.owner_id == me {
            return Permissions::ALL;
        }
        let base = self
            .roles
            .iter()
            .filter(|r| r.id == self.id || self.my_roles.contains(&r.id))
            .fold(Permissions::default(), |p, r| p.union(r.permissions));
        if base.contains(Permissions::ADMINISTRATOR) {
            Permissions::ALL
        } else {
            base
        }
    }

    /// What `me` may do in `channel`, following Discord's documented order:
    /// the @everyone overwrite, then all my roles' overwrites as one, then my
    /// own member overwrite, each denying before it allows.
    pub fn permissions(&self, channel: &Channel, me: Id) -> Permissions {
        self.overwritten(self.base_permissions(me), channel, me)
    }

    /// [`Self::permissions`] from base permissions already worked out, so a
    /// whole channel list computes them once.
    fn overwritten(&self, base: Permissions, channel: &Channel, me: Id) -> Permissions {
        if base == Permissions::ALL {
            return base;
        }
        let find = |kind, id| {
            channel
                .overwrites
                .iter()
                .find(|o| o.kind == kind && o.id == id)
        };
        let mut permissions = base;
        if let Some(o) = find(OverwriteKind::Role, self.id) {
            permissions = permissions.overwrite(o.allow, o.deny);
        }
        let (allow, deny) = channel
            .overwrites
            .iter()
            // @everyone's overwrite was applied above, even if my roles list it.
            .filter(|o| {
                o.kind == OverwriteKind::Role && o.id != self.id && self.my_roles.contains(&o.id)
            })
            .fold(
                (Permissions::default(), Permissions::default()),
                |(a, d), o| (a.union(o.allow), d.union(o.deny)),
            );
        permissions = permissions.overwrite(allow, deny);
        if let Some(o) = find(OverwriteKind::Member, me) {
            permissions = permissions.overwrite(o.allow, o.deny);
        }
        permissions
    }

    pub fn can_view(&self, channel: &Channel, me: Id) -> bool {
        self.permissions(channel, me)
            .contains(Permissions::VIEW_CHANNEL)
    }

    /// The channel list as Discord draws it for `me`: channels outside any
    /// category first, then each category followed by its channels. Within a
    /// group, text channels come before voice ones (each kind numbers its
    /// positions from zero), then by position, ties broken by id, as Discord
    /// does. Channels `me` cannot view are left out. A category shows when
    /// one of its channels does, even if the category itself is hidden; one
    /// whose channels are all hidden is left out, as the official client
    /// does; one with no channel at all shows to whoever can view it.
    pub fn sidebar(&self, me: Id) -> Vec<Entry<'_>> {
        // Worked out once: the list is drawn every frame.
        let base = self.base_permissions(me);
        let visible = |c: &Channel| {
            self.overwritten(base, c, me)
                .contains(Permissions::VIEW_CHANNEL)
        };
        let mut categories: Vec<&Channel> = self
            .channels
            .iter()
            .filter(|c| c.kind == ChannelKind::Category)
            .collect();
        categories.sort_by_key(|c| (c.position, c.id));
        // A parent the guild does not list (hidden, or not loaded yet) leaves
        // the channel loose rather than lost.
        let parent = |c: &Channel| {
            c.parent
                .filter(|p| categories.iter().any(|cat| cat.id == *p))
        };
        let children = |category: Option<Id>| {
            let mut channels: Vec<&Channel> = self
                .channels
                .iter()
                .filter(|c| c.kind != ChannelKind::Category && parent(c) == category)
                .filter(|c| visible(c))
                .collect();
            channels.sort_by_key(|c| (c.kind == ChannelKind::Voice, c.position, c.id));
            channels
        };

        let mut entries: Vec<Entry<'_>> = children(None).into_iter().map(Entry::Channel).collect();
        for &category in &categories {
            let channels = children(Some(category.id));
            let empty = !self.channels.iter().any(|c| c.parent == Some(category.id));
            if channels.is_empty() && !(empty && visible(category)) {
                continue;
            }
            entries.push(Entry::Category(category));
            entries.extend(channels.into_iter().map(Entry::Channel));
        }
        entries
    }

    /// The first channel `me` can open, for a guild opened afresh.
    pub fn first_text_channel(&self, me: Id) -> Option<Id> {
        self.sidebar(me).into_iter().find_map(|entry| match entry {
            Entry::Channel(c) if c.kind != ChannelKind::Voice => Some(c.id),
            _ => None,
        })
    }

    pub fn channel(&self, id: Id) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }
}

/// A direct message or a group DM.
#[derive(Clone, Debug, PartialEq)]
pub struct DmChannel {
    pub id: Id,
    pub recipients: Vec<User>,
    pub last_message_id: Option<Id>,
}

impl DmChannel {
    pub fn title(&self) -> String {
        self.recipients
            .iter()
            .map(User::display_name)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub id: Id,
    pub author: User,
    pub content: String,
    pub attachments: Vec<Attachment>,
    pub embeds: Vec<Embed>,
}

/// A file sent with a message.
#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub id: Id,
    pub filename: String,
    /// The file on Discord's CDN.
    pub url: String,
    /// The same file through Discord's media proxy, which resizes images.
    pub proxy_url: String,
    pub content_type: Option<String>,
    /// In bytes.
    pub size: u64,
    /// Set for the images and videos Discord could measure.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Discord's attachment flags, such as [`Attachment::SPOILER`].
    pub flags: u64,
}

impl Attachment {
    /// Sent behind a spoiler.
    pub const SPOILER: u64 = 1 << 3;
}

/// A picture in an embed. `url` is wherever the link pointed; only
/// `proxy_url`, Discord's copy, is ever loaded.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbedImage {
    pub url: String,
    pub proxy_url: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

/// A link preview, or a bot's rich message.
#[derive(Clone, Debug, PartialEq)]
pub struct Embed {
    /// Discord's `type`: "rich", "image", "gifv", "video", "article"…
    pub kind: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
    /// 0xRRGGBB, for the bar down the left.
    pub color: Option<u32>,
    pub author: Option<String>,
    pub footer: Option<String>,
    pub provider: Option<String>,
    pub fields: Vec<EmbedField>,
    pub thumbnail: Option<EmbedImage>,
    pub image: Option<EmbedImage>,
}

/// Messages from one author close enough in time to share a header.
const GROUP_WINDOW: jiff::SignedDuration = jiff::SignedDuration::from_mins(7);

/// Whether `message` starts a new group after `previous`: another author, or
/// more than seven minutes later, as the official client groups them.
pub fn starts_group(previous: Option<&Message>, message: &Message) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    previous.author.id != message.author.id
        || created_at(message.id).duration_since(created_at(previous.id)) > GROUP_WINDOW
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    /// The signed-in user.
    pub me: Id,
    pub guilds: Vec<Guild>,
    pub dms: Vec<DmChannel>,
    /// The people the client knows by id: READY's users, DM recipients and
    /// message authors, for mentions and DM titles.
    pub users: HashMap<Id, User>,
    /// Loaded history per channel, oldest first. A channel is absent until
    /// its history has been asked for and arrived.
    pub messages: HashMap<Id, Vec<Message>>,
    /// Channels whose history is loaded back to the first message.
    pub complete: HashSet<Id>,
    /// Per channel, the oldest message fetched, shown or not: where the next
    /// page of history starts.
    pub cursors: HashMap<Id, Id>,
    /// How far I have read each channel and DM.
    pub read_states: HashMap<Id, ReadState>,
    /// My notification settings per guild; `None` holds the DMs'.
    pub guild_settings: HashMap<Option<Id>, GuildSettings>,
    /// Whether the account's unread settings stand apart from its
    /// notification settings (Discord's "new notifications").
    pub separate_unreads: bool,
    /// My status is Do Not Disturb, for good or until the time Discord set:
    /// nothing notifies meanwhile.
    pub do_not_disturb: Option<Mute>,
    /// The account's quiet mode: nothing notifies while it is on.
    pub quiet_mode: bool,
    /// People I blocked, and people I ignored: nothing they send notifies.
    pub blocked: HashSet<Id>,
    pub ignored: HashSet<Id>,
    /// Nicknames by guild, then user, as messages report them.
    pub nicknames: HashMap<Id, HashMap<Id, String>>,
}

impl Model {
    pub fn guild(&self, id: Id) -> Option<&Guild> {
        self.guilds.iter().find(|g| g.id == id)
    }

    pub fn dm(&self, id: Id) -> Option<&DmChannel> {
        self.dms.iter().find(|d| d.id == id)
    }

    /// DMs with the most recent conversation first.
    pub fn dms_by_recency(&self) -> Vec<&DmChannel> {
        let mut dms: Vec<&DmChannel> = self.dms.iter().collect();
        dms.sort_by_key(|d| std::cmp::Reverse(d.last_message_id));
        dms
    }

    /// The web client's `isBlockedOrIgnored`.
    pub fn blocked_or_ignored(&self, user: Id) -> bool {
        self.blocked.contains(&user) || self.ignored.contains(&user)
    }

    /// What a guild calls someone: their nickname there, else their name.
    pub fn name_in(&self, guild: Option<Id>, user: &User) -> String {
        guild
            .and_then(|g| self.nicknames.get(&g)?.get(&user.id))
            .cloned()
            .unwrap_or_else(|| user.display_name().to_owned())
    }

    pub fn messages(&self, channel: Id) -> &[Message] {
        self.messages.get(&channel).map_or(&[], Vec::as_slice)
    }

    /// The guild a channel belongs to; `None` for DMs and unknown channels.
    pub fn guild_of(&self, channel: Id) -> Option<Id> {
        self.guilds
            .iter()
            .find(|g| g.channel(channel).is_some())
            .map(|g| g.id)
    }

    fn knows_channel(&self, channel: Id) -> bool {
        self.guild_of(channel).is_some() || self.dm(channel).is_some()
    }

    /// Drops what was loaded for a channel that went away.
    fn forget_history(&mut self, channel: Id) {
        self.messages.remove(&channel);
        self.complete.remove(&channel);
        self.cursors.remove(&channel);
    }
    /// Applies a change the gateway reported after READY.
    pub fn apply(&mut self, update: crate::events::Update) {
        use crate::events::Update;
        match update {
            Update::GuildUpsert(guild) => match self.guilds.iter_mut().find(|g| g.id == guild.id) {
                Some(existing) => *existing = guild,
                None => self.guilds.push(guild),
            },
            Update::GuildChanged { id, name, owner_id } => {
                if let Some(guild) = self.guilds.iter_mut().find(|g| g.id == id) {
                    if let Some(name) = name {
                        guild.name = name;
                    }
                    if let Some(owner_id) = owner_id {
                        guild.owner_id = owner_id;
                    }
                }
            }
            Update::GuildRemove(id) => {
                if let Some(guild) = self.guild(id) {
                    let channels: Vec<Id> = guild.channels.iter().map(|c| c.id).collect();
                    channels.into_iter().for_each(|c| self.forget_history(c));
                }
                self.guilds.retain(|g| g.id != id);
            }
            Update::ChannelUpsert { guild, channel } => {
                if let Some(guild) = self.guilds.iter_mut().find(|g| g.id == guild) {
                    match guild.channels.iter_mut().find(|c| c.id == channel.id) {
                        Some(existing) => *existing = channel,
                        None => guild.channels.push(channel),
                    }
                }
            }
            Update::RoleUpsert { guild, role } => {
                if let Some(guild) = self.guilds.iter_mut().find(|g| g.id == guild) {
                    match guild.roles.iter_mut().find(|r| r.id == role.id) {
                        Some(existing) => *existing = role,
                        None => guild.roles.push(role),
                    }
                }
            }
            Update::RoleRemove { guild, role } => {
                if let Some(guild) = self.guilds.iter_mut().find(|g| g.id == guild) {
                    guild.roles.retain(|r| r.id != role);
                    guild.my_roles.retain(|&r| r != role);
                }
            }
            Update::MyRoles { guild, roles } => {
                if let Some(guild) = self.guilds.iter_mut().find(|g| g.id == guild) {
                    guild.my_roles = roles;
                }
            }
            Update::ChannelRemove { guild, channel } => {
                if let Some(guild) = self.guilds.iter_mut().find(|g| g.id == guild) {
                    guild.channels.retain(|c| c.id != channel);
                }
                self.forget_history(channel);
            }
            Update::DmUpsert(dm) => match self.dms.iter_mut().find(|d| d.id == dm.id) {
                Some(existing) => *existing = dm,
                None => self.dms.push(dm),
            },
            Update::History {
                channel,
                messages,
                oldest,
                complete,
            } => {
                // A page for a channel deleted while it loaded.
                if !self.knows_channel(channel) {
                    return;
                }
                if let Some(oldest) = oldest {
                    let cursor = self.cursors.entry(channel).or_insert(oldest);
                    *cursor = (*cursor).min(oldest);
                }
                for message in &messages {
                    self.users.insert(message.author.id, message.author.clone());
                }
                let loaded = self.messages.entry(channel).or_default();
                loaded.extend(messages);
                loaded.sort_by_key(|m| m.id);
                loaded.dedup_by_key(|m| m.id);
                if complete {
                    self.complete.insert(channel);
                }
            }
            Update::MessageCreate {
                channel,
                guild,
                message,
                ping,
            } => {
                self.users.insert(message.author.id, message.author.clone());
                self.count_message(channel, guild, &message, &ping);
                if let Some(dm) = self.dms.iter_mut().find(|d| d.id == channel) {
                    dm.last_message_id = dm.last_message_id.max(Some(message.id));
                }
                self.bump_last_message(guild, channel, Some(message.id));
                // A channel whose history was never asked for gets it, this
                // message included, when it is opened.
                if let Some(loaded) = self.messages.get_mut(&channel)
                    && !loaded.iter().any(|m| m.id == message.id)
                {
                    let at = loaded.partition_point(|m| m.id < message.id);
                    loaded.insert(at, message);
                }
            }
            Update::MessageEdit {
                channel,
                id,
                content,
                attachments,
                embeds,
            } => {
                if let Some(message) = self
                    .messages
                    .get_mut(&channel)
                    .and_then(|loaded| loaded.iter_mut().find(|m| m.id == id))
                {
                    if let Some(content) = content {
                        message.content = content;
                    }
                    if let Some(attachments) = attachments {
                        message.attachments = attachments;
                    }
                    if let Some(embeds) = embeds {
                        message.embeds = embeds;
                    }
                }
            }
            Update::MessageDelete { channel, ids } => {
                if let Some(loaded) = self.messages.get_mut(&channel) {
                    loaded.retain(|m| !ids.contains(&m.id));
                }
            }
            Update::DmRemove(id) => {
                self.dms.retain(|d| d.id != id);
                self.forget_history(id);
            }
            Update::Acked {
                channel,
                message,
                manual,
                mentions,
                flags,
            } => self.acked(channel, message, manual, mentions, flags),
            Update::GuildSettings { guild, settings } => {
                self.guild_settings.insert(guild, settings);
            }
            Update::LastMessages { guild, channels } => {
                for (channel, last) in channels {
                    self.bump_last_message(Some(guild), channel, last);
                }
            }
            Update::DoNotDisturb(status) => self.do_not_disturb = status,
            Update::QuietMode(on) => self.quiet_mode = on,
            Update::Relationship {
                user,
                blocked,
                ignored,
            } => {
                for (set, value) in [(&mut self.blocked, blocked), (&mut self.ignored, ignored)] {
                    match value {
                        Some(true) => set.insert(user),
                        Some(false) => set.remove(&user),
                        None => false,
                    };
                }
            }
            Update::People { guild, people } => {
                for (user, nick) in people {
                    if let (Some(guild), Some(nick)) = (guild, nick) {
                        let nicknames = self.nicknames.entry(guild).or_default();
                        match nick {
                            Some(nick) => nicknames.insert(user.id, nick),
                            None => nicknames.remove(&user.id),
                        };
                    }
                    self.users.insert(user.id, user);
                }
            }
        }
    }

    /// Moves a guild channel's newest message forward, never back: a late
    /// or partial report must not hide newer messages.
    fn bump_last_message(&mut self, guild: Option<Id>, channel: Id, last: Option<Id>) {
        if let Some(channel) = self
            .guilds
            .iter_mut()
            .filter(|g| guild.is_none_or(|id| g.id == id))
            .find_map(|g| g.channels.iter_mut().find(|c| c.id == channel))
        {
            channel.last_message_id = channel.last_message_id.max(last);
        }
    }
}

// What I have read, what I muted, and the badges they make. The rules
// follow the web client's read state and guild settings stores.

/// How far I have read a channel or DM, as Discord keeps it per account.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadState {
    /// The last message read; any later one is unread. `None` when Discord
    /// has no position on record.
    pub last_read: Option<Id>,
    /// Unread mentions of me. In a DM every message counts, unless it is
    /// muted: Discord counts them so.
    pub mentions: u32,
    /// Discord's read state flags, which an ack sends back when they change.
    pub flags: Option<u32>,
}

/// The read state flag for a channel that belongs to a guild.
const IS_GUILD_CHANNEL: u32 = 1 << 0;

/// A read to report to Discord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ack {
    /// `None` for a DM.
    pub guild: Option<Id>,
    pub channel: Id,
    pub message: Id,
    /// The read state's flags, when they changed.
    pub flags: Option<u32>,
    /// Sent at once rather than after the usual delay: the channel had
    /// mentions, which the web client clears without waiting.
    pub immediate: bool,
}

/// A mute, for good or until a time Discord set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mute {
    pub until: Option<jiff::Timestamp>,
}

impl Mute {
    /// A temporary mute ends on its own: Discord sends nothing when it does.
    pub fn active(self, now: jiff::Timestamp) -> bool {
        self.until.is_none_or(|end| now < end)
    }
}

/// Which messages notify me (Discord's `message_notifications`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notify {
    All,
    Mentions,
    Nothing,
}

/// Which messages mark a channel unread, when the account separates this
/// from notifications (Discord's "new notifications").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unreads {
    All,
    Mentions,
}

/// My settings for one channel or category; `None` inherits.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ChannelSettings {
    pub muted: Option<Mute>,
    pub notify: Option<Notify>,
    pub unreads: Option<Unreads>,
}

/// My settings in one guild, or across DMs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuildSettings {
    pub muted: Option<Mute>,
    /// `None` follows the guild's default.
    pub notify: Option<Notify>,
    pub unreads: Option<Unreads>,
    pub suppress_everyone: bool,
    pub suppress_roles: bool,
    /// Per channel or category; a category's settings cover its channels.
    pub channels: HashMap<Id, ChannelSettings>,
}

/// Who a message pings, as Discord resolved it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ping {
    /// Mentions me by name.
    pub me: bool,
    /// @everyone or @here, when the author was allowed to.
    pub everyone: bool,
    pub roles: Vec<Id>,
    /// Sent with @silent: it still counts as a mention, but notifies no one.
    pub silent: bool,
    /// Sent by an account Discord flagged as a likely spammer: it never
    /// notifies.
    pub from_spammer: bool,
}

/// What a channel, DM or guild shows beside its name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Badge {
    /// Messages I have not read that the unread settings let through.
    pub unread: bool,
    /// Unread mentions of me. They show even when muted, as in the official
    /// client.
    pub mentions: u32,
    pub muted: bool,
}

/// Whether a message pings me, as Discord counts it: by name always;
/// @everyone (and @here) and my roles unless my settings suppress them.
fn mentions_me(ping: &Ping, settings: Option<&GuildSettings>, my_roles: &[Id]) -> bool {
    let suppress = |pick: fn(&GuildSettings) -> bool| settings.is_some_and(pick);
    ping.me
        || (ping.everyone && !suppress(|s| s.suppress_everyone))
        || (!suppress(|s| s.suppress_roles) && ping.roles.iter().any(|r| my_roles.contains(r)))
}

/// A count as Discord's badges print it: in full below a thousand, then
/// "1k+" up to "9k+".
pub fn badge_count(count: u32) -> String {
    if count < 1000 {
        count.to_string()
    } else {
        format!("{}k+", (count / 1000).min(9))
    }
}

/// One guild's settings, looked up once for all its channels.
struct Scope<'a> {
    guild: &'a Guild,
    settings: Option<&'a GuildSettings>,
    now: jiff::Timestamp,
    /// Whether unreads follow their own setting (Discord's new
    /// notifications); before that, every message marks a channel unread.
    separate_unreads: bool,
}

impl Scope<'_> {
    fn channel(&self, id: Option<Id>) -> ChannelSettings {
        id.and_then(|id| self.settings?.channels.get(&id).copied())
            .unwrap_or_default()
    }

    /// Muted itself or through its category. The guild's own mute only
    /// quiets its rail entry.
    fn muted(&self, channel: &Channel) -> bool {
        [Some(channel.id), channel.parent]
            .into_iter()
            .any(|id| self.channel(id).muted.is_some_and(|m| m.active(self.now)))
    }

    /// The channel's, else its category's, else the guild's, else the
    /// guild's default.
    fn notify(&self, channel: &Channel) -> Notify {
        let own = self.channel(Some(channel.id)).notify;
        own.or(self.channel(channel.parent).notify)
            .or(self.settings.and_then(|s| s.notify))
            .unwrap_or(self.guild.default_notify)
    }

    /// Discord's `resolveUnreadSetting`: an explicit unread setting on the
    /// channel, its category or the guild; otherwise channels that do not
    /// notify on every message mark themselves unread only on mentions.
    fn unreads(&self, channel: &Channel) -> Unreads {
        if !self.separate_unreads {
            return Unreads::All;
        }
        let own = self.channel(Some(channel.id)).unreads;
        own.or(self.channel(channel.parent).unreads)
            .or(self.settings.and_then(|s| s.unreads))
            .unwrap_or(match self.notify(channel) {
                Notify::All => Unreads::All,
                Notify::Mentions | Notify::Nothing => Unreads::Mentions,
            })
    }
}

impl Model {
    fn read_state(&self, channel: Id) -> ReadState {
        self.read_states.get(&channel).copied().unwrap_or_default()
    }

    fn scope<'a>(&'a self, guild: &'a Guild, now: jiff::Timestamp) -> Scope<'a> {
        Scope {
            guild,
            settings: self.guild_settings.get(&Some(guild.id)),
            now,
            separate_unreads: self.separate_unreads,
        }
    }

    /// Where I have read up to: the read state, or, without one, when I
    /// joined the guild (nothing is unread, if that is unknown) or when the
    /// DM began, as the official client counts.
    fn read_up_to(&self, guild: Option<&Guild>, channel: Id) -> Id {
        self.read_state(channel)
            .last_read
            .unwrap_or_else(|| match guild {
                Some(guild) => guild.joined_at.map_or(Id::MAX, |at| id_at(at, 0)),
                None => channel,
            })
    }

    fn behind(&self, guild: Option<&Guild>, channel: Id, last_message: Option<Id>) -> bool {
        last_message.is_some_and(|last| last > self.read_up_to(guild, channel))
    }

    /// A channel's row: unread only for channels I can open, outside a mute
    /// and when its unread setting takes every message.
    fn badge_in(&self, scope: &Scope<'_>, channel: &Channel) -> Badge {
        let muted = scope.muted(channel);
        let mentions = self.read_state(channel.id).mentions;
        // Voice channels' text chat cannot be opened here, so it is never
        // unread; the official client counts it only through mentions too.
        let unread = matches!(channel.kind, ChannelKind::Text | ChannelKind::Announcement)
            && !muted
            && scope.unreads(channel) == Unreads::All
            && self.behind(Some(scope.guild), channel.id, channel.last_message_id);
        Badge {
            unread,
            mentions,
            muted,
        }
    }

    pub fn channel_badge(&self, guild: &Guild, channel: &Channel, now: jiff::Timestamp) -> Badge {
        self.badge_in(&self.scope(guild, now), channel)
    }

    /// A guild's rail entry, over the channels I can view. A channel with
    /// mentions counts as unread even when muted, as in the official
    /// client; a muted guild mutes all its channels this way.
    pub fn guild_badge(&self, guild: &Guild, now: jiff::Timestamp) -> Badge {
        let scope = self.scope(guild, now);
        let base = guild.base_permissions(self.me);
        let muted = scope
            .settings
            .and_then(|s| s.muted)
            .is_some_and(|m| m.active(now));
        let mut total = Badge {
            muted,
            ..Badge::default()
        };
        for channel in &guild.channels {
            if channel.kind == ChannelKind::Category
                || !guild
                    .overwritten(base, channel, self.me)
                    .contains(Permissions::VIEW_CHANNEL)
            {
                continue;
            }
            let state = self.read_state(channel.id);
            total.mentions += state.mentions;
            if total.unread {
                continue;
            }
            total.unread = if state.mentions > 0 {
                self.behind(Some(guild), channel.id, channel.last_message_id)
            } else {
                !muted && self.badge_in(&scope, channel).unread
            };
        }
        total
    }

    fn dm_muted(&self, dm: Id, now: jiff::Timestamp) -> bool {
        self.guild_settings.get(&None).is_some_and(|s| {
            [s.muted, s.channels.get(&dm).and_then(|c| c.muted)]
                .into_iter()
                .flatten()
                .any(|m| m.active(now))
        })
    }

    pub fn dm_badge(&self, dm: &DmChannel, now: jiff::Timestamp) -> Badge {
        let muted = self.dm_muted(dm.id, now);
        Badge {
            unread: !muted && self.behind(None, dm.id, dm.last_message_id),
            mentions: self.read_state(dm.id).mentions,
            muted,
        }
    }

    /// The count on the direct messages button. A muted DM's count still
    /// shows, as in the official client, but only mentions of me add to it.
    pub fn dm_mentions(&self) -> u32 {
        self.dms
            .iter()
            .map(|dm| self.read_state(dm.id).mentions)
            .sum()
    }

    /// What a new message changes in the read state, as the web client
    /// keeps it between acks: my own message marks the channel read up to
    /// it; someone else's counts as a mention when it pings me (in a DM,
    /// every message does unless the DM is muted).
    fn count_message(&mut self, channel: Id, guild: Option<Id>, message: &Message, ping: &Ping) {
        let now = created_at(message.id);
        let guild = guild.and_then(|id| self.guild(id));
        let read_up_to = self.read_up_to(guild, channel);
        // A repeated or late event is not a new message.
        if self
            .newest(guild, channel)
            .is_some_and(|newest| message.id <= newest)
        {
            return;
        }
        let counts = match guild {
            _ if message.author.id == self.me || message.id <= read_up_to => false,
            None => ping.me || !self.dm_muted(channel, now),
            Some(guild) => mentions_me(
                ping,
                self.guild_settings.get(&Some(guild.id)),
                &guild.my_roles,
            ),
        };
        let state = self.read_states.entry(channel).or_default();
        if message.author.id == self.me {
            state.last_read = state.last_read.max(Some(message.id));
            state.mentions = 0;
        } else if counts {
            state.mentions += 1;
        }
    }

    /// The newest message known in a channel or DM.
    fn newest(&self, guild: Option<&Guild>, channel: Id) -> Option<Id> {
        match guild {
            Some(guild) => guild.channel(channel).and_then(|c| c.last_message_id),
            None => self.dm(channel).and_then(|d| d.last_message_id),
        }
    }

    /// Whether `message` is news: newer than the newest message known in its
    /// channel (a repeated or late event is not) and than where I have read.
    pub fn unseen(&self, channel: Id, guild: Option<Id>, message: Id) -> bool {
        let guild = guild.and_then(|id| self.guild(id));
        self.newest(guild, channel)
            .is_none_or(|newest| message > newest)
            && message > self.read_up_to(guild, channel)
    }

    /// Whether my settings let a message in `channel` notify, as the web
    /// client's `shouldNotify` reads them: never in a muted guild, category,
    /// channel or DM (mentions included), nor where the level is "nothing";
    /// every message where it is "all", except in a voice channel's chat,
    /// which only notifies while connected to it (never, here); otherwise
    /// only mentions of me, of @everyone and of my roles, unless my
    /// settings suppress the last two. DMs follow the DM settings and
    /// notify on every message by default.
    pub fn notifies(&self, channel: Id, ping: &Ping, now: jiff::Timestamp) -> bool {
        if self.dm(channel).is_some() {
            let settings = self.guild_settings.get(&None);
            let notify = settings
                .and_then(|s| s.channels.get(&channel).and_then(|c| c.notify).or(s.notify))
                .unwrap_or(Notify::All);
            return !self.dm_muted(channel, now)
                && match notify {
                    Notify::All => true,
                    Notify::Mentions => mentions_me(ping, settings, &[]),
                    Notify::Nothing => false,
                };
        }
        let Some((guild, channel)) = self
            .guilds
            .iter()
            .find_map(|g| g.channel(channel).map(|c| (g, c)))
        else {
            return false;
        };
        let scope = self.scope(guild, now);
        let guild_muted = scope
            .settings
            .and_then(|s| s.muted)
            .is_some_and(|m| m.active(now));
        if guild_muted || scope.muted(channel) {
            return false;
        }
        match scope.notify(channel) {
            Notify::Nothing => false,
            Notify::All if channel.kind != ChannelKind::Voice => true,
            Notify::All | Notify::Mentions => mentions_me(ping, scope.settings, &guild.my_roles),
        }
    }

    /// Marks a channel or DM read up to its newest message, when there is
    /// something to read, and returns the ack to send: once per new
    /// message, so calling this on every frame is free. The position only
    /// moves forward.
    pub fn mark_read(&mut self, channel: Id) -> Option<Ack> {
        let (guild, last, flags) = match self.dm(channel) {
            Some(dm) => (None, dm.last_message_id?, 0),
            None => {
                let (guild, c) = self
                    .guilds
                    .iter()
                    .find_map(|g| g.channel(channel).map(|c| (g, c)))?;
                (Some(guild), c.last_message_id?, IS_GUILD_CHANNEL)
            }
        };
        let state = self.read_state(channel);
        if !self.behind(guild, channel, Some(last)) && state.mentions == 0 {
            return None;
        }
        let message = state.last_read.map_or(last, |read| read.max(last));
        // The flags are recorded once Discord has them (`save_flags`).
        self.read_states.insert(
            channel,
            ReadState {
                last_read: Some(message),
                mentions: 0,
                flags: state.flags,
            },
        );
        Some(Ack {
            guild: guild.map(|g| g.id),
            channel,
            message,
            flags: (state.flags != Some(flags)).then_some(flags),
            immediate: state.mentions > 0,
        })
    }

    /// The flags an ack carried, once Discord saved them.
    pub fn save_flags(&mut self, channel: Id, flags: u32) {
        self.read_states.entry(channel).or_default().flags = Some(flags);
    }

    /// Whether a channel or DM is still there for me to read.
    pub fn can_read(&self, channel: Id) -> bool {
        self.dm(channel).is_some()
            || self
                .guilds
                .iter()
                .any(|g| g.channel(channel).is_some_and(|c| g.can_view(c, self.me)))
    }

    /// A read reported by Discord, from this session or another. A manual
    /// one (marked unread) may move back; others only move forward, so a
    /// late echo of an older ack does not undo a newer read, and the echo
    /// of the current one leaves mentions counted since alone.
    fn acked(
        &mut self,
        channel: Id,
        message: Id,
        manual: bool,
        mentions: Option<u32>,
        flags: Option<u32>,
    ) {
        let state = self.read_states.entry(channel).or_default();
        state.flags = flags.or(state.flags);
        if manual {
            state.last_read = Some(message);
            state.mentions = mentions.unwrap_or(state.mentions);
        } else if state.last_read.is_none_or(|read| message > read) {
            state.last_read = Some(message);
            state.mentions = mentions.unwrap_or(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: Id, kind: ChannelKind, parent: Option<Id>, position: i32) -> Channel {
        Channel {
            id,
            name: id.to_string(),
            kind,
            parent,
            position,
            overwrites: vec![],
            last_message_id: None,
        }
    }

    const ME: Id = 1;
    const GUILD: Id = 1000;

    /// A guild where @everyone may view channels and `ME` holds no role.
    fn guild(channels: Vec<Channel>) -> Guild {
        Guild {
            id: GUILD,
            name: "g".into(),
            channels,
            owner_id: 2,
            roles: vec![Role {
                id: GUILD,
                name: "@everyone".into(),
                position: 0,
                permissions: Permissions::VIEW_CHANNEL,
            }],
            my_roles: vec![],
            joined_at: None,
            default_notify: Notify::All,
        }
    }

    fn overwrite(id: Id, kind: OverwriteKind, allow: bool) -> Overwrite {
        let (allow, deny) = if allow {
            (Permissions::VIEW_CHANNEL, Permissions::default())
        } else {
            (Permissions::default(), Permissions::VIEW_CHANNEL)
        };
        Overwrite {
            id,
            kind,
            allow,
            deny,
        }
    }

    /// A text channel with these overwrites on viewing it.
    fn restricted(id: Id, overwrites: Vec<Overwrite>) -> Channel {
        Channel {
            overwrites,
            ..channel(id, ChannelKind::Text, None, 0)
        }
    }

    fn ids(entries: &[Entry<'_>]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                Entry::Category(c) => format!("[{}]", c.id),
                Entry::Channel(c) => c.id.to_string(),
            })
            .collect()
    }

    #[test]
    fn sidebar_puts_loose_channels_first_then_categories_in_order() {
        let guild = guild(vec![
            channel(20, ChannelKind::Category, None, 1),
            channel(10, ChannelKind::Category, None, 0),
            channel(21, ChannelKind::Text, Some(20), 0),
            channel(12, ChannelKind::Voice, Some(10), 1),
            channel(11, ChannelKind::Text, Some(10), 0),
            channel(2, ChannelKind::Text, None, 5),
        ]);
        assert_eq!(
            ids(&guild.sidebar(ME)),
            ["2", "[10]", "11", "12", "[20]", "21"]
        );
    }

    #[test]
    fn sidebar_breaks_position_ties_by_id() {
        let guild = guild(vec![
            channel(9, ChannelKind::Text, None, 0),
            channel(3, ChannelKind::Text, None, 0),
        ]);
        assert_eq!(ids(&guild.sidebar(ME)), ["3", "9"]);
    }

    #[test]
    fn sidebar_puts_text_before_voice_whatever_the_positions() {
        let guild = guild(vec![
            channel(10, ChannelKind::Category, None, 0),
            channel(11, ChannelKind::Voice, Some(10), 0),
            channel(12, ChannelKind::Text, Some(10), 1),
            channel(13, ChannelKind::Announcement, Some(10), 0),
        ]);
        assert_eq!(ids(&guild.sidebar(ME)), ["[10]", "13", "12", "11"]);
    }

    #[test]
    fn sidebar_keeps_channels_whose_category_is_unknown() {
        let guild = guild(vec![
            channel(2, ChannelKind::Text, None, 1),
            channel(3, ChannelKind::Text, Some(99), 0),
        ]);
        assert_eq!(ids(&guild.sidebar(ME)), ["3", "2"]);
        assert_eq!(guild.first_text_channel(ME), Some(3));
    }

    #[test]
    fn first_text_channel_skips_voice() {
        let guild = guild(vec![
            channel(5, ChannelKind::Voice, None, 0),
            channel(6, ChannelKind::Text, None, 1),
        ]);
        assert_eq!(guild.first_text_channel(ME), Some(6));
    }

    #[test]
    fn everyone_deny_hides_and_a_role_allow_shows_again() {
        let mut guild = guild(vec![
            restricted(2, vec![overwrite(GUILD, OverwriteKind::Role, false)]),
            restricted(
                3,
                vec![
                    overwrite(GUILD, OverwriteKind::Role, false),
                    overwrite(50, OverwriteKind::Role, true),
                ],
            ),
        ]);
        guild.my_roles = vec![50];
        assert_eq!(ids(&guild.sidebar(ME)), ["3"]);
    }

    #[test]
    fn member_overwrites_win_over_roles() {
        let mut guild = guild(vec![
            restricted(
                2,
                vec![
                    overwrite(50, OverwriteKind::Role, true),
                    overwrite(ME, OverwriteKind::Member, false),
                ],
            ),
            restricted(
                3,
                vec![
                    overwrite(GUILD, OverwriteKind::Role, false),
                    overwrite(ME, OverwriteKind::Member, true),
                ],
            ),
        ]);
        guild.my_roles = vec![50];
        assert_eq!(ids(&guild.sidebar(ME)), ["3"]);
    }

    #[test]
    fn my_role_overwrites_combine_with_allow_winning() {
        // Applied one by one, B's deny would come last and hide it.
        let mut guild = guild(vec![restricted(
            2,
            vec![
                overwrite(50, OverwriteKind::Role, true),
                overwrite(51, OverwriteKind::Role, false),
            ],
        )]);
        guild.my_roles = vec![50, 51];
        assert_eq!(ids(&guild.sidebar(ME)), ["2"]);
    }

    #[test]
    fn the_everyone_overwrite_applies_once_even_listed_among_my_roles() {
        // Folded in with my roles, @everyone's allow would beat role 50's deny.
        let mut guild = guild(vec![restricted(
            2,
            vec![
                overwrite(GUILD, OverwriteKind::Role, true),
                overwrite(50, OverwriteKind::Role, false),
            ],
        )]);
        guild.my_roles = vec![GUILD, 50];
        assert!(guild.sidebar(ME).is_empty());
    }

    #[test]
    fn owner_and_administrators_see_everything() {
        let hidden = || {
            guild(vec![restricted(
                2,
                vec![
                    overwrite(GUILD, OverwriteKind::Role, false),
                    overwrite(ME, OverwriteKind::Member, false),
                ],
            )])
        };
        assert!(hidden().sidebar(ME).is_empty());

        let mut owned = hidden();
        owned.owner_id = ME;
        assert_eq!(ids(&owned.sidebar(ME)), ["2"]);

        let mut administered = hidden();
        administered.roles.push(Role {
            id: 50,
            name: "admin".into(),
            position: 1,
            permissions: Permissions::ADMINISTRATOR,
        });
        administered.my_roles = vec![50];
        assert_eq!(ids(&administered.sidebar(ME)), ["2"]);
    }

    #[test]
    fn a_category_without_a_visible_channel_is_hidden() {
        let deny = vec![overwrite(GUILD, OverwriteKind::Role, false)];
        let guild = guild(vec![
            channel(10, ChannelKind::Category, None, 0),
            Channel {
                parent: Some(10),
                ..restricted(11, deny.clone())
            },
            // Nothing under it yet: it shows, as long as it is itself visible.
            channel(30, ChannelKind::Category, None, 2),
            Channel {
                kind: ChannelKind::Category,
                ..restricted(40, deny.clone())
            },
            // Hidden itself, but a channel under it is shown to me.
            Channel {
                kind: ChannelKind::Category,
                position: 3,
                ..restricted(50, deny.clone())
            },
            Channel {
                parent: Some(50),
                ..restricted(51, vec![overwrite(ME, OverwriteKind::Member, true)])
            },
        ]);
        assert_eq!(ids(&guild.sidebar(ME)), ["[30]", "[50]", "51"]);
    }

    #[test]
    fn snowflake_time_round_trips() {
        let at: jiff::Timestamp = "2026-10-07T12:34:56.789Z".parse().unwrap();
        assert_eq!(created_at(id_at(at, 7)), at);
        // A real snowflake from Discord's documentation.
        assert_eq!(
            created_at(175_928_847_299_117_063).to_string(),
            "2016-04-30T11:18:25.796Z"
        );
    }

    fn message(id: Id, author: Id) -> Message {
        Message {
            id,
            author: User {
                id: author,
                ..User::default()
            },
            content: String::new(),
            attachments: vec![],
            embeds: vec![],
        }
    }

    #[test]
    fn groups_split_on_author_and_after_seven_minutes() {
        let t0: jiff::Timestamp = "2026-10-07T12:00:00Z".parse().unwrap();
        let at = |mins: i64| id_at(t0 + jiff::SignedDuration::from_mins(mins), 0);
        let first = message(at(0), 1);
        assert!(starts_group(None, &first));
        assert!(!starts_group(Some(&first), &message(at(7), 1)));
        assert!(starts_group(Some(&first), &message(at(8), 1)));
        assert!(starts_group(Some(&first), &message(at(1), 2)));
    }

    #[test]
    fn applies_guild_and_channel_changes() {
        use crate::events::Update;
        let mut model = Model {
            guilds: vec![guild(vec![channel(10, ChannelKind::Text, None, 0)])],
            ..Model::default()
        };
        model.apply(Update::ChannelUpsert {
            guild: GUILD,
            channel: Channel {
                name: "renamed".into(),
                ..channel(10, ChannelKind::Text, None, 0)
            },
        });
        model.apply(Update::ChannelUpsert {
            guild: GUILD,
            channel: channel(11, ChannelKind::Voice, None, 1),
        });
        let g = model.guild(GUILD).unwrap();
        assert_eq!(g.channels.len(), 2);
        assert_eq!(g.channel(10).unwrap().name, "renamed");
        model.apply(Update::ChannelRemove {
            guild: GUILD,
            channel: 10,
        });
        assert!(model.guild(GUILD).unwrap().channel(10).is_none());
        model.apply(Update::GuildChanged {
            id: GUILD,
            name: Some("h".into()),
            owner_id: Some(ME),
        });
        assert_eq!(model.guild(GUILD).unwrap().name, "h");
        assert_eq!(model.guild(GUILD).unwrap().owner_id, ME);
        model.apply(Update::GuildRemove(GUILD));
        assert!(model.guilds.is_empty());
    }

    #[test]
    fn role_changes_reach_what_i_can_see() {
        use crate::events::Update;
        let hidden = Channel {
            overwrites: vec![Overwrite {
                id: GUILD,
                kind: OverwriteKind::Role,
                allow: Permissions::default(),
                deny: Permissions::VIEW_CHANNEL,
            }],
            ..channel(10, ChannelKind::Text, None, 0)
        };
        let mut model = Model {
            me: ME,
            guilds: vec![guild(vec![hidden])],
            ..Model::default()
        };
        let visible = |model: &Model| {
            let g = model.guild(GUILD).unwrap();
            g.can_view(g.channel(10).unwrap(), ME)
        };
        assert!(!visible(&model));
        model.apply(Update::RoleUpsert {
            guild: GUILD,
            role: Role {
                id: 50,
                name: "admin".into(),
                position: 1,
                permissions: Permissions::ADMINISTRATOR,
            },
        });
        assert!(!visible(&model), "a role I do not have changes nothing");
        model.apply(Update::MyRoles {
            guild: GUILD,
            roles: vec![50],
        });
        assert!(visible(&model));
        model.apply(Update::RoleRemove {
            guild: GUILD,
            role: 50,
        });
        assert!(!visible(&model));
        assert!(model.guild(GUILD).unwrap().my_roles.is_empty());
    }

    fn said(id: Id, author: Id, content: &str) -> Message {
        Message {
            content: content.into(),
            ..message(id, author)
        }
    }

    #[test]
    fn history_pages_merge_in_order() {
        use crate::events::Update;
        let mut model = Model {
            dms: vec![DmChannel {
                id: 7,
                recipients: vec![],
                last_message_id: None,
            }],
            ..Model::default()
        };
        assert!(!model.messages.contains_key(&7));
        model.apply(Update::History {
            channel: 7,
            messages: vec![said(30, 1, "c"), said(40, 2, "d")],
            oldest: Some(30),
            complete: false,
        });
        // An older page, overlapping by one message.
        model.apply(Update::History {
            channel: 7,
            messages: vec![said(10, 1, "a"), said(30, 1, "c")],
            oldest: Some(10),
            complete: true,
        });
        let ids: Vec<Id> = model.messages(7).iter().map(|m| m.id).collect();
        assert_eq!(ids, [10, 30, 40]);
        assert!(model.complete.contains(&7));
        assert_eq!(model.cursors[&7], 10);
        model.apply(Update::DmRemove(7));
        assert!(!model.complete.contains(&7) && !model.cursors.contains_key(&7));
        // A page landing after its channel went away is dropped.
        model.apply(Update::History {
            channel: 7,
            messages: vec![said(1, 1, "late")],
            oldest: Some(1),
            complete: true,
        });
        assert!(!model.messages.contains_key(&7));
        assert!(model.users.contains_key(&2), "authors become known users");
    }

    #[test]
    fn live_messages_join_loaded_history_only() {
        use crate::events::Update;
        let mut model = Model {
            dms: vec![DmChannel {
                id: 8,
                recipients: vec![],
                last_message_id: Some(5),
            }],
            ..Model::default()
        };
        model.apply(Update::MessageCreate {
            channel: 7,
            guild: None,
            message: said(50, 1, "unseen"),
            ping: Ping::default(),
        });
        assert!(!model.messages.contains_key(&7), "not loaded, not started");
        model.apply(Update::History {
            channel: 8,
            messages: vec![said(5, 1, "old")],
            oldest: Some(5),
            complete: true,
        });
        model.apply(Update::MessageCreate {
            channel: 8,
            guild: None,
            message: said(60, 2, "new"),
            ping: Ping::default(),
        });
        model.apply(Update::MessageCreate {
            channel: 8,
            guild: None,
            message: said(60, 2, "new"),
            ping: Ping::default(),
        });
        assert_eq!(model.messages(8).len(), 2, "a repeated event adds nothing");
        assert_eq!(model.dm(8).unwrap().last_message_id, Some(60));

        model.apply(Update::MessageEdit {
            channel: 8,
            id: 60,
            content: Some("edited".into()),
            attachments: None,
            embeds: None,
        });
        // A link preview resolving later: the text stays.
        let preview = Embed {
            kind: Some("article".into()),
            title: Some("Rust".into()),
            description: None,
            url: None,
            color: None,
            author: None,
            footer: None,
            provider: None,
            fields: vec![],
            thumbnail: None,
            image: None,
        };
        model.apply(Update::MessageEdit {
            channel: 8,
            id: 60,
            content: None,
            attachments: None,
            embeds: Some(vec![preview.clone()]),
        });
        assert_eq!(model.messages(8)[1].content, "edited");
        assert_eq!(model.messages(8)[1].embeds, [preview]);
        model.apply(Update::MessageDelete {
            channel: 8,
            ids: vec![5],
        });
        assert_eq!(model.messages(8).len(), 1);
    }

    #[test]
    fn applies_dm_changes() {
        use crate::events::Update;
        let mut model = Model::default();
        let dm = DmChannel {
            id: 5,
            recipients: vec![],
            last_message_id: None,
        };
        model.apply(Update::DmUpsert(dm.clone()));
        model.apply(Update::DmUpsert(DmChannel {
            last_message_id: Some(9),
            ..dm
        }));
        assert_eq!(model.dms.len(), 1);
        assert_eq!(model.dm(5).unwrap().last_message_id, Some(9));
        model.apply(Update::DmRemove(5));
        assert!(model.dms.is_empty());
    }

    #[test]
    fn dms_sort_most_recent_first() {
        let dm = |id, last| DmChannel {
            id,
            recipients: vec![],
            last_message_id: last,
        };
        let model = Model {
            dms: vec![dm(1, Some(10)), dm(2, None), dm(3, Some(30))],
            ..Model::default()
        };
        let order: Vec<Id> = model.dms_by_recency().iter().map(|d| d.id).collect();
        assert_eq!(order, [3, 1, 2]);
    }

    fn at(text: &str) -> jiff::Timestamp {
        text.parse().unwrap()
    }

    const NOW: &str = "2026-10-08T00:00:00Z";

    /// A guild I joined at noon, with #10 (newest message `after + 20`),
    /// voice #11 and category #12 holding #13 (newest `after + 30`), where
    /// `after` is the first snowflake after I joined.
    fn unread_model() -> Model {
        let joined = at("2026-10-07T12:00:00Z");
        let after = id_at(joined, 0) + 1;
        let mut g = guild(vec![
            Channel {
                last_message_id: Some(after + 20),
                ..channel(10, ChannelKind::Text, None, 0)
            },
            Channel {
                last_message_id: Some(after + 25),
                ..channel(11, ChannelKind::Voice, None, 1)
            },
            channel(12, ChannelKind::Category, None, 2),
            Channel {
                last_message_id: Some(after + 30),
                ..channel(13, ChannelKind::Text, Some(12), 0)
            },
        ]);
        g.joined_at = Some(joined);
        Model {
            me: ME,
            guilds: vec![g],
            ..Model::default()
        }
    }

    fn newest(model: &Model, channel: Id) -> Id {
        let g = model.guild(GUILD).unwrap();
        g.channel(channel).unwrap().last_message_id.unwrap()
    }

    fn shown(unread: bool, mentions: u32, muted: bool) -> Badge {
        Badge {
            unread,
            mentions,
            muted,
        }
    }

    fn badge(model: &Model, channel: Id) -> Badge {
        let g = model.guild(GUILD).unwrap();
        model.channel_badge(g, g.channel(channel).unwrap(), at(NOW))
    }

    fn guild_badge(model: &Model) -> Badge {
        model.guild_badge(model.guild(GUILD).unwrap(), at(NOW))
    }

    fn read(model: &mut Model, channel: Id, last_read: Option<Id>, mentions: u32) {
        model.read_states.insert(
            channel,
            ReadState {
                last_read,
                mentions,
                flags: None,
            },
        );
    }

    fn settings(model: &mut Model, settings: GuildSettings) {
        model.guild_settings.insert(Some(GUILD), settings);
    }

    fn channel_settings(channels: &[(Id, ChannelSettings)]) -> GuildSettings {
        GuildSettings {
            channels: channels.iter().copied().collect(),
            ..GuildSettings::default()
        }
    }

    #[test]
    fn a_channel_is_unread_past_its_read_state_or_my_joining() {
        let mut model = unread_model();
        // Never read: everything since I joined is new.
        assert!(badge(&model, 10).unread);
        assert!(!badge(&model, 11).unread, "voice is never unread");
        let last = newest(&model, 10);
        read(&mut model, 10, Some(last), 0);
        assert!(!badge(&model, 10).unread);
        let last = newest(&model, 10);
        read(&mut model, 10, Some(last - 1), 2);
        assert_eq!(badge(&model, 10), shown(true, 2, false));
        // Without a join date, a channel never read stays quiet.
        model.guilds[0].joined_at = None;
        assert!(!badge(&model, 13).unread);
    }

    #[test]
    fn mutes_hide_unreads_but_not_mentions_until_they_end() {
        let mut model = unread_model();
        read(&mut model, 13, None, 1);
        let until = |end: &str| ChannelSettings {
            muted: Some(Mute {
                until: Some(at(end)),
            }),
            ..ChannelSettings::default()
        };
        // The category until tomorrow; #10 until an hour ago.
        settings(
            &mut model,
            channel_settings(&[
                (12, until("2026-10-09T00:00:00Z")),
                (10, until("2026-10-07T23:00:00Z")),
            ]),
        );
        assert_eq!(badge(&model, 13), shown(false, 1, true));
        assert!(badge(&model, 10).unread, "an ended mute is over");
        let g = model.guild(GUILD).unwrap();
        let later = at("2026-10-09T00:00:01Z");
        assert!(model.channel_badge(g, g.channel(13).unwrap(), later).unread);
    }

    #[test]
    fn a_guild_sums_what_i_can_view() {
        let mut model = unread_model();
        // #13 is unread with two mentions; #10 is unread with five, but
        // hidden.
        let last = newest(&model, 11);
        read(&mut model, 11, Some(last), 0);
        read(&mut model, 10, None, 5);
        read(&mut model, 13, None, 2);
        model.guilds[0].channels[0]
            .overwrites
            .push(overwrite(GUILD, OverwriteKind::Role, false));
        assert_eq!(guild_badge(&model), shown(true, 2, false));
        // Muted, the guild is unread only through channels with mentions.
        settings(
            &mut model,
            GuildSettings {
                muted: Some(Mute { until: None }),
                ..GuildSettings::default()
            },
        );
        assert_eq!(guild_badge(&model), shown(true, 2, true));
        read(&mut model, 13, None, 0);
        assert_eq!(guild_badge(&model), shown(false, 0, true));
        // Its own list still marks the channel.
        assert!(badge(&model, 13).unread);
    }

    #[test]
    fn under_new_notifications_only_mentions_guilds_stay_quiet() {
        let mut model = unread_model();
        model.guilds[0].default_notify = Notify::Mentions;
        assert!(badge(&model, 10).unread, "the old settings: every message");
        model.separate_unreads = true;
        assert!(!badge(&model, 10).unread);
        assert!(!guild_badge(&model).unread);
        // Mentions still mark the guild.
        read(&mut model, 13, None, 1);
        assert_eq!(guild_badge(&model), shown(true, 1, false));
        // An explicit setting on the category wins over notifications.
        let all = ChannelSettings {
            unreads: Some(Unreads::All),
            ..ChannelSettings::default()
        };
        settings(&mut model, channel_settings(&[(12, all)]));
        assert!(badge(&model, 13).unread && !badge(&model, 10).unread);
        // So does a channel notifying on every message.
        let notify_all = ChannelSettings {
            notify: Some(Notify::All),
            ..ChannelSettings::default()
        };
        settings(&mut model, channel_settings(&[(10, notify_all)]));
        assert!(badge(&model, 10).unread);
    }

    #[test]
    fn dms_count_their_unread_messages() {
        let now = at(NOW);
        let dm = |id, last| DmChannel {
            id,
            recipients: vec![],
            last_message_id: Some(last),
        };
        let mut model = Model {
            dms: vec![dm(5, 50), dm(6, 60), dm(7, 70)],
            ..Model::default()
        };
        read(&mut model, 5, Some(50), 0);
        read(&mut model, 6, Some(55), 2);
        // Never read: unread since the DM began.
        read(&mut model, 7, None, 1);
        let badges: Vec<(bool, u32)> = model
            .dms
            .iter()
            .map(|d| model.dm_badge(d, now))
            .map(|b| (b.unread, b.mentions))
            .collect();
        assert_eq!(badges, [(false, 0), (true, 2), (true, 1)]);
        // Muted, #6 loses its unread mark; its count still adds up.
        let muted = ChannelSettings {
            muted: Some(Mute { until: None }),
            ..ChannelSettings::default()
        };
        model.guild_settings.insert(
            None,
            GuildSettings {
                channels: HashMap::from([(6, muted)]),
                ..GuildSettings::default()
            },
        );
        assert_eq!(model.dm_badge(&model.dms[1], now), shown(false, 2, true));
        assert_eq!(model.dm_mentions(), 3);
    }

    fn create(model: &mut Model, channel: Id, guild: Option<Id>, message: Message, ping: Ping) {
        model.apply(crate::events::Update::MessageCreate {
            channel,
            guild,
            message,
            ping,
        });
    }

    #[test]
    fn live_messages_count_the_mentions_that_reach_me() {
        let mut model = unread_model();
        model.guilds[0].my_roles = vec![50];
        let base = newest(&model, 10);
        read(&mut model, 10, Some(base), 0);
        let ping = |me, everyone, roles: &[Id]| Ping {
            me,
            everyone,
            roles: roles.to_vec(),
            ..Ping::default()
        };
        let mut next = base;
        let mut send = |model: &mut Model, author, ping| {
            next += 1;
            create(model, 10, Some(GUILD), message(next, author), ping);
            next
        };
        send(&mut model, 2, Ping::default());
        assert_eq!(badge(&model, 10), shown(true, 0, false));
        assert_eq!(newest(&model, 10), base + 1, "the newest message moves");
        send(&mut model, 2, ping(true, false, &[]));
        send(&mut model, 2, ping(false, true, &[]));
        send(&mut model, 2, ping(false, false, &[50]));
        send(&mut model, 2, ping(false, false, &[51]));
        assert_eq!(badge(&model, 10).mentions, 3);
        settings(
            &mut model,
            GuildSettings {
                suppress_everyone: true,
                suppress_roles: true,
                ..GuildSettings::default()
            },
        );
        send(&mut model, 2, ping(false, true, &[50]));
        assert_eq!(badge(&model, 10).mentions, 3, "suppressed");
        // A repeated event counts once.
        let last = newest(&model, 10);
        create(
            &mut model,
            10,
            Some(GUILD),
            message(last, 2),
            ping(true, false, &[]),
        );
        assert_eq!(badge(&model, 10).mentions, 3);
        // My own message, from another device, reads the channel.
        let mine = send(&mut model, ME, Ping::default());
        assert_eq!(badge(&model, 10), shown(false, 0, false));
        assert_eq!(model.read_states[&10].last_read, Some(mine));
    }

    #[test]
    fn every_dm_message_counts_unless_muted() {
        let mut model = Model {
            me: ME,
            dms: vec![DmChannel {
                id: 5,
                recipients: vec![],
                last_message_id: Some(50),
            }],
            ..Model::default()
        };
        read(&mut model, 5, Some(50), 0);
        create(&mut model, 5, None, message(51, 2), Ping::default());
        assert_eq!(model.dm_mentions(), 1);
        model.guild_settings.insert(
            None,
            GuildSettings {
                muted: Some(Mute { until: None }),
                ..GuildSettings::default()
            },
        );
        create(&mut model, 5, None, message(52, 2), Ping::default());
        assert_eq!(model.dm_mentions(), 1, "muted");
        let me = Ping {
            me: true,
            ..Ping::default()
        };
        create(&mut model, 5, None, message(53, 2), me);
        assert_eq!(model.dm_mentions(), 2, "a mention gets through");
    }

    #[test]
    fn acks_from_discord_move_forward_unless_manual() {
        use crate::events::Update;
        let mut model = Model::default();
        read(&mut model, 10, Some(100), 0);
        let ack = |message, manual, mentions| Update::Acked {
            channel: 10,
            message,
            manual,
            mentions,
            flags: None,
        };
        model.apply(ack(90, false, None));
        assert_eq!(model.read_states[&10].last_read, Some(100), "a late echo");
        model.apply(ack(120, false, None));
        assert_eq!(model.read_states[&10].last_read, Some(120));
        // The echo of the current read leaves a mention since alone.
        model.read_states.get_mut(&10).unwrap().mentions = 1;
        model.apply(ack(120, false, Some(0)));
        assert_eq!(model.read_states[&10].mentions, 1);
        // Marked unread on another device.
        model.apply(ack(80, true, Some(4)));
        assert_eq!(
            model.read_states[&10],
            ReadState {
                last_read: Some(80),
                mentions: 4,
                flags: None
            }
        );
    }

    #[test]
    fn newest_messages_never_move_back() {
        use crate::events::Update;
        let mut model = unread_model();
        let last = newest(&model, 10);
        model.apply(Update::LastMessages {
            guild: GUILD,
            channels: vec![(10, Some(last - 5)), (13, None)],
        });
        assert_eq!(newest(&model, 10), last);
        assert!(
            model
                .guild(GUILD)
                .unwrap()
                .channel(13)
                .unwrap()
                .last_message_id
                .is_some()
        );
        model.apply(Update::LastMessages {
            guild: GUILD,
            channels: vec![(10, Some(last + 1))],
        });
        assert_eq!(newest(&model, 10), last + 1);
    }

    #[test]
    fn badge_counts_shorten_past_a_thousand() {
        assert_eq!(badge_count(999), "999");
        assert_eq!(badge_count(1000), "1k+");
        assert_eq!(badge_count(25_000), "9k+");
    }

    #[test]
    fn reading_acks_once_per_new_message() {
        let mut model = unread_model();
        let last = newest(&model, 10);
        read(&mut model, 10, None, 3);
        assert_eq!(
            model.mark_read(10),
            Some(Ack {
                guild: Some(GUILD),
                channel: 10,
                message: last,
                flags: Some(IS_GUILD_CHANNEL),
                immediate: true,
            })
        );
        // The flags wait for Discord to save them.
        assert_eq!(
            model.read_states[&10],
            ReadState {
                last_read: Some(last),
                mentions: 0,
                flags: None,
            }
        );
        assert_eq!(model.mark_read(10), None, "nothing new");
        model.save_flags(10, IS_GUILD_CHANNEL);
        model.apply(crate::events::Update::LastMessages {
            guild: GUILD,
            channels: vec![(10, Some(last + 1))],
        });
        let ack = model.mark_read(10).unwrap();
        assert_eq!(
            (ack.message, ack.flags, ack.immediate),
            (last + 1, None, false)
        );
    }

    #[test]
    fn reading_never_moves_back() {
        let mut model = unread_model();
        let last = newest(&model, 10);
        // Read further than the newest message known here, with a mention
        // left to clear.
        read(&mut model, 10, Some(last + 9), 1);
        let ack = model.mark_read(10).unwrap();
        assert_eq!(ack.message, last + 9);
        assert_eq!(model.read_states[&10].last_read, Some(last + 9));
    }
}
