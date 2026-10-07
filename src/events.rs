//! The gateway's events, read into changes to the [`Model`].
//!
//! Shapes follow what Discord sends the web client with the capabilities
//! `gateway` asks for: READY lists every user once in `users` and DMs name
//! their recipients by id, guild metadata sits under `properties`, and DMs
//! may arrive late in READY_SUPPLEMENTAL. Only what fastcord shows is read;
//! serde skips the rest.

use crate::api::{ApiUser, optional_snowflake, snowflake};
use crate::model::{
    Attachment, Channel, ChannelKind, ChannelSettings, DmChannel, Embed, EmbedField, EmbedImage,
    Guild, GuildSettings, Id, Message, Model, Mute, Notify, Overwrite, OverwriteKind, Permissions,
    Ping, ReadState, Role, Unreads, User,
};
use serde_json::value::RawValue;
use std::collections::HashMap;

/// A change to the model, in the order the gateway reported it.
#[derive(Clone, Debug, PartialEq)]
pub enum Update {
    /// A guild joined or became available.
    GuildUpsert(Guild),
    /// A guild's name or owner changed (each `None` when unchanged).
    GuildChanged {
        id: Id,
        name: Option<String>,
        owner_id: Option<Id>,
    },
    /// A guild left, or deleted.
    GuildRemove(Id),
    ChannelUpsert {
        guild: Id,
        channel: Channel,
    },
    RoleUpsert {
        guild: Id,
        role: Role,
    },
    RoleRemove {
        guild: Id,
        role: Id,
    },
    /// The signed-in member's roles in a guild changed.
    MyRoles {
        guild: Id,
        roles: Vec<Id>,
    },
    ChannelRemove {
        guild: Id,
        channel: Id,
    },
    DmUpsert(DmChannel),
    DmRemove(Id),
    /// A page of a channel's history, oldest first, older than anything
    /// loaded (or the latest page when nothing was). `complete` once the
    /// channel's first message is in.
    History {
        channel: Id,
        messages: Vec<Message>,
        /// The oldest message the page held, shown or not: where the next
        /// page starts.
        oldest: Option<Id>,
        complete: bool,
    },
    MessageCreate {
        channel: Id,
        /// `None` in a DM.
        guild: Option<Id>,
        message: Message,
        ping: Ping,
    },
    /// An edit. Each part is `None` when the edit left it alone: a link
    /// preview resolving after the message sends `embeds` only.
    MessageEdit {
        channel: Id,
        id: Id,
        content: Option<String>,
        attachments: Option<Vec<Attachment>>,
        embeds: Option<Vec<Embed>>,
    },
    MessageDelete {
        channel: Id,
        ids: Vec<Id>,
    },
    /// A channel read up to `message`, here or on another device.
    Acked {
        channel: Id,
        message: Id,
        /// Marked unread, which may move the position back.
        manual: bool,
        mentions: Option<u32>,
        flags: Option<u32>,
    },
    /// My notification settings for a guild (`None`: DMs), in full.
    GuildSettings {
        guild: Option<Id>,
        settings: GuildSettings,
    },
    /// The newest message of some channels, which Discord reports for
    /// guilds whose messages it does not stream.
    LastMessages {
        guild: Id,
        channels: Vec<(Id, Option<Id>)>,
    },
    /// My status changed: Do Not Disturb (until when), or anything else.
    DoNotDisturb(Option<Mute>),
}

/// How many messages a history page asks for, as the official client does.
pub const PAGE: usize = 50;

#[derive(serde::Deserialize)]
struct WireMessage {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    #[serde(deserialize_with = "snowflake")]
    channel_id: Id,
    author: ApiUser,
    #[serde(default)]
    content: String,
    #[serde(rename = "type", default)]
    kind: u8,
    #[serde(default, deserialize_with = "lenient")]
    attachments: Vec<WireAttachment>,
    #[serde(default, deserialize_with = "lenient")]
    embeds: Vec<WireEmbed>,
    #[serde(default, deserialize_with = "optional_snowflake")]
    guild_id: Option<Id>,
    #[serde(default, deserialize_with = "lenient")]
    mentions: Vec<WireMemberUser>,
    /// Set only when the author was allowed to ping everyone.
    #[serde(default)]
    mention_everyone: bool,
    #[serde(default)]
    mention_roles: Vec<String>,
    #[serde(default)]
    flags: u64,
}

/// The message flag of @silent messages.
const SUPPRESS_NOTIFICATIONS: u64 = 1 << 12;

#[derive(serde::Deserialize)]
struct WireAttachment {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    filename: String,
    #[serde(default)]
    size: u64,
    url: String,
    #[serde(default)]
    proxy_url: String,
    content_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    #[serde(default)]
    flags: u64,
}

impl From<WireAttachment> for Attachment {
    fn from(wire: WireAttachment) -> Self {
        Attachment {
            id: wire.id,
            filename: wire.filename,
            url: wire.url,
            proxy_url: wire.proxy_url,
            content_type: wire.content_type,
            size: wire.size,
            width: wire.width,
            height: wire.height,
            flags: wire.flags,
        }
    }
}

/// An embed. Each part is read on its own, so one Discord sends in a shape
/// this version does not know is left out rather than the whole embed.
#[derive(serde::Deserialize)]
struct WireEmbed {
    #[serde(rename = "type")]
    kind: Option<String>,
    title: Option<String>,
    description: Option<String>,
    url: Option<String>,
    color: Option<u32>,
    #[serde(default, deserialize_with = "lenient_one")]
    author: Option<Named>,
    #[serde(default, deserialize_with = "lenient_one")]
    footer: Option<Footer>,
    #[serde(default, deserialize_with = "lenient_one")]
    provider: Option<Named>,
    #[serde(default, deserialize_with = "lenient")]
    fields: Vec<WireField>,
    #[serde(default, deserialize_with = "lenient_one")]
    thumbnail: Option<WireImage>,
    #[serde(default, deserialize_with = "lenient_one")]
    image: Option<WireImage>,
}

#[derive(serde::Deserialize)]
struct Named {
    name: String,
}

#[derive(serde::Deserialize)]
struct Footer {
    text: String,
}

#[derive(serde::Deserialize)]
struct WireField {
    name: String,
    value: String,
    #[serde(default)]
    inline: bool,
}

#[derive(serde::Deserialize)]
struct WireImage {
    url: String,
    proxy_url: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
}

impl From<WireImage> for EmbedImage {
    fn from(wire: WireImage) -> Self {
        EmbedImage {
            url: wire.url,
            proxy_url: wire.proxy_url,
            width: wire.width,
            height: wire.height,
        }
    }
}

impl From<WireEmbed> for Embed {
    fn from(wire: WireEmbed) -> Self {
        Embed {
            kind: wire.kind,
            title: wire.title,
            description: wire.description,
            url: wire.url,
            color: wire.color,
            author: wire.author.map(|a| a.name),
            footer: wire.footer.map(|f| f.text),
            provider: wire.provider.map(|p| p.name),
            fields: wire
                .fields
                .into_iter()
                .map(|f| EmbedField {
                    name: f.name,
                    value: f.value,
                    inline: f.inline,
                })
                .collect(),
            thumbnail: wire.thumbnail.map(Into::into),
            image: wire.image.map(Into::into),
        }
    }
}

impl WireMessage {
    fn ping(&self, me: Id) -> Ping {
        Ping {
            me: self.mentions.iter().any(|user| user.id == me),
            everyone: self.mention_everyone,
            roles: self
                .mention_roles
                .iter()
                .filter_map(|r| r.parse().ok())
                .collect(),
            silent: self.flags & SUPPRESS_NOTIFICATIONS != 0,
        }
    }
}

/// The message types shown as conversation: regular messages, replies and
/// command invocations. Joins, pins, boosts and the other system messages
/// come later.
fn shown(kind: u8) -> bool {
    matches!(kind, 0 | 19 | 20 | 23)
}

impl From<WireMessage> for Message {
    fn from(wire: WireMessage) -> Self {
        Message {
            id: wire.id,
            author: wire.author.into(),
            content: wire.content,
            attachments: wire.attachments.into_iter().map(Into::into).collect(),
            embeds: wire.embeds.into_iter().map(Into::into).collect(),
        }
    }
}

