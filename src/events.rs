//! The gateway's events, read into changes to the [`Model`].
//!
//! Shapes follow what Discord sends the web client with the capabilities
//! `gateway` asks for: READY lists every user once in `users` and DMs name
//! their recipients by id, guild metadata sits under `properties`, and DMs
//! may arrive late in READY_SUPPLEMENTAL. Only what fastcord shows is read;
//! serde skips the rest.

use crate::api::{ApiUser, optional_snowflake, snowflake};
use crate::model::{
    Channel, ChannelKind, DmChannel, Guild, Id, Model, Overwrite, OverwriteKind, Permissions, Role,
    User,
};
use std::collections::HashMap;

/// A change to the model, in the order the gateway reported it.
#[derive(Debug, PartialEq)]
pub enum Update {
    /// Everything at once, from READY.
    Ready(Model),
    /// A guild joined or became available.
    GuildUpsert(Guild),
    GuildRename {
        id: Id,
        name: String,
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
}

#[derive(serde::Deserialize)]
struct Ready {
    user: ApiUser,
    #[serde(default)]
    users: Vec<ApiUser>,
    #[serde(default)]
    guilds: Vec<WireGuild>,
    /// Per guild, in `guilds`' order: the members READY describes, the
    /// signed-in one among them.
    #[serde(default)]
    merged_members: Vec<Vec<WireMember>>,
    #[serde(default)]
    private_channels: Vec<WireChannel>,
    /// A refreshed token, when Discord rotates it.
    auth_token: Option<String>,
}

#[derive(serde::Deserialize)]
struct ReadySupplemental {
    #[serde(default)]
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
    /// Where bots and older payloads put the name.
    name: Option<String>,
    #[serde(default)]
    channels: Vec<WireChannel>,
    #[serde(default)]
    roles: Vec<WireRole>,
    /// GUILD_CREATE's members: the signed-in one is there.
    #[serde(default)]
    members: Vec<WireMember>,
}

#[derive(serde::Deserialize)]
struct GuildProperties {
    name: String,
    #[serde(deserialize_with = "snowflake")]
    owner_id: Id,
}

#[derive(serde::Deserialize)]
struct WireRole {
    #[serde(deserialize_with = "snowflake")]
    id: Id,
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
    #[serde(default)]
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
            channels: wire
                .channels
                .into_iter()
                .filter_map(Self::guild_channel)
                .collect(),
        })
    }

    /// READY: the whole model, and a refreshed token if Discord sent one.
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
        let guilds = ready
            .guilds
            .into_iter()
            .filter_map(|wire| {
                let my_roles = members
                    .next()
                    .and_then(|members| members.into_iter().find(|m| m.id() == Some(me)))
                    .map(|m| m.roles());
                self.guild(wire, Some(my_roles.unwrap_or_default()))
            })
            .collect();
        let model = Model {
            me,
            guilds,
            dms: ready
                .private_channels
                .into_iter()
                .filter_map(|wire| self.dm(wire))
                .collect(),
            ..Model::default()
        };
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
                let wire: WireGuild = serde_json::from_str(data)?;
                let id = wire.id;
                wire.properties
                    .map(|p| p.name)
                    .or(wire.name)
                    .map(|name| Update::GuildRename { id, name })
                    .into_iter()
                    .collect()
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
            _ => Vec::new(),
        })
    }
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
            [Update::GuildRename {
                id: 1001,
                name: "Renamed".into()
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
    fn other_events_are_ignored() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.event("PRESENCE_UPDATE", "{}").unwrap(), []);
    }
}
