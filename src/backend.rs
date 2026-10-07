//! Everything that talks to Discord or the keyring, on its own thread.
//!
//! The interface sends [`Command`]s and reads the [`Event`]s it reports; it
//! never waits on the network. Each event asks the window for a repaint.

use crate::api::{self, Api};
use crate::credentials::{self, Token};
use crate::events::{Decoder, Update};
use crate::gateway::{self, End, Gateway};
use crate::model::{Ack, Id, Model, User};
use crate::remote_auth::{self, Progress};
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

#[derive(Debug)]
pub enum Command {
    /// Try again after a failure.
    Retry,
    LogOut,
    /// Load a page of a channel's history: the latest, or the one before
    /// `before`.
    LoadHistory {
        channel: Id,
        /// The channel's guild, `None` for a DM: the page it is read from.
        guild: Option<Id>,
        before: Option<Id>,
    },
    /// Tell Discord a channel was read; the model already knows.
    Ack(Ack),
    /// Another device marked a channel unread: drop the ack waiting for it.
    KeepUnread(Id),
}

#[derive(Debug, PartialEq)]
pub enum Session {
    /// Reading the keyring and checking a stored session.
    Checking,
    /// Show this URL as a QR code.
    Qr(String),
    /// Someone scanned the code; the phone asks them to confirm.
    Scanned {
        username: String,
    },
    /// Discord asked for a captcha; its window is open.
    Captcha,
    SignedIn(User),
    /// The backend stopped after an internal error; only a restart helps.
    Stopped,
    /// Something went wrong; [`Command::Retry`] starts over.
    Failed(String),
}

/// The live connection's state, shown while signed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Connected,
    /// Lost; the next attempt is on its way.
    Reconnecting,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Session(Session),
    Link(Link),
    /// Everything at once, from READY.
    Ready(Model),
    Update(Update),
    /// A history page could not be loaded; the interface offers to retry.
    HistoryFailed {
        channel: Id,
    },
}

pub struct Backend {
    commands: UnboundedSender<Command>,
    events: mpsc::Receiver<Event>,
}

impl Backend {
    pub fn start(ctx: egui::Context) -> Self {
        let (commands, receiver) = unbounded_channel();
        let (sender, events) = mpsc::channel();
        let emit = move |event: Event| {
            if sender.send(event).is_ok() {
                ctx.request_repaint();
            }
        };
        std::thread::Builder::new()
            .name("backend".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("the backend's async runtime");
                let session = std::panic::AssertUnwindSafe(|| {
                    runtime.block_on(session(receiver, &emit));
                });
                // The panic itself is in the panic log; the window must not
                // keep waiting on a backend that is gone.
                if std::panic::catch_unwind(session).is_err() {
                    emit(Event::Session(Session::Stopped));
                }
            })
            .expect("the backend thread");
        Self { commands, events }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.try_iter()
    }
}

type Emit<'a> = &'a (dyn Fn(Event) + Send + Sync);

/// Restore or sign in, stay signed in until logged out, then start over.
/// Returns when the window is gone.
async fn session(mut commands: UnboundedReceiver<Command>, emit: Emit<'_>) {
    let api = Api::new();
    let keyring = Keyring::start();
    loop {
        emit(Event::Session(Session::Checking));
        let signed_in = match restore(&api, &keyring).await {
            Ok(Some(signed_in)) => Some(signed_in),
            Ok(None) => sign_in(&api, &keyring, emit).await,
            Err(message) => {
                emit(Event::Session(Session::Failed(message)));
                None
            }
        };
        let Some((token, user)) = signed_in else {
            if !wait_for(&mut commands, |c| matches!(c, Command::Retry)).await {
                return;
            }
            continue;
        };
        emit(Event::Session(Session::SignedIn(user)));
        // Replaced in place when Discord rotates it, so logging out uses the
        // token in force.
        let token = RefCell::new(token);
        let ended = tokio::select! {
            ended = stay_connected(&api, &keyring, &token, emit) => Some(ended),
            served = serve(&mut commands, &api, &token, emit) => match served {
                Served::Closed => return,
                Served::LoggedOut => None,
                // A history request found the token revoked.
                Served::Revoked => Some(Ended::Revoked),
            }
        };
        match ended {
            Some(Ended::Revoked) => {
                log::info!("Discord no longer accepts the session; signing in again");
                if let Err(error) = keyring.run(credentials::delete).await {
                    emit(Event::Session(Session::Failed(keyring_message(error))));
                    if !wait_for(&mut commands, |c| matches!(c, Command::Retry)).await {
                        return;
                    }
                }
                continue;
            }
            Some(ended @ (Ended::Refused(_) | Ended::Unreadable)) => {
                let message = match ended {
                    Ended::Refused(code) => format!("Discord refused the connection (code {code})."),
                    _ => "Discord sent account data this version cannot read. Try again, or update fastcord.".into(),
                };
                emit(Event::Session(Session::Failed(message)));
                if !wait_for(&mut commands, |c| matches!(c, Command::Retry)).await {
                    return;
                }
                continue;
            }
            None => {}
        }
        // Until the token is gone, trying again means logging out again,
        // never signing back in with what is left in the keyring.
        let token = token.into_inner();
        while let Err(error) = log_out(&api, &keyring, &token).await {
            emit(Event::Session(Session::Failed(error)));
            if !wait_for(&mut commands, |c| matches!(c, Command::Retry)).await {
                return;
            }
        }
    }
}