/// A page of history as the API returns it (newest first), oldest first.
///
/// Completeness and where the next page starts count every entry Discord
/// sent: system messages and entries this version cannot read are left
/// out of `messages` but still move the cursor, or a run of joins would
/// ask for the same page forever.
pub fn history(channel: Id, body: &str) -> serde_json::Result<Update> {
    #[derive(serde::Deserialize)]
    struct Entry {
        #[serde(deserialize_with = "snowflake")]
        id: Id,
    }
    let raw: Vec<Box<RawValue>> = serde_json::from_str(body)?;
    let complete = raw.len() < PAGE;
    let oldest = raw
        .iter()
        .filter_map(|entry| serde_json::from_str::<Entry>(entry.get()).ok())
        .map(|entry| entry.id)
        .min();
    let mut messages: Vec<Message> = raw
        .iter()
        .filter_map(|entry| serde_json::from_str::<WireMessage>(entry.get()).ok())
        .filter(|wire| shown(wire.kind))
        .map(Message::from)
        .collect();
    messages.sort_by_key(|m| m.id);
    Ok(Update::History {
        channel,
        messages,
        oldest,
        complete,
    })
}

#[derive(serde::Deserialize)]
struct MessageChange {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    #[serde(deserialize_with = "snowflake")]
    channel_id: Id,
    content: Option<String>,
    #[serde(default, deserialize_with = "lenient_some")]
    attachments: Option<Vec<WireAttachment>>,
    #[serde(default, deserialize_with = "lenient_some")]
    embeds: Option<Vec<WireEmbed>>,
}

#[derive(serde::Deserialize)]
struct MessageDelete {
    #[serde(default, deserialize_with = "optional_snowflake")]
    id: Option<Id>,
    #[serde(default)]
    ids: Vec<String>,
    #[serde(deserialize_with = "snowflake")]
    channel_id: Id,
}

#[derive(serde::Deserialize)]
struct Ready {
    user: ApiUser,
    #[serde(default, deserialize_with = "lenient")]
    users: Vec<ApiUser>,
    /// Read one by one in `ready`, keeping their order for `merged_members`.
    #[serde(default)]
    guilds: Vec<Box<RawValue>>,
    /// Per guild, in `guilds`' order: the members READY describes, the
    /// signed-in one among them.
    #[serde(default)]
    merged_members: Vec<Members>,
    #[serde(default, deserialize_with = "lenient")]
    private_channels: Vec<WireChannel>,
    /// A refreshed token, when Discord rotates it.
    auth_token: Option<String>,
    /// The next three are read on their own: a shape this version does
    /// not know costs badges, never the session.
    read_state: Option<Box<RawValue>>,
    user_guild_settings: Option<Box<RawValue>>,
    notification_settings: Option<Box<RawValue>>,
    /// The account's settings, as base64 protobuf: only my status is read.
    user_settings_proto: Option<String>,
}

/// The account's notification settings: only the flag that separates
/// unreads from notifications matters here.
#[derive(serde::Deserialize)]
struct NotificationSettings {
    #[serde(default)]
    flags: u64,
}

const USE_NEW_NOTIFICATIONS: u64 = 1 << 4;

/// A list Discord versions so clients can cache it, as the capabilities ask.
#[derive(serde::Deserialize)]
#[serde(bound = "T: serde::de::DeserializeOwned")]
struct Versioned<T> {
    #[serde(default, deserialize_with = "lenient")]
    entries: Vec<T>,
}

#[derive(serde::Deserialize)]
struct WireReadState {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    /// Absent for channels. Guild events, the notification centre and
    /// other features have read states too; fastcord shows none of them.
    #[serde(default)]
    read_state_type: u8,
    #[serde(default, deserialize_with = "loose_snowflake")]
    last_message_id: Option<Id>,
    #[serde(default)]
    mention_count: i64,
    flags: Option<u32>,
}

/// A read state too odd to read but still naming its channel.
#[derive(serde::Deserialize)]
struct ReadStateId {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    #[serde(default)]
    read_state_type: u8,
}

#[derive(serde::Deserialize)]
struct WireGuildSettings {
    /// Null for the DMs' settings.
    #[serde(default, deserialize_with = "loose_snowflake")]
    guild_id: Option<Id>,
    #[serde(default)]
    muted: bool,
    mute_config: Option<MuteConfig>,
    message_notifications: Option<u8>,
    #[serde(default)]
    flags: u32,
    #[serde(default)]
    suppress_everyone: bool,
    #[serde(default)]
    suppress_roles: bool,
    #[serde(default, deserialize_with = "lenient")]
    channel_overrides: Vec<WireChannelOverride>,
}

#[derive(serde::Deserialize)]
struct WireChannelOverride {
    #[serde(deserialize_with = "snowflake")]
    channel_id: Id,
    #[serde(default)]
    muted: bool,
    mute_config: Option<MuteConfig>,
    message_notifications: Option<u8>,
    #[serde(default)]
    flags: u32,
}

/// Discord's notification levels; 3 (and anything else) inherits.
fn notify(level: Option<u8>) -> Option<Notify> {
    match level? {
        0 => Some(Notify::All),
        1 => Some(Notify::Mentions),
        2 => Some(Notify::Nothing),
        _ => None,
    }
}

/// The unread setting in a settings `flags` field, whose bits differ
/// between guilds and channels.
fn unreads(flags: u32, all: u32, mentions: u32) -> Option<Unreads> {
    if flags & all != 0 {
        Some(Unreads::All)
    } else if flags & mentions != 0 {
        Some(Unreads::Mentions)
    } else {
        None
    }
}

#[derive(serde::Deserialize)]
struct MuteConfig {
    /// When a temporary mute ends; null for good.
    end_time: Option<String>,
}

/// A mute, when `muted`. Discord keeps a temporary mute's `muted` set once
/// it has ended: only its end time says it is over.
fn mute(muted: bool, config: Option<MuteConfig>) -> Option<Mute> {
    muted.then(|| Mute {
        until: config
            .and_then(|c| c.end_time)
            .and_then(|end| end.parse().ok()),
    })
}

impl WireGuildSettings {
    fn settings(self) -> (Option<Id>, GuildSettings) {
        let channels = self
            .channel_overrides
            .into_iter()
            .map(|o| {
                let settings = ChannelSettings {
                    muted: mute(o.muted, o.mute_config),
                    notify: notify(o.message_notifications),
                    unreads: unreads(o.flags, 1 << 10, 1 << 9),
                };
                (o.channel_id, settings)
            })
            .collect();
        let settings = GuildSettings {
            muted: mute(self.muted, self.mute_config),
            notify: notify(self.message_notifications),
            unreads: unreads(self.flags, 1 << 11, 1 << 12),
            suppress_everyone: self.suppress_everyone,
            suppress_roles: self.suppress_roles,
            channels,
        };
        (self.guild_id, settings)
    }
}

#[derive(serde::Deserialize)]
struct MessageAck {
    #[serde(deserialize_with = "snowflake")]
    channel_id: Id,
    #[serde(deserialize_with = "snowflake")]
    message_id: Id,
    /// Absent for channels, as in read states.
    #[serde(default)]
    ack_type: u8,
    #[serde(default)]
    manual: bool,
    mention_count: Option<i64>,
    flags: Option<u32>,
}

/// PASSIVE_UPDATE_V1/V2 and CHANNEL_UNREAD_UPDATE: one shape under three
/// names.
#[derive(serde::Deserialize)]
struct ChannelUnreads {
    #[serde(deserialize_with = "snowflake")]
    guild_id: Id,
    #[serde(
        default,
        alias = "channels",
        alias = "channel_unread_updates",
        deserialize_with = "lenient"
    )]
    updated_channels: Vec<ChannelUnread>,
}

#[derive(serde::Deserialize)]
struct ChannelUnread {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    #[serde(default, deserialize_with = "optional_snowflake")]
    last_message_id: Option<Id>,
}

/// A versioned list's entries, or `None` (logged) when the list itself is
/// in a shape this version does not know.
fn versioned<T: serde::de::DeserializeOwned>(raw: Option<&RawValue>) -> Option<Vec<T>> {
    match serde_json::from_str::<Versioned<T>>(raw?.get()) {
        Ok(list) => Some(list.entries),
        Err(error) => {
            log::warn!(
                "unreadable {}: {}",
                std::any::type_name::<T>(),
                crate::backend::describe(&error)
            );
            None
        }
    }
}

