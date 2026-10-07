//! Read acks on their way to Discord: held, coalesced, retried, dropped.
//!
//! The web client holds an ack three seconds and keeps the newest per
//! channel (its outgoing ack timer). On top of that, an ack that fails on
//! the network or the server is retried until it lands, never overtaken by
//! an older one, so the read position Discord keeps never moves back.

use crate::events::Update;
use crate::model::{Ack, Id};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

/// How long the web client holds an ack, so reading through a busy channel
/// sends one request rather than one per message.
pub const ACK_DELAY: Duration = Duration::from_secs(3);

/// The longest wait between two attempts, and the longest `retry_after`
/// honoured.
pub const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// What became of one ack request.
#[derive(Debug, PartialEq)]
pub enum Delivery {
    Saved,
    /// Try again: after Discord's `retry_after`, or after the backoff.
    Retry(Option<Duration>),
    /// Gone for good: the channel was deleted, or is no longer mine to read.
    Dropped,
    Unauthorized,
}

/// The wait before attempt `attempt + 1`: one second, doubling, capped.
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(6)).min(MAX_RETRY_WAIT)
}

struct Waiting {
    ack: Ack,
    due: Instant,
    /// Attempts already failed.
    attempt: u32,
}

#[derive(Default)]
pub struct AckQueue {
    pending: HashMap<Id, Waiting>,
    /// Channels with a request on its way, and whether it is still wanted:
    /// no second request for a channel goes out before the first is back.
    in_flight: HashMap<Id, bool>,
}

impl AckQueue {
    /// Queues an ack, due [`ACK_DELAY`] after the first one waiting for its
    /// channel, or at once when it is immediate. The newest message wins;
    /// a flags change an earlier one carried is kept.
    pub fn push(&mut self, ack: Ack, now: Instant) {
        let earlier = self.pending.remove(&ack.channel);
        let flags = ack.flags.or(earlier.as_ref().and_then(|w| w.ack.flags));
        let due = match &earlier {
            _ if ack.immediate => now,
            Some(waiting) => waiting.due,
            None => now + ACK_DELAY,
        };
        let ack = Ack { flags, ..ack };
        let waiting = Waiting {
            ack,
            due,
            attempt: 0,
        };
        self.pending.insert(ack.channel, waiting);
    }

    /// Drops what waits for `channel`; a request on its way is not retried.
    pub fn cancel(&mut self, channel: Id) {
        self.pending.remove(&channel);
        if let Some(wanted) = self.in_flight.get_mut(&channel) {
            *wanted = false;
        }
    }

    /// What the gateway reports that changes what may be acked: a channel
    /// marked unread on another device (the web client's
    /// `clearOutgoingAck`), my own message (Discord reads the channel for
    /// me), a channel or guild gone.
    pub fn follow(&mut self, update: &Update, me: Id) {
        match update {
            Update::Acked {
                channel,
                manual: true,
                ..
            }
            | Update::ChannelRemove { channel, .. }
            | Update::DmRemove(channel) => self.cancel(*channel),
            Update::MessageCreate {
                channel, message, ..
            } if message.author.id == me => self.cancel(*channel),
            Update::GuildRemove(guild) => {
                let channels: Vec<Id> = self
                    .pending
                    .values()
                    .filter(|w| w.ack.guild == Some(*guild))
                    .map(|w| w.ack.channel)
                    .collect();
                channels.into_iter().for_each(|c| self.cancel(c));
            }
            _ => {}
        }
    }

    /// When the next ack may go out.
    pub fn next_due(&self) -> Option<Instant> {
        self.pending
            .values()
            .filter(|w| !self.in_flight.contains_key(&w.ack.channel))
            .map(|w| w.due)
            .min()
    }

    /// The acks due by `now` (all of them with `None`), each with the
    /// attempts it already failed, now counted as on their way.
    pub fn take_due(&mut self, now: Option<Instant>) -> Vec<(Ack, u32)> {
        let due: Vec<Id> = self
            .pending
            .values()
            .filter(|w| !self.in_flight.contains_key(&w.ack.channel))
            .filter(|w| now.is_none_or(|now| w.due <= now))
            .map(|w| w.ack.channel)
            .collect();
        due.into_iter()
            .filter_map(|channel| self.pending.remove(&channel))
            .map(|w| {
                self.in_flight.insert(w.ack.channel, true);
                (w.ack, w.attempt)
            })
            .collect()
    }