/// Why the live connection stopped for good.
#[derive(Debug, PartialEq)]
enum Ended {
    /// Discord no longer accepts the token.
    Revoked,
    /// Discord refuses this client (a close code retrying cannot fix).
    Refused(u16),
    /// READY could not be read: reconnecting would only get it again.
    Unreadable,
}

/// The first delay before reconnecting, and the longest: doubled after each
/// connection that failed before it was established.
const MIN_DELAY: Duration = Duration::from_secs(1);
const MAX_DELAY: Duration = Duration::from_secs(60);

fn next_delay(delay: Duration, established: bool) -> Duration {
    if established {
        MIN_DELAY
    } else {
        (delay * 2).min(MAX_DELAY)
    }
}

/// A serde error without the text it quotes: a message's content can be in
/// it, and logs never hold message content.
pub fn describe(error: &serde_json::Error) -> String {
    format!(
        "{:?} error at line {} column {}",
        error.classify(),
        error.line(),
        error.column()
    )
}

/// Keeps the gateway connected and reports what it delivers, reconnecting
/// with growing delays, until Discord ends the session for good. A token
/// Discord rotates replaces `token` at once and is queued for the keyring.
async fn stay_connected(
    api: &Api,
    keyring: &Keyring,
    token: &RefCell<Token>,
    emit: Emit<'_>,
) -> Ended {
    let properties = api.client_properties().await;
    let mut gateway = Gateway::default();
    let mut decoder = Decoder::default();
    let mut delay = MIN_DELAY;
    emit(Event::Link(Link::Connecting));
    loop {
        let mut established = false;
        let end = gateway::connect(
            &mut gateway,
            token,
            &properties,
            &mut |name, data| {
                let data = data.get();
                if name == "READY" {
                    let (model, rotated) = match decoder.ready(data) {
                        Ok(ready) => ready,
                        Err(error) => {
                            log::warn!("unreadable READY: {}", describe(&error));
                            return false;
                        }
                    };
                    if let Some(rotated) = rotated {
                        log::info!("Discord rotated the session token");
                        let saved = Token::new(rotated);
                        *token.borrow_mut() = Token::new(saved.expose().to_owned());
                        keyring.queue(move || {
                            if let Err(error) = credentials::save(&saved) {
                                log::warn!("the rotated token could not be saved: {error}");
                            }
                        });
                    }
                    emit(Event::Ready(model));
                    return true;
                }
                match decoder.event(name, data) {
                    Ok(updates) => updates
                        .into_iter()
                        .for_each(|update| emit(Event::Update(update))),
                    Err(error) => log::warn!("unreadable {name}: {}", describe(&error)),
                }
                true
            },
            &mut || {
                established = true;
                emit(Event::Link(Link::Connected));
            },
        )
        .await;
        match end {
            End::AuthenticationFailed => return Ended::Revoked,
            End::Refused(code) => return Ended::Refused(code),
            End::Unreadable => return Ended::Unreadable,
            End::Identify | End::Resume => gateway.ended(end),
        }
        delay = next_delay(delay, established);
        emit(Event::Link(Link::Reconnecting));
        log::info!("gateway reconnecting in {}s", delay.as_secs());
        tokio::time::sleep(delay).await;
    }
}

