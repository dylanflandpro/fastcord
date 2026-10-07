//! The messages on their way, in order, as the web client's message queue
//! keeps them: one in flight at a time, and when one fails, the messages
//! written after it in the same channel fail with it, unsent
//! (`cancelPendingSendRequests`), so none arrives before the one it
//! followed. The protocol without I/O: `backend::serve` drives it.

use crate::backend::Outgoing;
use crate::model::Id;
use std::collections::VecDeque;

#[derive(Debug, Default)]
pub struct Outbox {
    queue: VecDeque<Outgoing>,
    flying: bool,
}

impl Outbox {
    pub fn push(&mut self, outgoing: Outgoing) {
        self.queue.push_back(outgoing);
    }

    /// The next message to send, when none is on its way.
    pub fn next(&mut self) -> Option<Outgoing> {
        if self.flying {
            return None;
        }
        let next = self.queue.pop_front()?;
        self.flying = true;
        Some(next)
    }

    /// The one on its way goes again first (its token was replaced).
    pub fn again(&mut self, outgoing: Outgoing) {
        self.flying = false;
        self.queue.push_front(outgoing);
    }

    /// The one on its way in `channel` landed. When it `failed`, the nonces
    /// of the channel's messages queued after it, which now fail unsent.
    pub fn landed(&mut self, channel: Id, failed: bool) -> Vec<Id> {
        self.flying = false;
        if !failed {
            return Vec::new();
        }
        let (cancelled, kept) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|o| o.place.channel == channel);
        self.queue = kept;
        cancelled.into_iter().map(|o: Outgoing| o.nonce).collect()
    }

    /// Nothing waits and nothing is on its way.
    pub fn is_idle(&self) -> bool {
        !self.flying && self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Place;

    fn message(channel: Id, nonce: Id) -> Outgoing {
        Outgoing {
            place: Place {
                channel,
                guild: None,
            },
            nonce,
            content: String::new(),
        }
    }

    fn nonces(outbox: &mut Outbox) -> Vec<Id> {
        let mut sent = Vec::new();
        while let Some(next) = outbox.next() {
            sent.push(next.nonce);
            outbox.landed(next.place.channel, false);
        }
        sent
    }

    #[test]
    fn one_at_a_time_in_order() {
        let mut outbox = Outbox::default();
        assert!(outbox.is_idle());
        outbox.push(message(1, 10));
        outbox.push(message(2, 20));
        let first = outbox.next().unwrap();
        assert_eq!(first.nonce, 10);
        assert_eq!(outbox.next(), None, "one in flight");
        assert!(!outbox.is_idle());
        outbox.landed(1, false);
        assert_eq!(nonces(&mut outbox), [20]);
        assert!(outbox.is_idle());
    }

    #[test]
    fn a_failure_takes_its_channels_later_messages_with_it() {
        let mut outbox = Outbox::default();
        for (channel, nonce) in [(1, 10), (1, 11), (2, 20), (1, 12)] {
            outbox.push(message(channel, nonce));
        }
        let first = outbox.next().unwrap();
        assert_eq!(outbox.landed(first.place.channel, true), [11, 12]);
        assert_eq!(nonces(&mut outbox), [20]);
    }

    #[test]
    fn a_message_sent_again_goes_first() {
        let mut outbox = Outbox::default();
        outbox.push(message(1, 10));
        outbox.push(message(1, 11));
        let first = outbox.next().unwrap();
        outbox.again(first);
        assert_eq!(nonces(&mut outbox), [10, 11]);
    }
}