/// READY's channel read states. A channel whose read state is missing
/// because its entry, or the whole list, could not be read counts as read
/// up to its newest message: a lost badge beats marking every channel
/// unread since I joined.
fn read_states(raw: Option<&RawValue>, model: &Model) -> HashMap<Id, ReadState> {
    let read_to_newest = |id: Id| {
        let newest = match model.dm(id) {
            Some(dm) => dm.last_message_id,
            None => {
                model
                    .guilds
                    .iter()
                    .find_map(|g| g.channel(id))?
                    .last_message_id
            }
        };
        Some((
            id,
            ReadState {
                last_read: newest,
                ..ReadState::default()
            },
        ))
    };
    let Some(entries) = versioned::<Box<RawValue>>(raw) else {
        let channels = model
            .guilds
            .iter()
            .flat_map(|g| g.channels.iter().map(|c| c.id));
        return channels
            .chain(model.dms.iter().map(|d| d.id))
            .filter_map(read_to_newest)
            .collect();
    };
    entries
        .iter()
        .filter_map(
            |raw| match serde_json::from_str::<WireReadState>(raw.get()) {
                Ok(r) => (r.read_state_type == CHANNEL_READ_STATE).then(|| {
                    let state = ReadState {
                        last_read: r.last_message_id,
                        mentions: count(r.mention_count),
                        flags: r.flags,
                    };
                    (r.id, state)
                }),
                Err(_) => serde_json::from_str::<ReadStateId>(raw.get())
                    .ok()
                    .filter(|r| r.read_state_type == CHANNEL_READ_STATE)
                    .and_then(|r| read_to_newest(r.id)),
            },
        )
        .collect()
}

/// A snowflake read states send as a string, or as the number 0 for none.
fn loose_snowflake<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Id>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Loose {
        Text(String),
        Number(u64),
    }
    let id = match serde::Deserialize::deserialize(deserializer)? {
        Some(Loose::Text(text)) => text.parse().map_err(serde::de::Error::custom)?,
        Some(Loose::Number(number)) => number,
        None => 0,
    };
    Ok(Some(id).filter(|&id| id != 0))
}

/// Discord's counts are never negative; a bad one counts as none.
fn count(count: i64) -> u32 {
    count.clamp(0, u32::MAX.into()) as u32
}

#[derive(serde::Deserialize)]
struct ReadySupplemental {
    #[serde(default, deserialize_with = "lenient")]
    lazy_private_channels: Vec<WireChannel>,
}

#[derive(serde::Deserialize)]
struct WireGuild {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    #[serde(default)]
    unavailable: bool,
    /// The guild's metadata, for user accounts.
    properties: Option<GuildProperties>,
    #[serde(default, deserialize_with = "lenient")]
    channels: Vec<WireChannel>,
    #[serde(default, deserialize_with = "lenient")]
    roles: Vec<WireRole>,
    /// GUILD_CREATE's members: the signed-in one is there.
    #[serde(default, deserialize_with = "lenient")]
    members: Vec<WireMember>,
    /// When I joined.
    joined_at: Option<String>,
}

/// GUILD_UPDATE: the fields that changed, top level or under `properties`.
#[derive(serde::Deserialize)]
struct GuildChange {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    name: Option<String>,
    #[serde(default, deserialize_with = "optional_snowflake")]
    owner_id: Option<Id>,
    properties: Option<GuildPropertiesChange>,
}

#[derive(serde::Deserialize)]
struct GuildPropertiesChange {
    name: Option<String>,
    #[serde(default, deserialize_with = "optional_snowflake")]
    owner_id: Option<Id>,
}

/// One guild's members in READY, read leniently so a member in an unknown
/// shape costs that member, not the guild's place in the list.
struct Members(Vec<WireMember>);

impl<'de> serde::Deserialize<'de> for Members {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        lenient(deserializer).map(Members)
    }
}

/// A list read element by element: an element in a shape this version does
/// not know is skipped (and counted in the log, by type only), rather than
/// failing the whole event.
fn lenient<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let raw: Vec<Box<RawValue>> = serde::Deserialize::deserialize(deserializer)?;
    let total = raw.len();
    let items: Vec<T> = raw
        .iter()
        .filter_map(|item| serde_json::from_str(item.get()).ok())
        .collect();
    if items.len() < total {
        log::warn!(
            "skipped {} unreadable {}",
            total - items.len(),
            std::any::type_name::<T>()
        );
    }
    Ok(items)
}

/// [`lenient`] for a list that may be missing, as in a partial update.
fn lenient_some<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    lenient(deserializer).map(Some)
}

/// One object read leniently: in a shape this version does not know, or
/// null, it is left out.
fn lenient_one<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let raw: Option<Box<RawValue>> = serde::Deserialize::deserialize(deserializer)?;
    Ok(raw.and_then(|raw| serde_json::from_str(raw.get()).ok()))
}

#[derive(serde::Deserialize)]
struct GuildProperties {
    name: String,
    #[serde(deserialize_with = "snowflake")]
    owner_id: Id,
    #[serde(default)]
    default_message_notifications: u8,
}

#[derive(serde::Deserialize)]
struct WireRole {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    #[serde(default)]
    name: String,
    #[serde(default)]
    position: i32,
    #[serde(deserialize_with = "permissions")]
    permissions: Permissions,
}

#[derive(serde::Deserialize)]
struct WireOverwrite {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    #[serde(rename = "type")]
    kind: u8,
    #[serde(deserialize_with = "permissions")]
    allow: Permissions,
    #[serde(deserialize_with = "permissions")]
    deny: Permissions,
}

#[derive(serde::Deserialize)]
struct WireMember {
    /// READY's merged members carry the id alone.
    #[serde(default, deserialize_with = "optional_snowflake")]
    user_id: Option<Id>,
    /// Other events nest the user.
    user: Option<WireMemberUser>,
    #[serde(default)]
    roles: Vec<String>,
}

#[derive(serde::Deserialize)]
struct WireMemberUser {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
}

impl WireMember {
    fn id(&self) -> Option<Id> {
        self.user_id.or(self.user.as_ref().map(|u| u.id))
    }

    fn roles(&self) -> Vec<Id> {
        self.roles.iter().filter_map(|r| r.parse().ok()).collect()
    }
}

#[derive(serde::Deserialize)]
struct RoleEvent {
    #[serde(deserialize_with = "snowflake")]
    guild_id: Id,
    role: WireRole,
}

#[derive(serde::Deserialize)]
struct RoleDelete {
    #[serde(deserialize_with = "snowflake")]
    guild_id: Id,
    #[serde(deserialize_with = "snowflake")]
    role_id: Id,
}

#[derive(serde::Deserialize)]
struct MemberUpdate {
    #[serde(deserialize_with = "snowflake")]
    guild_id: Id,
    #[serde(flatten)]
    member: WireMember,
}

/// Permission bitfields arrive as decimal strings: they outgrow JavaScript
/// numbers.
fn permissions<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Permissions, D::Error> {
    let text: String = serde::Deserialize::deserialize(deserializer)?;
    text.parse()
        .map(Permissions)
        .map_err(serde::de::Error::custom)
}

impl From<WireRole> for Role {
    fn from(role: WireRole) -> Self {
        Role {
            id: role.id,
            name: role.name,
            position: role.position,
            permissions: role.permissions,
        }
    }
}

/// An overwrite, unless its type is one this client does not know.
fn overwrite(wire: WireOverwrite) -> Option<Overwrite> {
    let kind = match wire.kind {
        0 => OverwriteKind::Role,
        1 => OverwriteKind::Member,
        _ => return None,
    };
    Some(Overwrite {
        id: wire.id,
        kind,
        allow: wire.allow,
        deny: wire.deny,
    })
}

#[derive(serde::Deserialize)]
struct WireChannel {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    #[serde(rename = "type")]
    kind: u8,
    name: Option<String>,
    #[serde(default, deserialize_with = "optional_snowflake")]
    parent_id: Option<Id>,
    #[serde(default, deserialize_with = "optional_snowflake")]
    guild_id: Option<Id>,
    #[serde(default)]
    position: i32,
    #[serde(default, deserialize_with = "optional_snowflake")]
    last_message_id: Option<Id>,
    #[serde(default, deserialize_with = "lenient")]
    permission_overwrites: Vec<WireOverwrite>,
    /// DMs in READY: users by id, resolved through READY's `users`.
    #[serde(default)]
    recipient_ids: Vec<String>,
    /// DMs in CHANNEL_CREATE: the users themselves.
    #[serde(default)]
    recipients: Vec<ApiUser>,
}

#[derive(serde::Deserialize)]
struct GuildDelete {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
    /// An outage rather than leaving: the guild comes back with GUILD_CREATE.
    #[serde(default)]
    unavailable: bool,
}