/// How the signed-in phase ended on the interface's side.
enum Served {
    LoggedOut,
    /// The window closed.
    Closed,
    /// Discord answered a request with 401: the token is gone.
    Revoked,
}

/// Answers the interface's requests while signed in. History pages load
/// side by side.
async fn serve(
    commands: &mut UnboundedReceiver<Command>,
    api: &Api,
    token: &RefCell<Token>,
    emit: Emit<'_>,
) -> Served {
    while commands.try_recv().is_ok() {}
    let mut loading = FuturesUnordered::new();
    let mut acks = AckQueue::default();
    let mut sending = FuturesUnordered::new();
    // `true` when Discord says the token is no longer valid.
    let send = |ack: Ack| {
        let token = Token::new(token.borrow().expose().to_owned());
        async move { matches!(api.ack(&token, &ack).await, Err(api::Error::Unauthorized)) }
    };
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => {
                    flush(&mut acks, &mut sending, send).await;
                    return Served::Closed;
                }
                Some(Command::LogOut) => {
                    flush(&mut acks, &mut sending, send).await;
                    return Served::LoggedOut;
                }
                Some(Command::Ack(ack)) => {
                    if let Some(now) = acks.push(ack, tokio::time::Instant::now()) {
                        sending.push(send(now));
                    }
                }
                Some(Command::KeepUnread(channel)) => acks.cancel(channel),
                Some(Command::LoadHistory { channel, guild, before }) => {
                    let token = Token::new(token.borrow().expose().to_owned());
                    loading.push(async move {
                        load_history(api, &token, channel, guild, before, emit).await
                    });
                }
                Some(Command::Retry) => {}
            },
            Some(revoked) = loading.next(), if !loading.is_empty() => {
                if revoked {
                    return Served::Revoked;
                }
            }
            () = sleep_until(acks.due), if acks.due.is_some() => {
                acks.take().into_iter().for_each(|ack| sending.push(send(ack)));
            }
            Some(revoked) = sending.next(), if !sending.is_empty() => {
                if revoked {
                    return Served::Revoked;
                }
            }
        }
    }
}

/// How long the web client holds an ack, so reading through a busy
/// channel sends one request rather than one per message.
const ACK_DELAY: Duration = Duration::from_secs(3);
/// How long logging out or closing waits for the acks still to send.
const ACK_FLUSH: Duration = Duration::from_secs(2);

/// Acks waiting out [`ACK_DELAY`], the newest per channel, as the web
/// client's outgoing ack timer keeps them.
#[derive(Default)]
struct AckQueue {
    pending: HashMap<Id, Ack>,
    /// When the pending acks go out: [`ACK_DELAY`] after the first.
    due: Option<tokio::time::Instant>,
}

impl AckQueue {
    /// Queues an ack, or returns it to send at once when it is immediate.
    /// A flags change an earlier one carried is kept.
    fn push(&mut self, ack: Ack, now: tokio::time::Instant) -> Option<Ack> {
        let flags = self.pending.remove(&ack.channel).and_then(|a| a.flags);
        let ack = Ack {
            flags: ack.flags.or(flags),
            ..ack
        };
        if ack.immediate {
            return Some(ack);
        }
        self.due.get_or_insert(now + ACK_DELAY);
        self.pending.insert(ack.channel, ack);
        None
    }

    fn cancel(&mut self, channel: Id) {
        self.pending.remove(&channel);
    }

    /// Everything pending, which empties the queue.
    fn take(&mut self) -> Vec<Ack> {
        self.due = None;
        self.pending.drain().map(|(_, ack)| ack).collect()
    }
}

async fn sleep_until(due: Option<tokio::time::Instant>) {
    if let Some(due) = due {
        tokio::time::sleep_until(due).await;
    }
}

/// Sends what is still pending and waits, briefly, for every ack in flight.
async fn flush<F: std::future::Future<Output = bool>>(
    acks: &mut AckQueue,
    sending: &mut FuturesUnordered<F>,
    send: impl Fn(Ack) -> F,
) {
    acks.take()
        .into_iter()
        .for_each(|ack| sending.push(send(ack)));
    let all = async { while sending.next().await.is_some() {} };
    let _ = tokio::time::timeout(ACK_FLUSH, all).await;
}

