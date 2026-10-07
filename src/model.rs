//! What the client knows about the account, kept in memory only.
//!
//! The shapes follow Discord's own objects closely enough that the gateway
//! can fill them later, and no further than the interface needs.

use std::collections::{HashMap, HashSet};

/// A Discord snowflake: guilds, channels, users and messages all use one.
pub type Id = u64;

/// Milliseconds between the Unix epoch and Discord's (2015-01-01).
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

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

#[derive(Debug, Default, PartialEq)]
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
            Update::MessageCreate { channel, message } => {
                self.users.insert(message.author.id, message.author.clone());
                if let Some(dm) = self.dms.iter_mut().find(|d| d.id == channel) {
                    dm.last_message_id = dm.last_message_id.max(Some(message.id));
                }
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
            message: said(50, 1, "unseen"),
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
            message: said(60, 2, "new"),
        });
        model.apply(Update::MessageCreate {
            channel: 8,
            message: said(60, 2, "new"),
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
}
