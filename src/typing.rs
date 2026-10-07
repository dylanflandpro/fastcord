//! Typing indicators both ways, timed as the official client times them
//! (its `TypingStore`).

use crate::model::Id;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How long someone shows as typing after their last TYPING_START.
const SHOWN_FOR: Duration = Duration::from_secs(10);
/// I tell Discord at most this often (0.8 × [`SHOWN_FOR`]), so my
/// indicator never lapses while I type.
const RESEND: Duration = Duration::from_secs(8);
/// How long a keystroke waits before Discord is told, so a quick "ok" sent
/// at once shows nothing. After a long pause in the same draft, the client
/// tells it at once.
const DELAY: Duration = Duration::from_millis(1500);

/// Who is typing where, in the order they started.
#[derive(Debug, Default)]
pub struct Others {
    until: HashMap<Id, Vec<(Id, Instant)>>,
}

impl Others {
    pub fn start(&mut self, channel: Id, user: Id, now: Instant) {
        let typing = self.until.entry(channel).or_default();
        match typing.iter_mut().find(|(u, _)| *u == user) {
            Some(entry) => entry.1 = now + SHOWN_FOR,
            None => typing.push((user, now + SHOWN_FOR)),
        }
    }

    /// Forgets the indicators that lapsed, so the record never grows.
    pub fn prune(&mut self, now: Instant) {
        for typing in self.until.values_mut() {
            typing.retain(|(_, until)| now < *until);
        }
        self.until.retain(|_, typing| !typing.is_empty());
    }

    /// Their message arrived: they are done.
    pub fn stop(&mut self, channel: Id, user: Id) {
        if let Some(typing) = self.until.get_mut(&channel) {
            typing.retain(|(u, _)| *u != user);
        }
    }

    pub fn who(&self, channel: Id, now: Instant) -> Vec<Id> {
        let typing = self.until.get(&channel).into_iter().flatten();
        typing
            .filter(|(_, until)| now < *until)
            .map(|(u, _)| *u)
            .collect()
    }

    /// When the next indicator lapses, to redraw then.
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        let all = self.until.values().flatten().map(|(_, until)| *until);
        all.filter(|until| *until > now).min()
    }
}

/// The line under the composer, worded as the official client words it.
pub fn line(names: &[&str]) -> Option<String> {
    Some(match names {
        [] => return None,
        [one] => format!("{one} is typing…"),
        [one, two] => format!("{one} and {two} are typing…"),
        [one, two, three] => format!("{one}, {two} and {three} are typing…"),
        _ => "Several people are typing…".into(),
    })
}

/// How many people typing already make my own indicator pointless: the web
/// client sends none past five.
pub const CROWD: usize = 5;

/// When to tell Discord that I am typing.
#[derive(Debug, Default)]
pub struct Mine {
    channel: Option<Id>,
    /// When the next POST is due, once a keystroke asked for one.
    due: Option<Instant>,
    /// When the last one was asked for.
    asked: Option<Instant>,
}

impl Mine {
    /// A keystroke in `channel`'s composer.
    pub fn keystroke(&mut self, channel: Id, now: Instant) {
        if self.channel != Some(channel) {
            *self = Self {
                channel: Some(channel),
                ..Self::default()
            };
        }
        if self.due.is_some() || self.asked.is_some_and(|at| now < at + RESEND) {
            return;
        }
        let paused = self.asked.is_some_and(|at| now >= at + 2 * RESEND);
        self.due = Some(if paused { now } else { now + DELAY });
        self.asked = Some(now);
    }

    /// The draft was sent or emptied: nothing is owed, and the next
    /// keystroke starts afresh.
    pub fn stop(&mut self) {
        *self = Self::default();
    }

    /// Another conversation opened: what was owed to the last one is not.
    pub fn follow(&mut self, open: Option<Id>) {
        if self.channel.is_some() && self.channel != open {
            self.stop();
        }
    }

    /// The channel to tell now, once.
    pub fn due(&mut self, now: Instant) -> Option<Id> {
        if self.due? > now {
            return None;
        }
        self.due = None;
        self.channel
    }

    /// When [`Self::due`] will have something, to wake up then.
    pub fn next_change(&self) -> Option<Instant> {
        self.due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f32) -> Duration {
        Duration::from_secs_f32(s)
    }

    #[test]
    fn others_show_for_ten_seconds_or_until_their_message() {
        let t0 = Instant::now();
        let mut others = Others::default();
        others.start(1, 7, t0);
        others.start(1, 8, t0 + secs(2.0));
        assert_eq!(others.who(1, t0 + secs(9.0)), [7, 8]);
        // Renewed, 7 keeps its place.
        others.start(1, 7, t0 + secs(9.0));
        assert_eq!(others.who(1, t0 + secs(11.0)), [7, 8]);
        assert_eq!(others.who(1, t0 + secs(12.5)), [7]);
        assert_eq!(others.next_change(t0 + secs(12.5)), Some(t0 + secs(19.0)));
        others.stop(1, 7);
        assert!(others.who(1, t0 + secs(12.5)).is_empty());
        assert!(others.who(2, t0).is_empty());
        others.start(2, 9, t0);
        others.prune(t0 + secs(30.0));
        assert!(others.until.is_empty(), "lapsed ones are forgotten");
    }

    #[test]
    fn typing_lines_follow_discords_wording() {
        assert_eq!(line(&[]), None);
        assert_eq!(line(&["Léa"]).unwrap(), "Léa is typing…");
        assert_eq!(line(&["Léa", "Sam"]).unwrap(), "Léa and Sam are typing…");
        assert_eq!(
            line(&["Léa", "Sam", "marc"]).unwrap(),
            "Léa, Sam and marc are typing…"
        );
        assert_eq!(
            line(&["a", "b", "c", "d"]).unwrap(),
            "Several people are typing…"
        );
    }

    #[test]
    fn i_tell_discord_after_a_moment_then_every_eight_seconds() {
        let t0 = Instant::now();
        let mut mine = Mine::default();
        mine.keystroke(1, t0);
        assert_eq!(mine.due(t0 + secs(1.0)), None);
        assert_eq!(mine.due(t0 + secs(1.5)), Some(1));
        assert_eq!(mine.due(t0 + secs(2.0)), None, "once");
        mine.keystroke(1, t0 + secs(7.0));
        assert_eq!(mine.due(t0 + secs(7.0)), None, "too soon");
        mine.keystroke(1, t0 + secs(8.0));
        assert_eq!(mine.due(t0 + secs(9.0)), None);
        assert_eq!(mine.due(t0 + secs(9.5)), Some(1));
        // A long pause in the same draft: told at once.
        mine.keystroke(1, t0 + secs(30.0));
        assert_eq!(mine.due(t0 + secs(30.0)), Some(1));
    }

    #[test]
    fn sending_before_the_moment_tells_nothing() {
        let t0 = Instant::now();
        let mut mine = Mine::default();
        mine.keystroke(1, t0);
        mine.stop();
        assert_eq!(mine.due(t0 + secs(5.0)), None);
        // Another channel starts afresh.
        mine.keystroke(1, t0 + secs(6.0));
        mine.keystroke(2, t0 + secs(6.5));
        assert_eq!(mine.due(t0 + secs(7.6)), None);
        assert_eq!(mine.due(t0 + secs(8.0)), Some(2));
        // Leaving the channel before the moment tells nothing either.
        mine.keystroke(2, t0 + secs(30.0));
        mine.follow(Some(3));
        assert_eq!(mine.due(t0 + secs(40.0)), None);
    }
}