/// Loads one page and reports it. `true` when Discord says the token is no
/// longer valid.
async fn load_history(
    api: &Api,
    token: &Token,
    channel: Id,
    guild: Option<Id>,
    before: Option<Id>,
    emit: Emit<'_>,
) -> bool {
    let started = std::time::Instant::now();
    let page = match api.messages(token, channel, guild, before).await {
        Ok(body) => crate::events::history(channel, &body).map_err(|error| {
            log::warn!("unreadable history page: {}", describe(&error));
        }),
        // Nothing to read here for this account: an empty, complete history.
        Err(api::Error::Forbidden) => Ok(Update::History {
            channel,
            messages: Vec::new(),
            oldest: None,
            complete: true,
        }),
        Err(api::Error::Unauthorized) => {
            emit(Event::HistoryFailed { channel });
            return true;
        }
        Err(error) => {
            log::warn!("loading history failed: {error}");
            Err(())
        }
    };
    match page {
        Ok(update) => {
            if let Update::History { messages, .. } = &update {
                log::debug!(
                    "history for channel {channel}: {} messages in {} ms",
                    messages.len(),
                    started.elapsed().as_millis()
                );
            }
            emit(Event::Update(update));
        }
        Err(()) => emit(Event::HistoryFailed { channel }),
    }
    false
}

/// Waits for a command matching `wanted`, ignoring others. `false` once the
/// window has closed.
async fn wait_for(
    commands: &mut UnboundedReceiver<Command>,
    wanted: impl Fn(&Command) -> bool,
) -> bool {
    // Commands sent while the backend was busy answered an earlier screen (a
    // second click on Log out, say): acting on them now would log out the
    // next session.
    while commands.try_recv().is_ok() {}
    while let Some(command) = commands.recv().await {
        if wanted(&command) {
            return true;
        }
    }
    false
}

type Job = Box<dyn FnOnce() + Send>;

/// Every keyring call, on one thread, in the order they were asked for: a
/// rotated token's save and a later logout's delete can never swap, even
/// when the save was queued by a connection that has since gone.
struct Keyring {
    jobs: mpsc::Sender<Job>,
}

impl Keyring {
    fn start() -> Self {
        let (jobs, queue) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("keyring".into())
            .spawn(move || {
                for job in queue {
                    job();
                }
            })
            .expect("the keyring thread");
        Self { jobs }
    }

    /// Runs `work` after everything queued before it, and waits for it.
    async fn run<T: Send + 'static>(&self, work: impl FnOnce() -> T + Send + 'static) -> T {
        let (done, result) = tokio::sync::oneshot::channel();
        self.queue(move || {
            let _ = done.send(work());
        });
        result.await.expect("the keyring thread stopped")
    }

    /// Queues `work` without waiting for it.
    fn queue(&self, work: impl FnOnce() + Send + 'static) {
        let _ = self.jobs.send(Box::new(work));
    }
}

fn keyring_message(error: credentials::Error) -> String {
    match error {
        credentials::Error::Locked => {
            "The system keyring is locked. Unlock it, then try again.".into()
        }
        credentials::Error::Unavailable => {
            "No system keyring is available to keep the session.".into()
        }
    }
}

/// The stored session, when there is one and Discord still accepts it. A
/// token Discord refuses is removed.
async fn restore(api: &Api, keyring: &Keyring) -> Result<Option<(Token, User)>, String> {
    let Some(token) = keyring
        .run(credentials::load)
        .await
        .map_err(keyring_message)?
    else {
        return Ok(None);
    };
    match api.me(&token).await {
        Ok(user) => Ok(Some((token, user))),
        Err(api::Error::Unauthorized) => {
            log::info!("the stored session was revoked; signing in again");
            keyring
                .run(credentials::delete)
                .await
                .map_err(keyring_message)?;
            Ok(None)
        }
        Err(error) => Err(format!("Unable to check the session: {error}.")),
    }
}

