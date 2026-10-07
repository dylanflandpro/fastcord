//! My messages, edits and deletions on their way, in order, as the web
//! client's message queue keeps them: one in flight at a time, and when a
//! message fails, the messages written after it in the same channel fail
//! with it, unsent (`cancelPendingSendRequests`), so none arrives before the
//! one it followed. The protocol without I/O: `backend::serve` drives it.

use crate::backend::Write;
use crate::model::Id;
use std::collections::VecDeque;

#[derive(Debug, Default)]
pub struct Outbox {
    queue: VecDeque<Write>,
    flying: bool,
}

impl Outbox {
    pub fn push(&mut self, outgoing: Write) {
        self.queue.push_back(outgoing);
    }

    /// The next message to send, when none is on its way.
    pub fn next(&mut self) -> Option<Write> {
        if self.flying {
            return None;
        }
        let next = self.queue.pop_front()?;
        self.flying = true;
        Some(next)
    }

    /// The one on its way goes again first (its token was replaced).
    pub fn again(&mut self, outgoing: Write) {
        self.flying = false;
        self.queue.push_front(outgoing);
    }

    /// The one on its way in `channel` landed. When it was a message that
    /// `failed`, the nonces of the channel's messages queued after it, which
    /// now fail unsent; edits and deletions stay.
    pub fn landed(&mut self, channel: Id, failed: bool) -> Vec<Id> {
        self.flying = false;
        if !failed {
            return Vec::new();
        }
        let mut cancelled = Vec::new();
        self.queue.retain(|write| match write {
            Write::Send(o) if o.place.channel == channel => {
                cancelled.push(o.nonce);
                false
            }
            _ => true,
        });
        cancelled
    }

    /// How many messages wait or are on their way.
    pub fn pending(&self) -> usize {
        self.queue.len() + usize::from(self.flying)
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

    fn message(channel: Id, nonce: Id) -> Write {
        let place = Place {
            channel,
            guild: None,
        };
        Write::Send(crate::backend::Outgoing {
            place,
            nonce,
            content: String::new(),
            reply: None,
        })
    }

    fn ids(outbox: &mut Outbox) -> Vec<Id> {
        let mut sent = Vec::new();
        while let Some(next) = outbox.next() {
            sent.push(match &next {
                Write::Send(o) => o.nonce,
                Write::Edit { id, .. } | Write::Delete { id, .. } => *id,
            });
            outbox.landed(next.place().channel, false);
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
        assert_eq!(first, message(1, 10));
        assert_eq!(outbox.next(), None, "one in flight");
        assert!(!outbox.is_idle());
        outbox.landed(1, false);
        assert_eq!(ids(&mut outbox), [20]);
        assert!(outbox.is_idle());
    }

    #[test]
    fn a_failure_takes_its_channels_later_messages_with_it() {
        let mut outbox = Outbox::default();
        for (channel, nonce) in [(1, 10), (1, 11), (2, 20), (1, 12)] {
            outbox.push(message(channel, nonce));
        }
        let place = Place {
            channel: 1,
            guild: None,
        };
        outbox.push(Write::Delete { place, id: 5 });
        let first = outbox.next().unwrap();
        assert_eq!(outbox.landed(first.place().channel, true), [11, 12]);
        assert_eq!(ids(&mut outbox), [20, 5], "a deletion is no message");
    }

    #[test]
    fn a_message_sent_again_goes_first() {
        let mut outbox = Outbox::default();
        outbox.push(message(1, 10));
        outbox.push(message(1, 11));
        let first = outbox.next().unwrap();
        outbox.again(first);
        assert_eq!(ids(&mut outbox), [10, 11]);
    }
}
