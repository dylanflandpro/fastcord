//! Offline sample data for `--demo`: servers, channels, DMs and messages,
//! so the interface can be built without touching a Discord account.

use crate::model::{Channel, ChannelKind, DmChannel, Guild, Id, Message, Model, User, id_at};

fn user(id: Id, username: &str, global_name: Option<&str>) -> User {
    User {
        id,
        username: username.into(),
        global_name: global_name.map(Into::into),
    }
}

fn channel(id: Id, name: &str, kind: ChannelKind, parent: Option<Id>, position: i32) -> Channel {
    Channel {
        id,
        name: name.into(),
        kind,
        parent,
        position,
    }
}

/// Hands out message ids from one clock and one sequence for the whole demo,
/// so two messages never share a snowflake, even across channels.
struct Timeline {
    now: jiff::Timestamp,
    sequence: u64,
}

impl Timeline {
    /// A conversation, one message per `(minutes ago, author, text)`.
    fn history(&mut self, lines: &[(i64, &User, &str)]) -> Vec<Message> {
        lines
            .iter()
            .map(|(minutes_ago, author, content)| {
                self.sequence += 1;
                Message {
                    id: id_at(
                        self.now - jiff::SignedDuration::from_mins(*minutes_ago),
                        self.sequence,
                    ),
                    author: (*author).clone(),
                    content: (*content).into(),
                }
            })
            .collect()
    }
}

pub fn model() -> Model {
    let me = user(1, "dylan", Some("Dylan"));
    let lea = user(2, "lea.dev", Some("Léa"));
    let marc = user(3, "marc", None);
    let sam = user(4, "samuel_k", Some("Sam"));

    let rust = Guild {
        id: 100,
        name: "Rust Francophone".into(),
        channels: vec![
            channel(101, "annonces", ChannelKind::Announcement, None, 0),
            channel(110, "Discussions", ChannelKind::Category, None, 0),
            channel(111, "général", ChannelKind::Text, Some(110), 0),
            channel(112, "aide", ChannelKind::Text, Some(110), 1),
            channel(113, "egui", ChannelKind::Text, Some(110), 2),
            channel(120, "Vocal", ChannelKind::Category, None, 1),
            channel(121, "Salon vocal", ChannelKind::Voice, Some(120), 0),
        ],
    };
    let omarchy = Guild {
        id: 200,
        name: "Omarchy".into(),
        channels: vec![
            channel(201, "general", ChannelKind::Text, None, 0),
            channel(202, "themes", ChannelKind::Text, None, 1),
            channel(203, "hyprland", ChannelKind::Text, None, 2),
        ],
    };
    let bear = Guild {
        id: 300,
        name: "BearStudio".into(),
        channels: vec![
            channel(310, "Équipe", ChannelKind::Category, None, 0),
            channel(311, "random", ChannelKind::Text, Some(310), 0),
            channel(312, "veille", ChannelKind::Text, Some(310), 1),
        ],
    };

    let mut model = Model {
        guilds: vec![rust, omarchy, bear],
        ..Model::default()
    };
    let mut timeline = Timeline {
        now: jiff::Timestamp::now(),
        sequence: 0,
    };

    model.messages.insert(
        101,
        timeline.history(&[(
            1440,
            &marc,
            "Le meetup de jeudi est confirmé : 19 h, salle B. Pensez à vous inscrire dans #général.",
        )]),
    );
    model.messages.insert(
        111,
        timeline.history(&[
            (95, &marc, "Quelqu'un a déjà testé egui 0.36 ?"),
            (
                94,
                &marc,
                "Le nouveau `Panel` remplace `SidePanel` et `TopBottomPanel`.",
            ),
            (80, &lea, "Oui, la migration prend dix minutes 👌"),
            (
                79,
                &lea,
                "Le plus long c'est de passer de `update` à `logic` + `ui` sur `eframe::App`.",
            ),
            (
                42,
                &me,
                "Je démarre un client Discord natif dessus, sans navigateur embarqué.",
            ),
            (41, &me, "Objectif : ouvrir en moins d'une seconde 🚀"),
            (12, &sam, "Ça m'intéresse, tu publies le repo ?"),
            (3, &me, "Oui, il est public dès aujourd'hui."),
        ]),
    );
    model.messages.insert(
        112,
        timeline.history(&[
            (
                300,
                &sam,
                "Comment on fait un `Arc<Mutex<T>>` sans deadlock ?",
            ),
            (
                290,
                &lea,
                "On garde le verrou le moins longtemps possible, et jamais pendant un `.await`.",
            ),
        ]),
    );
    model.messages.insert(
        202,
        timeline.history(&[(
            60,
            &marc,
            "Le thème Ristretto est vraiment bien sur un écran OLED.",
        )]),
    );

    let dm_lea = DmChannel {
        id: 900,
        recipients: vec![lea.clone()],
        last_message_id: None,
    };
    let dm_group = DmChannel {
        id: 901,
        recipients: vec![marc.clone(), sam.clone()],
        last_message_id: None,
    };
    model.messages.insert(
        900,
        timeline.history(&[
            (30, &lea, "Tu passes au meetup jeudi ?"),
            (28, &me, "Oui, je viendrai avec la démo."),
        ]),
    );
    model.messages.insert(
        901,
        timeline.history(&[(240, &sam, "On se fait un point demain ?")]),
    );
    model.dms = [dm_lea, dm_group]
        .into_iter()
        .map(|mut dm| {
            dm.last_message_id = model.messages(dm.id).last().map(|m| m.id);
            dm
        })
        .collect();
    model
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_message_has_its_own_id() {
        let model = model();
        let ids: Vec<Id> = model.messages.values().flatten().map(|m| m.id).collect();
        let unique: HashSet<Id> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len());
    }

    #[test]
    fn histories_are_oldest_first() {
        for messages in model().messages.values() {
            assert!(messages.windows(2).all(|pair| pair[0].id < pair[1].id));
        }
    }
}