/// QR codes until one is scanned and approved. An expired or cancelled code
/// is replaced at once, as the official client does.
async fn sign_in(api: &Api, keyring: &Keyring, emit: Emit<'_>) -> Option<(Token, User)> {
    let progress = |progress: Progress| match progress {
        Progress::Qr(url) => emit(Event::Session(Session::Qr(url))),
        Progress::Scanned(user) => emit(Event::Session(Session::Scanned {
            username: user.username,
        })),
        Progress::Captcha => emit(Event::Session(Session::Captcha)),
    };
    let token = loop {
        match remote_auth::run(api, &progress).await {
            Ok(token) => break token,
            Err(remote_auth::Error::Expired | remote_auth::Error::Cancelled) => {
                log::info!("QR session ended; showing a new code");
            }
            Err(error) => {
                log::warn!("QR sign-in failed: {error}");
                emit(Event::Session(Session::Failed(format!(
                    "Sign-in failed: {error}."
                ))));
                return None;
            }
        }
    };
    let token = match keyring
        .run(move || credentials::save(&token).map(|()| token))
        .await
    {
        Ok(token) => token,
        Err(error) => {
            emit(Event::Session(Session::Failed(keyring_message(error))));
            return None;
        }
    };
    match api.me(&token).await {
        Ok(user) => Some((token, user)),
        Err(error) => {
            emit(Event::Session(Session::Failed(format!(
                "Unable to read the account: {error}."
            ))));
            None
        }
    }
}

/// Ends the session on Discord's side, then forgets the token. A failed
/// remote logout still removes it locally.
async fn log_out(api: &Api, keyring: &Keyring, token: &Token) -> Result<(), String> {
    if let Err(error) = api.logout(token).await {
        log::warn!("remote logout failed: {error}");
    }
    keyring.run(credentials::delete).await.map_err(|error| {
        format!(
            "The session could not be removed: {}",
            keyring_message(error)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_delays_double_until_a_connection_holds() {
        let mut delay = MIN_DELAY;
        for expected in [2, 4, 8, 16, 32, 60, 60] {
            delay = next_delay(delay, false);
            assert_eq!(delay, Duration::from_secs(expected));
        }
        assert_eq!(next_delay(delay, true), MIN_DELAY);
    }

    #[test]
    fn parse_errors_are_described_without_their_text() {
        let error = serde_json::from_str::<u64>(r#""a private message""#).unwrap_err();
        let described = describe(&error);
        assert!(!described.contains("private"), "{described}");
        assert!(described.contains("line 1"));
    }

    fn ack(channel: Id, message: Id, flags: Option<u32>, immediate: bool) -> Ack {
        Ack {
            guild: None,
            channel,
            message,
            flags,
            immediate,
        }
    }

    #[test]
    fn acks_wait_and_keep_the_newest_per_channel() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                let start = tokio::time::Instant::now();
                let mut acks = AckQueue::default();
                assert_eq!(acks.push(ack(1, 10, Some(1), false), start), None);
                tokio::time::advance(Duration::from_secs(2)).await;
                let later = tokio::time::Instant::now();
                assert_eq!(acks.push(ack(1, 11, None, false), later), None);
                assert_eq!(acks.push(ack(2, 20, None, false), later), None);
                // Due three seconds after the first, not the last.
                assert_eq!(acks.due, Some(start + ACK_DELAY));
                let before = tokio::time::Instant::now();
                sleep_until(acks.due).await;
                assert_eq!(before.elapsed(), Duration::from_secs(1));
                let mut sent = acks.take();
                sent.sort_by_key(|a| a.channel);
                assert_eq!(sent, [ack(1, 11, Some(1), false), ack(2, 20, None, false)]);
                assert_eq!(acks.due, None);
            });
    }

    #[test]
    fn mentions_ack_at_once_and_unread_marks_cancel() {
        let now = tokio::time::Instant::now();
        let mut acks = AckQueue::default();
        acks.push(ack(1, 10, Some(1), false), now);
        // The channel got mentions: the newest ack goes now, flags kept.
        assert_eq!(
            acks.push(ack(1, 12, None, true), now),
            Some(ack(1, 12, Some(1), true))
        );
        acks.push(ack(2, 20, None, false), now);
        acks.cancel(2);
        assert!(acks.take().is_empty());
    }

    #[test]
    fn commands_from_an_earlier_screen_are_ignored() {
        let (commands, mut receiver) = unbounded_channel();
        commands.send(Command::LogOut).unwrap();
        commands.send(Command::Retry).unwrap();
        drop(commands);
        let waited = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(wait_for(&mut receiver, |c| matches!(c, Command::LogOut)));
        // The queued LogOut was stale; the channel then closed.
        assert!(!waited);
    }
}
