//! The gateway's events, read into changes to the [`Model`].
//!
//! Shapes follow what Discord sends the web client with the capabilities
//! `gateway` asks for: READY lists every user once in `users` and DMs name
//! their recipients by id, guild metadata sits under `properties`, and DMs
//! may arrive late in READY_SUPPLEMENTAL. Only what fastcord shows is read;
//! serde skips the rest.

use crate::api::{ApiUser, optional_snowflake, snowflake};
use crate::model::{Channel, ChannelKind, DmChannel, Guild, Id, Model, User};
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
}

#[derive(serde::Deserialize)]
struct GuildProperties {
    name: String,
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

    fn guild(wire: WireGuild) -> Option<Guild> {
        if wire.unavailable {
            return None;
        }
        let name = wire.properties.map(|p| p.name).or(wire.name)?;
        Some(Guild {
            id: wire.id,
            name,
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
        self.users = ready
            .users
            .into_iter()
            .chain(std::iter::once(ready.user))
            .map(|user| {
                let user = User::from(user);
                (user.id, user)
            })
            .collect();
        let model = Model {
            guilds: ready.guilds.into_iter().filter_map(Self::guild).collect(),
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
                Self::guild(wire)
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