/// Discord's channel types, as far as fastcord shows them. Threads, forums
/// and the rest come later.
fn channel_kind(kind: u8) -> Option<ChannelKind> {
    match kind {
        0 => Some(ChannelKind::Text),
        2 | 13 => Some(ChannelKind::Voice),
        4 => Some(ChannelKind::Category),
        5 => Some(ChannelKind::Announcement),
        _ => None,
    }
}

const DM: u8 = 1;
const GROUP_DM: u8 = 3;

/// The read state type of a channel's messages.
const CHANNEL_READ_STATE: u8 = 0;

/// Reads events into [`Update`]s, remembering the users READY listed so
/// later events can name people by id.
#[derive(Default)]
pub struct Decoder {
    users: HashMap<Id, User>,
    /// The signed-in user, from READY.
    me: Id,
}

impl Decoder {
    fn user(&self, id: Id) -> User {
        self.users.get(&id).cloned().unwrap_or_else(|| User {
            id,
            username: "unknown-user".into(),
            global_name: None,
        })
    }

    fn guild_channel(wire: WireChannel) -> Option<Channel> {
        Some(Channel {
            kind: channel_kind(wire.kind)?,
            name: wire.name.unwrap_or_default(),
            id: wire.id,
            parent: wire.parent_id,
            position: wire.position,
            last_message_id: wire.last_message_id,
            overwrites: wire
                .permission_overwrites
                .into_iter()
                .filter_map(overwrite)
                .collect(),
        })
    }

    fn dm(&mut self, wire: WireChannel) -> Option<DmChannel> {
        if wire.kind != DM && wire.kind != GROUP_DM {
            return None;
        }
        // CHANNEL_CREATE names the recipients in full; READY names them by id.
        let mut recipients: Vec<User> = wire.recipients.into_iter().map(User::from).collect();
        for user in &recipients {
            self.users.insert(user.id, user.clone());
        }
        if recipients.is_empty() {
            recipients = wire
                .recipient_ids
                .iter()
                .filter_map(|id| id.parse().ok())
                .map(|id| self.user(id))
                .collect();
        }
        Some(DmChannel {
            id: wire.id,
            recipients,
            last_message_id: wire.last_message_id,
        })
    }

    /// A guild with my roles in it: READY's merged members (`my_roles`), or
    /// the members a GUILD_CREATE lists.
    fn guild(&self, wire: WireGuild, my_roles: Option<Vec<Id>>) -> Option<Guild> {
        if wire.unavailable {
            return None;
        }
        let properties = wire.properties?;
        let my_roles = my_roles
            .or_else(|| {
                wire.members
                    .iter()
                    .find(|m| m.id() == Some(self.me))
                    .map(WireMember::roles)
            })
            .unwrap_or_default();
        Some(Guild {
            id: wire.id,
            name: properties.name,
            owner_id: properties.owner_id,
            roles: wire.roles.into_iter().map(Role::from).collect(),
            my_roles,
            joined_at: wire.joined_at.and_then(|at| at.parse().ok()),
            default_notify: notify(Some(properties.default_message_notifications))
                .unwrap_or(Notify::All),
            channels: wire
                .channels
                .into_iter()
                .filter_map(Self::guild_channel)
                .collect(),
        })
    }

    /// READY: the whole model, and a refreshed token if Discord sent one.
    /// Fails only when READY itself is unreadable; a guild, channel or user
    /// in an unknown shape is skipped.
    pub fn ready(&mut self, data: &str) -> serde_json::Result<(Model, Option<String>)> {
        let ready: Ready = serde_json::from_str(data)?;
        self.me = ready.user.id;
        self.users = ready
            .users
            .into_iter()
            .chain(std::iter::once(ready.user))
            .map(|user| {
                let user = User::from(user);
                (user.id, user)
            })
            .collect();
        let me = self.me;
        let mut members = ready.merged_members.into_iter();
        let mut unreadable = 0;
        let guilds = ready
            .guilds
            .iter()
            .filter_map(|raw| {
                // Taken for every guild, readable or not, so each guild keeps
                // its own members.
                let my_roles = members
                    .next()
                    .and_then(|Members(members)| members.into_iter().find(|m| m.id() == Some(me)))
                    .map(|m| m.roles())
                    .unwrap_or_default();
                let Ok(wire) = serde_json::from_str::<WireGuild>(raw.get()) else {
                    unreadable += 1;
                    return None;
                };
                self.guild(wire, Some(my_roles))
            })
            .collect();
        if unreadable > 0 {
            log::warn!("skipped {unreadable} unreadable guilds in READY");
        }
        let mut model = Model {
            me,
            guilds,
            users: self.users.clone(),
            dms: ready
                .private_channels
                .into_iter()
                .filter_map(|wire| self.dm(wire))
                .collect(),
            guild_settings: versioned::<WireGuildSettings>(ready.user_guild_settings.as_deref())
                .unwrap_or_default()
                .into_iter()
                .map(WireGuildSettings::settings)
                .collect(),
            separate_unreads: ready
                .notification_settings
                .and_then(|raw| serde_json::from_str::<NotificationSettings>(raw.get()).ok())
                .is_some_and(|s| s.flags & USE_NEW_NOTIFICATIONS != 0),
            do_not_disturb: ready
                .user_settings_proto
                .and_then(|proto| do_not_disturb(&proto, false))
                .flatten(),
            ..Model::default()
        };
        model.read_states = read_states(ready.read_state.as_deref(), &model);
        Ok((model, ready.auth_token))
    }

    /// Any other event. Events fastcord does not show yield nothing.
    pub fn event(&mut self, name: &str, data: &str) -> serde_json::Result<Vec<Update>> {
        Ok(match name {
            "READY_SUPPLEMENTAL" => {
                let supplemental: ReadySupplemental = serde_json::from_str(data)?;
                supplemental
                    .lazy_private_channels
                    .into_iter()
                    .filter_map(|wire| self.dm(wire))
                    .map(Update::DmUpsert)
                    .collect()
            }
            "GUILD_CREATE" => {
                let wire: WireGuild = serde_json::from_str(data)?;
                self.guild(wire, None)
                    .map(Update::GuildUpsert)
                    .into_iter()
                    .collect()
            }
            "GUILD_UPDATE" => {
                let change: GuildChange = serde_json::from_str(data)?;
                let (name, owner_id) = match change.properties {
                    Some(p) => (p.name.or(change.name), p.owner_id.or(change.owner_id)),
                    None => (change.name, change.owner_id),
                };
                vec![Update::GuildChanged {
                    id: change.id,
                    name,
                    owner_id,
                }]
            }
            "GUILD_DELETE" => {
                let deleted: GuildDelete = serde_json::from_str(data)?;
                if deleted.unavailable {
                    Vec::new()
                } else {
                    vec![Update::GuildRemove(deleted.id)]
                }
            }
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" => {
                let wire: WireChannel = serde_json::from_str(data)?;
                match wire.guild_id {
                    Some(guild) => Self::guild_channel(wire)
                        .map(|channel| Update::ChannelUpsert { guild, channel })
                        .into_iter()
                        .collect(),
                    None => self.dm(wire).map(Update::DmUpsert).into_iter().collect(),
                }
            }
            "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => {
                let event: RoleEvent = serde_json::from_str(data)?;
                vec![Update::RoleUpsert {
                    guild: event.guild_id,
                    role: event.role.into(),
                }]
            }
            "GUILD_ROLE_DELETE" => {
                let event: RoleDelete = serde_json::from_str(data)?;
                vec![Update::RoleRemove {
                    guild: event.guild_id,
                    role: event.role_id,
                }]
            }
            "GUILD_MEMBER_UPDATE" => {
                let event: MemberUpdate = serde_json::from_str(data)?;
                if event.member.id() == Some(self.me) {
                    vec![Update::MyRoles {
                        guild: event.guild_id,
                        roles: event.member.roles(),
                    }]
                } else {
                    Vec::new()
                }
            }
            "MESSAGE_CREATE" => {
                let wire: WireMessage = serde_json::from_str(data)?;
                if !shown(wire.kind) {
                    return Ok(Vec::new());
                }
                let (channel, guild) = (wire.channel_id, wire.guild_id);
                let ping = wire.ping(self.me);
                let message = Message::from(wire);
                self.users.insert(message.author.id, message.author.clone());
                vec![Update::MessageCreate {
                    channel,
                    guild,
                    message,
                    ping,
                }]
            }
            "MESSAGE_UPDATE" => {
                let change: MessageChange = serde_json::from_str(data)?;
                vec![Update::MessageEdit {
                    channel: change.channel_id,
                    id: change.id,
                    content: change.content,
                    attachments: change
                        .attachments
                        .map(|list| list.into_iter().map(Into::into).collect()),
                    embeds: change
                        .embeds
                        .map(|list| list.into_iter().map(Into::into).collect()),
                }]
            }
            "MESSAGE_DELETE" | "MESSAGE_DELETE_BULK" => {
                let deleted: MessageDelete = serde_json::from_str(data)?;
                let ids = deleted
                    .id
                    .into_iter()
                    .chain(deleted.ids.iter().filter_map(|id| id.parse().ok()))
                    .collect();
                vec![Update::MessageDelete {
                    channel: deleted.channel_id,
                    ids,
                }]
            }
            "CHANNEL_DELETE" => {
                let wire: WireChannel = serde_json::from_str(data)?;
                match wire.guild_id {
                    Some(guild) => vec![Update::ChannelRemove {
                        guild,
                        channel: wire.id,
                    }],
                    None => vec![Update::DmRemove(wire.id)],
                }
            }
            "MESSAGE_ACK" => {
                let ack: MessageAck = serde_json::from_str(data)?;
                if ack.ack_type != CHANNEL_READ_STATE {
                    return Ok(Vec::new());
                }
                vec![Update::Acked {
                    channel: ack.channel_id,
                    message: ack.message_id,
                    manual: ack.manual,
                    mentions: ack.mention_count.map(count),
                    flags: ack.flags,
                }]
            }
            "USER_GUILD_SETTINGS_UPDATE" => {
                let wire: WireGuildSettings = serde_json::from_str(data)?;
                let (guild, settings) = wire.settings();
                vec![Update::GuildSettings { guild, settings }]
            }
            "PASSIVE_UPDATE_V1" | "PASSIVE_UPDATE_V2" | "CHANNEL_UNREAD_UPDATE" => {
                let unreads: ChannelUnreads = serde_json::from_str(data)?;
                vec![Update::LastMessages {
                    guild: unreads.guild_id,
                    channels: unreads
                        .updated_channels
                        .into_iter()
                        .map(|c| (c.id, c.last_message_id))
                        .collect(),
                }]
            }
            "USER_SETTINGS_PROTO_UPDATE" => {
                let update: SettingsUpdate = serde_json::from_str(data)?;
                if update.settings.kind != PRELOADED_USER_SETTINGS {
                    return Ok(Vec::new());
                }
                do_not_disturb(&update.settings.proto, update.partial)
                    .map(Update::DoNotDisturb)
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        })
    }
}

