//! Everything that talks to Discord or the keyring, on its own thread.
//!
//! The interface sends [`Command`]s and reads the [`Event`]s it reports; it
//! never waits on the network. Each event asks the window for a repaint.

use crate::acks::{AckQueue, Delivery, backoff};
use crate::api::{self, Api};
use crate::credentials::{self, Token};
use crate::events::{Decoder, Update};
use crate::gateway::{self, End, Gateway};
use crate::model::{Ack, Id, Model, User};
use crate::remote_auth::{self, Progress};
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use std::cell::RefCell;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;

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
    /// Drop the ack waiting for a channel: marked unread elsewhere, or no
    /// longer mine to read.
    DropAck(Id),
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
    /// An ack is settled: saved, with the flags Discord now keeps, or
    /// dropped (`None`).
    AckDone {
        channel: Id,
        flags: Option<u32>,
    },
}

pub struct Backend {
    commands: UnboundedSender<Command>,
    events: mpsc::Receiver<Event>,
    thread: std::thread::JoinHandle<()>,
    /// Whether acks wait or are on their way, which closing waits for.
    acking: Arc<AtomicBool>,
}

impl Backend {
    pub fn start(ctx: egui::Context) -> Self {
        let (commands, receiver) = unbounded_channel();
        let (sender, events) = mpsc::channel();
        let acking = Arc::new(AtomicBool::new(false));
        let busy = acking.clone();
        let emit = move |event: Event| {
            if sender.send(event).is_ok() {
                ctx.request_repaint();
            }
        };
        let thread = std::thread::Builder::new()
            .name("backend".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("the backend's async runtime");
                let session = std::panic::AssertUnwindSafe(|| {
                    runtime.block_on(session(receiver, &emit, &busy));
                });
                // The panic itself is in the panic log; the window must not
                // keep waiting on a backend that is gone.
                if std::panic::catch_unwind(session).is_err() {
                    emit(Event::Session(Session::Stopped));
                }
            })
            .expect("the backend thread");
        Self {
            commands,
            events,
            thread,
            acking,
        }
    }

    /// Ends the backend as the window closes. Acks still waiting are sent
    /// first: closing the commands ends the session, which sends them, and
    /// the thread is given a little longer than that to finish.
    pub fn close(self) {
        let Self {
            commands,
            thread,
            acking,
            ..
        } = self;
        drop(commands);
        let deadline = std::time::Instant::now() + ACK_FLUSH + Duration::from_millis(500);
        while acking.load(Ordering::Relaxed)
            && !thread.is_finished()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
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
async fn session(mut commands: UnboundedReceiver<Command>, emit: Emit<'_>, busy: &AtomicBool) {
    let api = Api::new();
    let keyring = Keyring::start();
    loop {
        busy.store(false, Ordering::Relaxed);
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
        let acks = RefCell::new(AckQueue::default());
        let send = |ack, token| deliver(&api, ack, token);
        let ended = tokio::select! {
            ended = stay_connected(&api, &keyring, &token, &acks, emit) => Some(ended),
            served = serve(&mut commands, &api, &token, emit, &acks, busy, send) => match served {
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
    acks: &RefCell<AckQueue>,
    emit: Emit<'_>,
) -> Ended {
    let properties = api.client_properties().await;
    let mut me = 0;
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
                    me = model.me;
                    emit(Event::Ready(model));
                    return true;
                }
                match decoder.event(name, data) {
                    Ok(updates) => updates.into_iter().for_each(|update| {
                        // At once, not after a round trip through the window.
                        acks.borrow_mut().follow(&update, me);
                        emit(Event::Update(update));
                    }),
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
/// side by side; acks wait in `acks` and go out through `send`. Logging
/// out or closing sends what still waits, giving it [`ACK_FLUSH`].
async fn serve<S, F>(
    commands: &mut UnboundedReceiver<Command>,
    api: &Api,
    token: &RefCell<Token>,
    emit: Emit<'_>,
    acks: &RefCell<AckQueue>,
    busy: &AtomicBool,
    send: S,
) -> Served
where
    S: Fn(Ack, Token) -> F,
    F: Future<Output = (Delivery, Token)>,
{
    while commands.try_recv().is_ok() {}
    let mut loading = FuturesUnordered::new();
    let mut sending = FuturesUnordered::new();
    // Each request with the token in force when it leaves.
    let launch = |(ack, attempt): (Ack, u32)| {
        let flight = send(ack, Token::new(token.borrow().expose().to_owned()));
        async move {
            let (delivery, used) = flight.await;
            (ack, attempt, delivery, used)
        }
    };
    let served = loop {
        busy.store(!acks.borrow().is_empty(), Ordering::Relaxed);
        let due = acks.borrow().next_due();
        tokio::select! {
            command = commands.recv() => match command {
                None => break Served::Closed,
                Some(Command::LogOut) => break Served::LoggedOut,
                Some(Command::LoadHistory { channel, guild, before }) => {
                    let token = Token::new(token.borrow().expose().to_owned());
                    loading.push(async move {
                        load_history(api, &token, channel, guild, before, emit).await
                    });
                }
                Some(Command::Ack(ack)) => acks.borrow_mut().push(ack, Instant::now()),
                Some(Command::DropAck(channel)) => acks.borrow_mut().cancel(channel),
                Some(Command::Retry) => {}
            },
            Some(revoked) = loading.next(), if !loading.is_empty() => {
                if revoked {
                    return Served::Revoked;
                }
            }
            () = sleep_until(due), if due.is_some() => {
                let ready = acks.borrow_mut().take_due(Some(Instant::now()));
                sending.extend(ready.into_iter().map(&launch));
            }
            Some((ack, attempt, delivery, used)) = sending.next(), if !sending.is_empty() => {
                if landed(acks, token, emit, ack, attempt, delivery, &used) {
                    return Served::Revoked;
                }
            }
        }
    };
    let rest = acks.borrow_mut().take_due(None);
    sending.extend(rest.into_iter().map(&launch));
    let drain = async {
        while let Some((ack, attempt, delivery, used)) = sending.next().await {
            landed(acks, token, emit, ack, attempt, delivery, &used);
        }
    };
    let _ = tokio::time::timeout(ACK_FLUSH, drain).await;
    busy.store(false, Ordering::Relaxed);
    served
}

/// How long logging out or closing waits for the acks still to send.
const ACK_FLUSH: Duration = Duration::from_secs(2);

async fn sleep_until(due: Option<Instant>) {
    if let Some(due) = due {
        tokio::time::sleep_until(due).await;
    }
}

/// Sends one ack. The token comes back, to tell a 401 on a token since
/// rotated from one on the token in force.
async fn deliver(api: &Api, ack: Ack, token: Token) -> (Delivery, Token) {
    (api.ack(&token, &ack).await, token)
}

/// Takes an ack's answer: saved, it reports the flags Discord now keeps;
/// failed, it goes back in line. `true` when the session is over: a 401
/// on the token still in force (one rotated meanwhile tries again).
fn landed(
    acks: &RefCell<AckQueue>,
    token: &RefCell<Token>,
    emit: Emit<'_>,
    ack: Ack,
    attempt: u32,
    delivery: Delivery,
    used: &Token,
) -> bool {
    let retry = match delivery {
        Delivery::Saved => {
            emit(Event::AckDone {
                channel: ack.channel,
                flags: ack.flags,
            });
            None
        }
        Delivery::Dropped => {
            emit(Event::AckDone {
                channel: ack.channel,
                flags: None,
            });
            None
        }
        Delivery::Retry(after) => Some(after.unwrap_or_else(|| backoff(attempt))),
        Delivery::Unauthorized if used.expose() == token.borrow().expose() => return true,
        Delivery::Unauthorized => Some(Duration::ZERO),
    };
    acks.borrow_mut()
        .finished(ack, attempt, retry, Instant::now());
    false
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

    use crate::model::Ack;
    use std::cell::Cell;
    use std::sync::Mutex;

    fn ack(channel: Id, immediate: bool) -> Ack {
        Ack {
            guild: Some(1),
            channel,
            message: 10,
            flags: Some(1),
            immediate,
        }
    }

    fn paused<T>(test: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(test)
    }

    /// A signed-in phase whose acks get `answers`, in turn, after
    /// `commands` arrive; how it ended, the acks sent, and what it reported.
    async fn session_with(
        commands: Vec<Command>,
        answers: Vec<Delivery>,
        rotate: bool,
    ) -> (Served, usize, Vec<Event>) {
        let api = Api::new();
        let token = RefCell::new(Token::new("first".into()));
        let acks = RefCell::new(AckQueue::default());
        let busy = AtomicBool::new(false);
        let events = Mutex::new(Vec::new());
        let emit = |event: Event| events.lock().unwrap().push(event);
        let answers = RefCell::new(answers.into_iter());
        let sent = Cell::new(0);
        let send = |_: Ack, used: Token| {
            sent.set(sent.get() + 1);
            let answer = answers.borrow_mut().next().expect("an answer");
            let token = &token;
            async move {
                if rotate && used.expose() == "first" {
                    *token.borrow_mut() = Token::new("second".into());
                }
                (answer, used)
            }
        };
        let (sender, mut receiver) = unbounded_channel();
        let (served, ()) = tokio::join!(
            serve(&mut receiver, &api, &token, &emit, &acks, &busy, send),
            async {
                tokio::task::yield_now().await;
                for command in commands {
                    sender.send(command).unwrap();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        );
        (served, sent.get(), events.into_inner().unwrap())
    }

    #[test]
    fn a_401_on_the_token_in_force_ends_the_session() {
        let (served, sent, _) = paused(session_with(
            vec![Command::Ack(ack(5, true))],
            vec![Delivery::Unauthorized],
            false,
        ));
        assert!(matches!(served, Served::Revoked));
        assert_eq!(sent, 1);
    }

    #[test]
    fn a_401_on_a_token_since_rotated_tries_again() {
        let (served, sent, events) = paused(session_with(
            vec![
                Command::Ack(ack(5, true)),
                Command::Ack(ack(6, false)),
                Command::LogOut,
            ],
            vec![Delivery::Unauthorized, Delivery::Saved, Delivery::Saved],
            true,
        ));
        assert!(matches!(served, Served::LoggedOut));
        assert_eq!(sent, 3);
        // Saved, with their flags; #6 went out with the logout.
        assert!(events.contains(&Event::AckDone {
            channel: 5,
            flags: Some(1)
        }));
        assert!(events.contains(&Event::AckDone {
            channel: 6,
            flags: Some(1)
        }));
    }

    #[test]
    fn failed_acks_come_back_until_saved() {
        let (_, sent, events) = paused(session_with(
            vec![Command::Ack(ack(5, true)), Command::LogOut],
            vec![Delivery::Retry(None), Delivery::Saved],
            false,
        ));
        assert_eq!(sent, 2);
        assert!(events.contains(&Event::AckDone {
            channel: 5,
            flags: Some(1)
        }));
    }

    #[test]
    fn acks_die_with_the_session_before_they_are_due() {
        let sent = paused(async {
            let api = Api::new();
            let token = RefCell::new(Token::new("t".into()));
            let acks = RefCell::new(AckQueue::default());
            let busy = AtomicBool::new(false);
            let emit = |_: Event| {};
            let sent = Cell::new(0);
            let send = |_: Ack, used: Token| {
                sent.set(sent.get() + 1);
                async move { (Delivery::Saved, used) }
            };
            let (sender, mut receiver) = unbounded_channel();
            tokio::select! {
                _ = serve(&mut receiver, &api, &token, &emit, &acks, &busy, send) => {
                    panic!("still signed in");
                }
                () = async {
                    tokio::task::yield_now().await;
                    sender.send(Command::Ack(ack(5, false))).unwrap();
                    // The gateway ends the session within the delay.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                } => {}
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
            sent.get()
        });
        assert_eq!(sent, 0);
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