    /// A request came back. One that failed goes back in line after
    /// `retry`, unless it was cancelled meanwhile or a newer ack for its
    /// channel waits already (which then carries its flags).
    pub fn finished(&mut self, ack: Ack, attempt: u32, retry: Option<Duration>, now: Instant) {
        let wanted = self.in_flight.remove(&ack.channel).unwrap_or(false);
        let Some(after) = retry.filter(|_| wanted) else {
            return;
        };
        match self.pending.get_mut(&ack.channel) {
            Some(newer) => newer.ack.flags = newer.ack.flags.or(ack.flags),
            None => {
                let waiting = Waiting {
                    ack,
                    due: now + after,
                    attempt: attempt + 1,
                };
                self.pending.insert(ack.channel, waiting);
            }
        }
    }

    /// Nothing waits and nothing is on its way.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.in_flight.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, User};

    pub fn ack(channel: Id, message: Id, flags: Option<u32>, immediate: bool) -> Ack {
        Ack {
            guild: Some(1),
            channel,
            message,
            flags,
            immediate,
        }
    }

    fn paused(test: impl std::future::Future<Output = ()>) {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(test);
    }

    #[test]
    fn acks_wait_and_keep_the_newest_per_channel() {
        paused(async {
            let start = Instant::now();
            let mut acks = AckQueue::default();
            acks.push(ack(1, 10, Some(1), false), start);
            tokio::time::advance(Duration::from_secs(2)).await;
            acks.push(ack(1, 11, None, false), Instant::now());
            acks.push(ack(2, 20, None, false), Instant::now());
            // Due three seconds after the first, not the last.
            assert_eq!(acks.next_due(), Some(start + ACK_DELAY));
            assert!(acks.take_due(Some(Instant::now())).is_empty());
            tokio::time::advance(Duration::from_secs(1)).await;
            assert_eq!(
                acks.take_due(Some(Instant::now())),
                [(ack(1, 11, Some(1), false), 0)]
            );
        });
    }

    #[test]
    fn mentions_go_at_once() {
        let now = Instant::now();
        let mut acks = AckQueue::default();
        acks.push(ack(1, 10, Some(1), false), now);
        acks.push(ack(1, 12, None, true), now);
        assert_eq!(acks.take_due(Some(now)), [(ack(1, 12, Some(1), true), 0)]);
    }

    #[test]
    fn failures_retry_without_ever_overtaking() {
        let now = Instant::now();
        let mut acks = AckQueue::default();
        acks.push(ack(1, 10, Some(1), true), now);
        let [(sent, attempt)] = acks.take_due(Some(now))[..] else {
            panic!("one ack");
        };
        // A newer read waits while the first is on its way.
        acks.push(ack(1, 11, None, true), now);
        assert_eq!(acks.next_due(), None);
        // The first fails: the newer one goes, with the flags that did not
        // land.
        acks.finished(sent, attempt, Some(backoff(attempt)), now);
        assert_eq!(acks.take_due(Some(now)), [(ack(1, 11, Some(1), true), 0)]);
        // That one fails too and comes back after the backoff.
        acks.finished(ack(1, 11, Some(1), true), 0, Some(backoff(0)), now);
        assert_eq!(acks.next_due(), Some(now + Duration::from_secs(1)));
        assert_eq!(
            acks.take_due(None),
            [(ack(1, 11, Some(1), true), 1)],
            "second attempt"
        );
        acks.finished(ack(1, 11, Some(1), true), 1, None, now);
        assert!(acks.is_empty());
    }

    #[test]
    fn backoff_doubles_up_to_a_minute() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(30), MAX_RETRY_WAIT);
    }

    #[test]
    fn the_gateway_cancels_what_must_not_be_acked() {
        let now = Instant::now();
        let mut acks = AckQueue::default();
        for channel in [1, 2, 3, 4] {
            acks.push(ack(channel, 10, None, false), now);
        }
        // #4 is on its way when it is marked unread elsewhere: not retried.
        acks.push(ack(4, 10, None, true), now);
        let flying = acks.take_due(Some(now));
        let me = User {
            id: 9,
            ..User::default()
        };
        let mine = Message {
            id: 11,
            author: me,
            content: String::new(),
            attachments: vec![],
            embeds: vec![],
        };
        let marked = |channel| Update::Acked {
            channel,
            message: 5,
            manual: true,
            mentions: None,
            flags: None,
        };
        acks.follow(&marked(1), 9);
        acks.follow(
            &Update::MessageCreate {
                channel: 2,
                guild: Some(1),
                message: mine,
                ping: Default::default(),
            },
            9,
        );
        acks.follow(&Update::GuildRemove(1), 9);
        acks.follow(&marked(4), 9);
        acks.finished(flying[0].0, 0, Some(backoff(0)), now);
        assert!(acks.is_empty());
    }
}
