//! What the client knows about the account, kept in memory only.
//!
//! The shapes follow Discord's own objects closely enough that the gateway
//! can fill them later, and no further than the interface needs.

use std::collections::HashMap;

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
}

#[derive(Clone, Debug, PartialEq)]
pub struct Guild {
    pub id: Id,
    pub name: String,
    pub channels: Vec<Channel>,
}

/// One line of a guild's channel list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Entry<'a> {
    Category(&'a Channel),
    Channel(&'a Channel),
}

impl Guild {
    /// The channel list as Discord draws it: channels outside any category
    /// first, then each category followed by its channels, all by position
    /// (ties broken by id, as Discord does).
    pub fn sidebar(&self) -> Vec<Entry<'_>> {
        let by_position = |a: &&Channel, b: &&Channel| (a.position, a.id).cmp(&(b.position, b.id));
        let children = |parent: Option<Id>| {
            let mut channels: Vec<&Channel> = self
                .channels
                .iter()
                .filter(|c| c.kind != ChannelKind::Category && c.parent == parent)
                .collect();
            channels.sort_by(by_position);
            channels
        };
        let mut categories: Vec<&Channel> = self
            .channels
            .iter()
            .filter(|c| c.kind == ChannelKind::Category)
            .collect();
        categories.sort_by(by_position);

        let mut entries: Vec<Entry<'_>> = children(None).into_iter().map(Entry::Channel).collect();
        for category in categories {
            entries.push(Entry::Category(category));
            entries.extend(children(Some(category.id)).into_iter().map(Entry::Channel));
        }
        entries
    }

    /// The channel to open when the guild is selected.
    pub fn first_text_channel(&self) -> Option<Id> {
        self.sidebar().into_iter().find_map(|entry| match entry {
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

#[derive(Debug, Default)]
pub struct Model {
    pub guilds: Vec<Guild>,
    pub dms: Vec<DmChannel>,
    /// Loaded history per channel, oldest first.
    pub messages: HashMap<Id, Vec<Message>>,
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
        let guild = Guild {
            id: 1,
            name: "g".into(),
            channels: vec![
                channel(20, ChannelKind::Category, None, 1),
                channel(10, ChannelKind::Category, None, 0),
                channel(21, ChannelKind::Text, Some(20), 0),
                channel(12, ChannelKind::Voice, Some(10), 1),
                channel(11, ChannelKind::Text, Some(10), 0),
                channel(2, ChannelKind::Text, None, 5),
            ],
        };
        assert_eq!(
            ids(&guild.sidebar()),
            ["2", "[10]", "11", "12", "[20]", "21"]
        );
    }

    #[test]
    fn sidebar_breaks_position_ties_by_id() {
        let guild = Guild {
            id: 1,
            name: "g".into(),
            channels: vec![
                channel(9, ChannelKind::Text, None, 0),
                channel(3, ChannelKind::Text, None, 0),
            ],
        };
        assert_eq!(ids(&guild.sidebar()), ["3", "9"]);
    }

    #[test]
    fn first_text_channel_skips_voice() {
        let guild = Guild {
            id: 1,
            name: "g".into(),
            channels: vec![
                channel(5, ChannelKind::Voice, None, 0),
                channel(6, ChannelKind::Text, None, 1),
            ],
        };
        assert_eq!(guild.first_text_channel(), Some(6));
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