// My status, from the account settings Discord keeps as protobuf
// (`discord_protos.discord_users.v1.PreloadedUserSettings`). Only the path
// to the status is read: field 11 (`status`), then field 1 (`status`, a
// `StringValue` holding "online", "idle", "dnd"…) and field 4
// (`status_expires_at_ms`, a fixed64, 0 for none).

#[derive(serde::Deserialize)]
struct SettingsUpdate {
    settings: SettingsProto,
    /// Only the fields that changed.
    #[serde(default)]
    partial: bool,
}

#[derive(serde::Deserialize)]
struct SettingsProto {
    #[serde(rename = "type")]
    kind: u8,
    proto: String,
}

/// The settings type that holds the status.
const PRELOADED_USER_SETTINGS: u8 = 1;

/// Whether the settings set Do Not Disturb, and until when. `None` when they
/// say nothing of the status (a partial update leaves it alone) or cannot
/// be read; a full set without one means online.
fn do_not_disturb(base64: &str, partial: bool) -> Option<Option<Mute>> {
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD.decode(base64);
    let Some(status) = decoded.as_deref().ok().and_then(proto_status) else {
        log::warn!("unreadable settings protobuf");
        return None;
    };
    match status {
        Some((status, expires)) => Some((status == "dnd").then(|| {
            Mute {
                until: (expires > 0)
                    .then(|| i64::try_from(expires).ok())
                    .flatten()
                    .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok()),
            }
        })),
        None if partial => None,
        None => Some(None),
    }
}

/// The status and when it expires (0: never), or `Some(None)` without one.
fn proto_status(settings: &[u8]) -> Option<Option<(&str, u64)>> {
    let Some(ProtoValue::Bytes(status)) = proto_field(settings, 11)? else {
        return Some(None);
    };
    let Some(ProtoValue::Bytes(value)) = proto_field(status, 1)? else {
        return Some(None);
    };
    let text = match proto_field(value, 1)? {
        Some(ProtoValue::Bytes(text)) => std::str::from_utf8(text).ok()?,
        _ => "",
    };
    let expires = match proto_field(status, 4)? {
        Some(ProtoValue::Number(ms)) => ms,
        _ => 0,
    };
    Some(Some((text, expires)))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ProtoValue<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}

/// The last value of field `number` in a protobuf message (the last one
/// wins, as protobuf merges them): `Some(None)` when it is absent, `None`
/// when the message cannot be read.
fn proto_field(mut bytes: &[u8], number: u64) -> Option<Option<ProtoValue<'_>>> {
    let mut found = None;
    while !bytes.is_empty() {
        let key = varint(&mut bytes)?;
        let value = match key & 7 {
            0 => ProtoValue::Number(varint(&mut bytes)?),
            1 => ProtoValue::Number(u64::from_le_bytes(take(&mut bytes, 8)?.try_into().ok()?)),
            2 => {
                let len = usize::try_from(varint(&mut bytes)?).ok()?;
                ProtoValue::Bytes(take(&mut bytes, len)?)
            }
            5 => {
                ProtoValue::Number(u32::from_le_bytes(take(&mut bytes, 4)?.try_into().ok()?).into())
            }
            _ => return None,
        };
        if key >> 3 == number {
            found = Some(value);
        }
    }
    Some(found)
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Option<&'a [u8]> {
    let (head, rest) = bytes.split_at_checked(len)?;
    *bytes = rest;
    Some(head)
}

fn varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A READY with the shape Discord sends the web client, made-up values.
    const READY: &str = include_str!("fixtures/ready.json");

    fn ready() -> (Decoder, Model, Option<String>) {
        let mut decoder = Decoder::default();
        let (model, token) = decoder.ready(READY).unwrap();
        (decoder, model, token)
    }

    #[test]
    fn reads_guilds_from_their_properties() {
        let (_, model, _) = ready();
        assert_eq!(model.guilds.len(), 2, "the unavailable guild is left out");
        let guild = model.guild(1001).unwrap();
        assert_eq!(guild.name, "Rust Francophone");
        let general = guild.channel(2002).unwrap();
        assert_eq!(general.kind, ChannelKind::Text);
        assert_eq!(general.parent, Some(2001));
        assert_eq!(general.position, 0);
        // The forum channel is not shown yet.
        assert!(guild.channel(2009).is_none());
    }

    #[test]
    fn reads_permissions_and_my_roles() {
        let (_, model, _) = ready();
        assert_eq!(model.me, 9000);
        let guild = model.guild(1001).unwrap();
        assert_eq!(guild.owner_id, 9001);
        assert_eq!(guild.my_roles, [1001]);
        assert_eq!(guild.roles[0].permissions, Permissions(104324673));
        let general = guild.channel(2002).unwrap();
        assert_eq!(
            general.overwrites,
            [Overwrite {
                id: 1001,
                kind: OverwriteKind::Role,
                allow: Permissions(0),
                deny: Permissions(1024),
            }]
        );
        assert!(!guild.can_view(general, model.me), "@everyone is denied");
        assert!(guild.can_view(guild.channel(2004).unwrap(), model.me));
        // I own the second guild.
        let omarchy = model.guild(1002).unwrap();
        assert!(omarchy.can_view(omarchy.channel(2101).unwrap(), model.me));
    }

    #[test]
    fn role_and_member_events() {
        let (mut decoder, _, _) = ready();
        assert_eq!(
            decoder
                .event(
                    "GUILD_ROLE_UPDATE",
                    r#"{"guild_id":"1001","role":{"id":"1050","name":"mod","position":3,"permissions":"8"}}"#,
                )
                .unwrap(),
            [Update::RoleUpsert {
                guild: 1001,
                role: Role {
                    id: 1050,
                    name: "mod".into(),
                    position: 3,
                    permissions: Permissions::ADMINISTRATOR,
                },
            }]
        );
        assert_eq!(
            decoder
                .event(
                    "GUILD_ROLE_DELETE",
                    r#"{"guild_id":"1001","role_id":"1050"}"#
                )
                .unwrap(),
            [Update::RoleRemove {
                guild: 1001,
                role: 1050
            }]
        );
        assert_eq!(
            decoder
                .event(
                    "GUILD_MEMBER_UPDATE",
                    r#"{"guild_id":"1001","user":{"id":"9000","username":"imbu"},"roles":["1050"]}"#,
                )
                .unwrap(),
            [Update::MyRoles {
                guild: 1001,
                roles: vec![1050]
            }]
        );
        // Someone else's roles are not mine.
        assert_eq!(
            decoder
                .event(
                    "GUILD_MEMBER_UPDATE",
                    r#"{"guild_id":"1001","user":{"id":"9001","username":"marc"},"roles":["1050"]}"#,
                )
                .unwrap(),
            []
        );
    }

    #[test]
    fn a_joined_guild_finds_me_among_its_members() {
        let (mut decoder, _, _) = ready();
        let joined = decoder
            .event(
                "GUILD_CREATE",
                r#"{"id":"1004","properties":{"name":"New","owner_id":"9001"},"roles":[],"channels":[],"members":[{"user":{"id":"9000","username":"imbu"},"roles":["1077"]}]}"#,
            )
            .unwrap();
        assert!(matches!(&joined[..], [Update::GuildUpsert(g)] if g.my_roles == [1077]));
    }

    #[test]
    fn names_dm_recipients_through_the_users_list() {
        let (_, model, _) = ready();
        let dm = model.dm(3001).unwrap();
        assert_eq!(dm.title(), "Léa");
        assert_eq!(dm.last_message_id, Some(175928847299117063));
        let group = model.dm(3002).unwrap();
        assert_eq!(group.title(), "marc, unknown-user");
    }

    #[test]
    fn keeps_a_refreshed_token() {
        let (_, _, token) = ready();
        assert_eq!(token.as_deref(), Some("refreshed"));
    }

    #[test]
    fn late_dms_from_ready_supplemental() {
        let (mut decoder, _, _) = ready();
        let updates = decoder
            .event(
                "READY_SUPPLEMENTAL",
                r#"{"lazy_private_channels":[{"id":"3003","type":1,"recipient_ids":["9002"]}],"guilds":[]}"#,
            )
            .unwrap();
        assert_eq!(
            updates,
            [Update::DmUpsert(DmChannel {
                id: 3003,
                recipients: vec![User {
                    id: 9002,
                    username: "lea.dev".into(),
                    global_name: Some("Léa".into()),
                }],
                last_message_id: None,
            })]
        );
    }

    #[test]
    fn channel_events() {
        let (mut decoder, _, _) = ready();
        let created = decoder
            .event(
                "CHANNEL_CREATE",
                r#"{"id":"2010","type":0,"guild_id":"1001","name":"nouveau","position":3,"parent_id":"2001"}"#,
            )
            .unwrap();
        assert!(matches!(
            &created[..],
            [Update::ChannelUpsert { guild: 1001, channel }] if channel.name == "nouveau"
        ));
        let dm = decoder
            .event(
                "CHANNEL_CREATE",
                r#"{"id":"3004","type":1,"recipients":[{"id":"9005","username":"new","global_name":null}]}"#,
            )
            .unwrap();
        assert!(matches!(&dm[..], [Update::DmUpsert(dm)] if dm.title() == "new"));
        assert_eq!(
            decoder
                .event(
                    "CHANNEL_DELETE",
                    r#"{"id":"2010","type":0,"guild_id":"1001"}"#
                )
                .unwrap(),
            [Update::ChannelRemove {
                guild: 1001,
                channel: 2010
            }]
        );
    }

    #[test]
    fn guild_events() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder
                .event("GUILD_UPDATE", r#"{"id":"1001","name":"Renamed"}"#)
                .unwrap(),
            [Update::GuildChanged {
                id: 1001,
                name: Some("Renamed".into()),
                owner_id: None,
            }]
        );
        assert_eq!(
            decoder
                .event("GUILD_DELETE", r#"{"id":"1001","unavailable":true}"#)
                .unwrap(),
            []
        );
        assert_eq!(
            decoder.event("GUILD_DELETE", r#"{"id":"1001"}"#).unwrap(),
            [Update::GuildRemove(1001)]
        );
    }

    #[test]
    fn an_ownership_transfer_reaches_the_guild() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder
                .event(
                    "GUILD_UPDATE",
                    r#"{"id":"1001","properties":{"name":"Rust","owner_id":"9000"}}"#,
                )
                .unwrap(),
            [Update::GuildChanged {
                id: 1001,
                name: Some("Rust".into()),
                owner_id: Some(9000),
            }]
        );
    }

    #[test]
    fn an_unreadable_element_costs_only_itself() {
        // A guild whose channel type is out of range, a channel with a bad
        // overwrite, a user without a username, and a DM in an unknown shape.
        let ready = r#"{
            "user": {"id":"9000","username":"imbu","global_name":null},
            "users": [{"id":"9001"}, {"id":"9002","username":"lea","global_name":null}],
            "guilds": [
                {"id":"1","properties":{"name":"Bad","owner_id":"9"},"channels":[{"id":"5","type":999}]},
                {"id":"2","properties":{"name":"Good","owner_id":"9"},"channels":[
                    {"id":"6","type":0,"name":"ok","position":0},
                    {"id":"7","type":0,"name":"odd","position":1,"permission_overwrites":[{"id":"2","type":0,"allow":7,"deny":"0"}]}
                ]},
                {"id":"3","properties":{"name":"Mine","owner_id":"9"},"channels":[]}
            ],
            "merged_members": [[], [], [{"user_id":"9000","roles":["33"]}]],
            "private_channels": [{"id":"8","type":1,"recipient_ids":["9002"]}, {"type":"weird"}]
        }"#;
        let mut decoder = Decoder::default();
        let (model, _) = decoder.ready(ready).unwrap();
        let guilds: Vec<&str> = model.guilds.iter().map(|g| g.name.as_str()).collect();
        // The bad channel type fails only that channel, not the guild.
        assert_eq!(guilds, ["Bad", "Good", "Mine"]);
        assert!(model.guild(1).unwrap().channels.is_empty());
        let good = model.guild(2).unwrap();
        assert_eq!(good.channels.len(), 2);
        assert!(good.channel(7).unwrap().overwrites.is_empty());
        // Members stay with their own guild.
        assert_eq!(model.guild(3).unwrap().my_roles, [33]);
        assert_eq!(model.dms.len(), 1);
        assert_eq!(model.dms[0].title(), "lea");
    }

    #[test]
    fn a_history_page_reads_oldest_first_and_skips_system_messages() {
        let body = r#"[
            {"id":"30","channel_id":"7","type":0,"content":"third","author":{"id":"2","username":"lea","global_name":"Léa"}},
            {"id":"20","channel_id":"7","type":7,"content":"","author":{"id":"3","username":"marc","global_name":null}},
            {"id":"10","channel_id":"7","type":19,"content":"first","author":{"id":"3","username":"marc","global_name":null}},
            {"id":"5","channel_id":"7","type":0,"author":{"id":"4"}}
        ]"#;
        let Update::History {
            channel,
            messages,
            oldest,
            complete,
        } = history(7, body).unwrap()
        else {
            panic!("expected history");
        };
        assert_eq!(channel, 7);
        let ids: Vec<Id> = messages.iter().map(|m| m.id).collect();
        // The join (type 7) is left out; the author without a username too.
        assert_eq!(ids, [10, 30]);
        assert_eq!(messages[1].author.display_name(), "Léa");
        assert!(complete, "fewer than a page means the start is reached");
        assert_eq!(oldest, Some(5), "the cursor counts what is not shown");
    }

    #[test]
    fn a_full_page_with_unreadable_entries_is_not_the_start() {
        let entry = |id: usize| {
            format!(
                r#"{{"id":"{id}","channel_id":"7","type":7,"content":"","author":{{"id":"3","username":"m"}}}}"#
            )
        };
        let mut entries: Vec<String> = (100..100 + PAGE - 1).map(entry).collect();
        entries.push(r#"{"id":"99","odd":true}"#.into());
        let body = format!("[{}]", entries.join(","));
        let Update::History {
            messages,
            oldest,
            complete,
            ..
        } = history(7, &body).unwrap()
        else {
            panic!("expected history");
        };
        assert!(messages.is_empty(), "joins only");
        assert!(!complete, "a full page, readable or not");
        assert_eq!(oldest, Some(99));
    }

    /// A made-up message in the shape the web client receives.
    const WITH_MEDIA: &str = r#"{"id":"50","channel_id":"7","type":0,"content":"regarde","author":{"id":"5","username":"sam","global_name":null},
        "attachments":[
            {"id":"60","filename":"SPOILER_photo.png","size":48213,"url":"https://cdn.discordapp.com/attachments/7/60/SPOILER_photo.png?ex=1&is=2&hm=3&","proxy_url":"https://media.discordapp.net/attachments/7/60/SPOILER_photo.png?ex=1&is=2&hm=3&","content_type":"image/png","width":800,"height":600,"flags":8,"placeholder":"abc","placeholder_version":1},
            {"id":"61","filename":"notes.pdf","size":1024,"url":"https://cdn.discordapp.com/attachments/7/61/notes.pdf","proxy_url":"https://media.discordapp.net/attachments/7/61/notes.pdf","content_type":"application/pdf"},
            {"id":"62"}
        ],
        "embeds":[
            {"type":"rich","title":"Titre","description":"Du **texte**","url":"https://example.com/","color":13517355,
             "author":{"name":"Auteur","url":"https://example.com/a"},"footer":{"text":"Pied"},"provider":{"name":"Site"},
             "fields":[{"name":"A","value":"1","inline":true},{"name":"B"},{"name":"C","value":"3"}],
             "thumbnail":{"url":"https://example.com/t.png","proxy_url":"https://images-ext-1.discordapp.net/external/x/https/example.com/t.png","width":80,"height":80,"flags":0},
             "image":{"proxy_url":"https://images-ext-1.discordapp.net/external/y"}},
            {"type":"gifv","url":"https://tenor.com/view/x","provider":{"name":"Tenor","url":"https://tenor.co"},
             "thumbnail":{"url":"https://media.tenor.com/x.png","proxy_url":"https://images-ext-2.discordapp.net/external/z/https/media.tenor.com/x.png","width":498,"height":280},
             "video":{"url":"https://media.tenor.com/x.mp4","proxy_url":"https://images-ext-2.discordapp.net/external/v","width":498,"height":280}},
            {"type":"rich","color":"red"}
        ]}"#;

    #[test]
    fn messages_carry_attachments_and_embeds() {
        let mut decoder = Decoder::default();
        let created = decoder.event("MESSAGE_CREATE", WITH_MEDIA).unwrap();
        let [Update::MessageCreate { message, .. }] = &created[..] else {
            panic!("expected a message");
        };
        // The attachment without a filename or URL is left out, the message kept.
        assert_eq!(message.content, "regarde");
        let [photo, notes] = &message.attachments[..] else {
            panic!("expected two attachments");
        };
        assert_eq!(
            (photo.id, photo.size, photo.width, photo.height, photo.flags),
            (60, 48213, Some(800), Some(600), Attachment::SPOILER)
        );
        assert!(photo.proxy_url.starts_with("https://media.discordapp.net/"));
        assert_eq!(notes.content_type.as_deref(), Some("application/pdf"));
        assert_eq!((notes.width, notes.flags), (None, 0));

        // The embed with a colour in an unknown shape is left out.
        let [rich, gif] = &message.embeds[..] else {
            panic!("expected two embeds");
        };
        assert_eq!(rich.kind.as_deref(), Some("rich"));
        assert_eq!(rich.color, Some(0xce422b));
        assert_eq!(
            (
                rich.author.as_deref(),
                rich.footer.as_deref(),
                rich.provider.as_deref()
            ),
            (Some("Auteur"), Some("Pied"), Some("Site"))
        );
        // The field without a value is left out; `inline` defaults to false.
        let fields: Vec<(&str, bool)> = rich
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.inline))
            .collect();
        assert_eq!(fields, [("A", true), ("C", false)]);
        let thumbnail = rich.thumbnail.as_ref().unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (Some(80), Some(80)));
        // An image without its URL is left out, not the embed.
        assert_eq!(rich.image, None);
        assert_eq!(gif.kind.as_deref(), Some("gifv"));
        assert!(gif.thumbnail.as_ref().unwrap().proxy_url.is_some());

        // History pages read the same way.
        let Update::History { messages, .. } = history(7, &format!("[{WITH_MEDIA}]")).unwrap()
        else {
            panic!("expected history");
        };
        assert_eq!(messages[0].embeds.len(), 2);
    }

    #[test]
    fn a_link_preview_arrives_as_an_edit() {
        let mut decoder = Decoder::default();
        let edited = decoder
            .event(
                "MESSAGE_UPDATE",
                r#"{"id":"50","channel_id":"7","guild_id":"1","embeds":[{"type":"article","title":"Rust","url":"https://www.rust-lang.org/","provider":{"name":"rust-lang.org"}}]}"#,
            )
            .unwrap();
        let [
            Update::MessageEdit {
                content: None,
                attachments: None,
                embeds: Some(embeds),
                ..
            },
        ] = &edited[..]
        else {
            panic!("expected an edit of the embeds only");
        };
        assert_eq!(embeds[0].title.as_deref(), Some("Rust"));
    }

    #[test]
    fn message_events() {
        let mut decoder = Decoder::default();
        let created = decoder
            .event(
                "MESSAGE_CREATE",
                r#"{"id":"40","channel_id":"7","guild_id":"1","type":0,"content":"salut","author":{"id":"5","username":"sam","global_name":"Sam"},"mentions":[],"attachments":[],"embeds":[]}"#,
            )
            .unwrap();
        assert!(matches!(
            &created[..],
            [Update::MessageCreate { channel: 7, guild: Some(1), message, .. }] if message.content == "salut"
        ));
        assert_eq!(decoder.user(5).display_name(), "Sam");
        assert_eq!(
            decoder
                .event(
                    "MESSAGE_UPDATE",
                    r#"{"id":"40","channel_id":"7","embeds":[]}"#
                )
                .unwrap(),
            [Update::MessageEdit {
                channel: 7,
                id: 40,
                content: None,
                attachments: None,
                embeds: Some(vec![]),
            }]
        );
        assert_eq!(
            decoder
                .event(
                    "MESSAGE_DELETE_BULK",
                    r#"{"ids":["40","41"],"channel_id":"7","guild_id":"1"}"#
                )
                .unwrap(),
            [Update::MessageDelete {
                channel: 7,
                ids: vec![40, 41]
            }]
        );
    }

    #[test]
    fn reads_channel_read_states_and_mutes() {
        let (_, model, _) = ready();
        let guild = model.guild(1001).unwrap();
        assert_eq!(
            guild.joined_at,
            Some("2024-01-01T00:00:00Z".parse().unwrap())
        );
        assert_eq!(
            guild.channel(2002).unwrap().last_message_id,
            Some(175928847299117063)
        );
        // Guild events and the notification centre are not channels.
        let mut ids: Vec<Id> = model.read_states.keys().copied().collect();
        ids.sort();
        assert_eq!(ids, [2002, 2004, 3001]);
        assert_eq!(
            model.read_states[&2002],
            ReadState {
                last_read: Some(175928847299117063),
                mentions: 2,
                flags: Some(1),
            }
        );
        assert_eq!(model.read_states[&2004].last_read, None, "0 is none");
        let settings = &model.guild_settings[&Some(1001)];
        assert_eq!(
            settings.muted,
            Some(Mute {
                until: Some("2026-10-08T09:00:00Z".parse().unwrap())
            })
        );
        assert_eq!(settings.notify, Some(Notify::Mentions));
        assert_eq!(
            settings.channels[&2001],
            ChannelSettings {
                muted: Some(Mute { until: None }),
                notify: None,
                unreads: Some(Unreads::All),
            }
        );
        assert_eq!(settings.channels[&2004].notify, Some(Notify::Mentions));
        assert!(model.guild_settings[&None].channels[&3002].muted.is_some());
        assert!(model.separate_unreads);
        assert_eq!(guild.default_notify, Notify::Mentions);
    }

    #[test]
    fn unreadable_read_states_count_as_read() {
        let ready = |read_state: &str| {
            format!(
                r#"{{"user":{{"id":"9000","username":"imbu"}},
                "guilds":[{{"id":"1","joined_at":"2024-01-01T00:00:00+00:00","properties":{{"name":"G","owner_id":"9"}},
                    "channels":[{{"id":"5","type":0,"last_message_id":"900000000000000000"}},{{"id":"6","type":0,"last_message_id":"900000000000000001"}}]}}],
                "read_state":{read_state}}}"#
            )
        };
        let read = |text: &str| {
            Decoder::default()
                .ready(&ready(text))
                .unwrap()
                .0
                .read_states
        };
        // An entry in an odd shape: its channel is read up to its newest.
        let states = read(
            r#"{"entries":[{"id":"5","mention_count":"many"},{"id":"6","last_message_id":"1","mention_count":1}]}"#,
        );
        assert_eq!(states[&5].last_read, Some(900000000000000000));
        assert_eq!(states[&6].mentions, 1);
        // The whole list in an odd shape: every channel is.
        let states = read(r#"[1, 2]"#);
        assert_eq!(states[&6].last_read, Some(900000000000000001));
    }

    #[test]
    fn acks_and_settings_from_other_devices() {
        let mut decoder = Decoder::default();
        assert_eq!(
            decoder
                .event(
                    "MESSAGE_ACK",
                    r#"{"channel_id":"2002","message_id":"175928847299117070","version":12,"last_viewed":4200,"flags":null}"#,
                )
                .unwrap(),
            [Update::Acked {
                channel: 2002,
                message: 175928847299117070,
                manual: false,
                mentions: None,
                flags: None,
            }]
        );
        assert_eq!(
            decoder
                .event(
                    "MESSAGE_ACK",
                    r#"{"channel_id":"2002","message_id":"175928847299117000","manual":true,"mention_count":3,"version":13}"#,
                )
                .unwrap(),
            [Update::Acked {
                channel: 2002,
                message: 175928847299117000,
                manual: true,
                mentions: Some(3),
                flags: None,
            }]
        );
        // Another feature's read state.
        assert_eq!(
            decoder
                .event(
                    "MESSAGE_ACK",
                    r#"{"ack_type":1,"channel_id":"1001","message_id":"1","version":14}"#
                )
                .unwrap(),
            []
        );
        let updated = decoder
            .event(
                "USER_GUILD_SETTINGS_UPDATE",
                r#"{"guild_id":"1001","muted":false,"mute_config":null,"suppress_everyone":true,"channel_overrides":[{"channel_id":"2002","muted":true,"mute_config":{"end_time":null,"selected_time_window":-1}}],"version":4}"#,
            )
            .unwrap();
        assert_eq!(
            updated,
            [Update::GuildSettings {
                guild: Some(1001),
                settings: GuildSettings {
                    channels: HashMap::from([(
                        2002,
                        ChannelSettings {
                            muted: Some(Mute { until: None }),
                            ..ChannelSettings::default()
                        }
                    )]),
                    suppress_everyone: true,
                    ..GuildSettings::default()
                },
            }]
        );
    }

    #[test]
    fn newest_messages_of_guilds_not_streamed() {
        let mut decoder = Decoder::default();
        let expected = [Update::LastMessages {
            guild: 1001,
            channels: vec![(2002, Some(175928847299117070)), (2004, None)],
        }];
        let channels = r#"[{"id":"2002","last_message_id":"175928847299117070","last_pin_timestamp":null},{"id":"2004","last_message_id":null}]"#;
        assert_eq!(
            decoder
                .event(
                    "PASSIVE_UPDATE_V2",
                    &format!(
                        r#"{{"guild_id":"1001","updated_channels":{channels},"updated_voice_states":[],"removed_voice_states":[],"updated_members":[]}}"#
                    ),
                )
                .unwrap(),
            expected
        );
        assert_eq!(
            decoder
                .event(
                    "CHANNEL_UNREAD_UPDATE",
                    &format!(r#"{{"guild_id":"1001","channel_unread_updates":{channels}}}"#),
                )
                .unwrap(),
            expected
        );
    }

    #[test]
    fn messages_say_whom_they_ping() {
        let (mut decoder, _, _) = ready();
        let created = decoder
            .event(
                "MESSAGE_CREATE",
                r#"{"id":"50","channel_id":"2002","guild_id":"1001","type":19,"content":"<@9000> @everyone <@&1050>","author":{"id":"9001","username":"marc"},"mentions":[{"id":"9000","username":"imbu"}],"mention_everyone":true,"mention_roles":["1050"]}"#,
            )
            .unwrap();
        let [Update::MessageCreate { guild, ping, .. }] = &created[..] else {
            panic!("expected a message");
        };
        assert_eq!(*guild, Some(1001));
        assert_eq!(
            *ping,
            Ping {
                me: true,
                everyone: true,
                roles: vec![1050],
                silent: false,
            }
        );
    }

    #[test]
    fn silent_messages_say_so() {
        let (mut decoder, _, _) = ready();
        let created = decoder
            .event(
                "MESSAGE_CREATE",
                r#"{"id":"50","channel_id":"3001","content":"psst","flags":4096,"author":{"id":"9002","username":"lea.dev"}}"#,
            )
            .unwrap();
        let [Update::MessageCreate { ping, .. }] = &created[..] else {
            panic!("expected a message");
        };
        assert!(ping.silent);
    }

    /// Account settings as protobuf, base64: an unrelated field, then the
    /// status (and when it expires) the way Discord nests it.
    fn settings_proto(status: Option<&str>, expires_ms: u64) -> String {
        use base64::Engine as _;
        let mut inner = Vec::new();
        if let Some(status) = status {
            let value = [&[0x0a, status.len() as u8][..], status.as_bytes()].concat();
            inner.extend([0x0a, value.len() as u8]);
            inner.extend(value);
        }
        if expires_ms > 0 {
            inner.push(0x21);
            inner.extend(expires_ms.to_le_bytes());
        }
        // Field 2 holding a two-byte varint, then field 11 (status).
        let mut proto = vec![0x10, 0x96, 0x01];
        proto.extend([0x5a, inner.len() as u8]);
        proto.extend(inner);
        base64::engine::general_purpose::STANDARD.encode(proto)
    }

    #[test]
    fn reads_do_not_disturb_from_the_settings_protobuf() {
        let dnd = settings_proto(Some("dnd"), 0);
        assert_eq!(
            do_not_disturb(&dnd, false),
            Some(Some(Mute { until: None }))
        );
        let until = settings_proto(Some("dnd"), 1_800_000_000_000);
        assert_eq!(
            do_not_disturb(&until, true),
            Some(Some(Mute {
                until: Some(jiff::Timestamp::from_millisecond(1_800_000_000_000).unwrap())
            }))
        );
        assert_eq!(
            do_not_disturb(&settings_proto(Some("idle"), 0), true),
            Some(None)
        );
        // Without a status: online in full settings, unchanged in a partial update.
        assert_eq!(do_not_disturb(&settings_proto(None, 0), false), Some(None));
        assert_eq!(do_not_disturb(&settings_proto(None, 0), true), None);
        assert_eq!(do_not_disturb("", false), Some(None));
        // Unreadable: base64 that is not, a truncated message.
        assert_eq!(do_not_disturb("ignored!", false), None);
        assert_eq!(do_not_disturb("Wg8K", false), None);
    }

    #[test]
    fn my_status_comes_from_ready_and_settings_updates() {
        let (mut decoder, model, _) = ready();
        assert_eq!(
            model.do_not_disturb, None,
            "the fixture's proto is unreadable"
        );
        let dnd = settings_proto(Some("dnd"), 0);
        let ready = READY.replace(
            r#""user_settings_proto": "ignored""#,
            &format!(r#""user_settings_proto": "{dnd}""#),
        );
        let (model, _) = Decoder::default().ready(&ready).unwrap();
        assert_eq!(model.do_not_disturb, Some(Mute { until: None }));
        let event = |proto: &str, partial: bool, kind: u8| {
            format!(r#"{{"settings":{{"type":{kind},"proto":"{proto}"}},"partial":{partial}}}"#)
        };
        let update = |decoder: &mut Decoder, data: String| {
            decoder.event("USER_SETTINGS_PROTO_UPDATE", &data).unwrap()
        };
        assert_eq!(
            update(&mut decoder, event(&dnd, true, 1)),
            [Update::DoNotDisturb(Some(Mute { until: None }))]
        );
        assert_eq!(update(&mut decoder, event(&dnd, true, 2)), [], "frecency");
        let other = settings_proto(None, 0);
        assert_eq!(update(&mut decoder, event(&other, true, 1)), []);
    }

    #[test]
    fn other_events_are_ignored() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.event("PRESENCE_UPDATE", "{}").unwrap(), []);
    }
}
