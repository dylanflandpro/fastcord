//! Everything that talks to Discord or the keyring, on its own thread.
//!
//! The interface sends [`Command`]s and reads the [`Event`]s it reports; it
//! never waits on the network. Each event asks the window for a repaint.

use crate::acks::{AckQueue, Delivery, backoff};
use crate::api::{self, Api, Attempt, Place, Verdict};
use crate::credentials::{self, Token};
use crate::events::{Decoder, Update};
use crate::gateway::{self, End, Gateway};
use crate::model::{Ack, Id, Model, ReactionRequest, User};
use crate::notify::{self, Notice, Notifications};
use crate::outbox::Outbox;
use crate::remote_auth::{self, Progress};
use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// Add or remove my reaction; the model already shows it. Writes to the
    /// account.
    React(ReactionRequest),
    /// Post a message I wrote. Messages leave one at a time, in the order
    /// they were written, as the official client's queue sends them.
    Write(Write),
}

/// A change I make on Discord. Writes leave one at a time, in the order
/// they were made, as the official client's queue sends messages and edits.
#[derive(Clone, Debug, PartialEq)]
pub enum Write {
    Send(Outgoing),
    /// My message's new text.
    Edit {
        place: Place,
        id: Id,
        content: String,
    },
    Delete {
        place: Place,
        id: Id,
    },
}

impl Write {
    pub fn place(&self) -> Place {
        match self {
            Write::Send(outgoing) => outgoing.place,
            Write::Edit { place, .. } | Write::Delete { place, .. } => *place,
        }
    }
}

/// What an edit or a deletion of mine was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Change {
    Edit,
    Delete,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Outgoing {
    pub place: Place,
    pub nonce: Id,
    pub content: String,
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
    Ready(Box<Model>),
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
    /// A notification for this channel was clicked.
    Open(Id),
    /// Discord did not take a reaction: the interface undoes it.
    ReactionFailed(ReactionRequest),
    /// The message sent with `nonce` did not go through; `reason` is
    /// Discord's, when it gave one.
    SendFailed {
        channel: Id,
        nonce: Id,
        reason: Option<String>,
    },
    /// The gateway answered a heartbeat: what it had to deliver until now,
    /// it has.
    Alive,
    /// The answer to the message sent with `nonce` was lost: it may have
    /// arrived, which the gateway will tell.
    SendUnsure {
        channel: Id,
        nonce: Id,
    },
    /// An edit or a deletion of mine came back: done, or not, with
    /// Discord's reason when it gave one.
    Changed {
        channel: Id,
        id: Id,
        change: Change,
        result: Result<(), Option<String>>,
    },
    /// Discord asked to slow down: the message goes in `wait`.
    SendHeld {
        channel: Id,
        nonce: Id,
        wait: Duration,
    },
}

pub struct Backend {
    commands: UnboundedSender<Command>,
    events: mpsc::Receiver<Event>,
    thread: std::thread::JoinHandle<()>,
    /// What closing waits for, a little while.
    acking: Arc<Pending>,
    notifier: notify::Notifier,
}

impl Backend {
    /// `shared` is what the window tells notifications about itself.
    pub fn start(ctx: egui::Context, shared: Arc<notify::Shared>) -> Self {
        let (commands, receiver) = unbounded_channel();
        let (sender, events) = mpsc::channel();
        let acking = Arc::new(Pending::default());
        let busy = acking.clone();
        let opened = {
            let (sender, ctx) = (sender.clone(), ctx.clone());
            move |channel| {
                if sender.send(Event::Open(channel)).is_ok() {
                    ctx.request_repaint();
                }
            }
        };
        let notifier = notify::desktop(opened);
        let desktop = notifier.clone();
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
                let notice = |notice| desktop.send(notice);
                let alerts = Alerts {
                    shared,
                    notify: &notice,
                };
                let session = std::panic::AssertUnwindSafe(|| {
                    runtime.block_on(session(receiver, &emit, &busy, &alerts));
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
            notifier,
        }
    }

    /// Tells the desktop's notifier at once, without the network thread.
    pub fn notify(&self, notice: Notice) {
        self.notifier.send(notice);
    }

    /// Ends the backend as the window closes. Acks and messages still
    /// waiting are sent first, for [`ACK_FLUSH`] at most: closing the
    /// commands ends the session, which sends them, and the thread is given
    /// a little longer than that to finish. What is left then is lost.
    pub fn close(self) {
        let Self {
            commands,
            thread,
            acking,
            ..
        } = self;
        drop(commands);
        let deadline = std::time::Instant::now() + ACK_FLUSH + Duration::from_millis(500);
        while !acking.idle() && !thread.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn send(&self, command: Command) {
        // Closing waits for a message from the moment it is handed over.
        if matches!(command, Command::Write(Write::Send(_))) {
            self.acking.handed.fetch_add(1, Ordering::Relaxed);
        }
        let _ = self.commands.send(command);
    }

    pub fn events(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.try_iter()
    }
}

type Emit<'a> = &'a (dyn Fn(Event) + Send + Sync);

/// Where notifications come from and go: what the window shares, and the
/// notifier (the desktop's, or a fake in tests).
struct Alerts<'a> {
    shared: Arc<notify::Shared>,
    notify: &'a dyn Fn(Notice),
}

/// Restore or sign in, stay signed in until logged out, then start over.
/// Returns when the window is gone.
/// What is still owed to Discord, shared with the window, which waits for
/// it before closing.
#[derive(Debug, Default)]
pub struct Pending {
    /// Acks or messages waiting or on their way.
    working: AtomicBool,
    /// Messages handed to the backend and not yet taken in.
    handed: AtomicUsize,
    /// Messages waiting or on their way, as last counted: what is lost if
    /// the session ends under them.
    unsent: AtomicUsize,
}

impl Pending {
    fn idle(&self) -> bool {
        !self.working.load(Ordering::Relaxed) && self.handed.load(Ordering::Relaxed) == 0
    }

    fn taken(&self) {
        let _ = self
            .handed
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }
}

async fn session(
    mut commands: UnboundedReceiver<Command>,
    emit: Emit<'_>,
    busy: &Pending,
    alerts: &Alerts<'_>,
) {
    let api = Api::new();
    let keyring = Keyring::start();
    loop {
        busy.working.store(false, Ordering::Relaxed);
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
        let connection = Connection {
            acks: &acks,
            emit,
            notifications: RefCell::new(Notifications::new(alerts.shared.clone())),
            notify: alerts.notify,
        };
        let post = |write: Write, token: Token| {
            let api = &api;
            async move {
                let attempt = match &write {
                    Write::Send(Outgoing {
                        place,
                        nonce,
                        content,
                    }) => api.send_message(&token, *place, *nonce, content).await,
                    Write::Edit { place, id, content } => {
                        api.edit_message(&token, *place, *id, content).await
                    }
                    Write::Delete { place, id } => api.delete_message(&token, *place, *id).await,
                };
                (attempt, token)
            }
        };
        let ended = tokio::select! {
            ended = stay_connected(&api, &keyring, &token, &connection) => Some(ended),
            served = serve(&mut commands, &api, &token, emit, &acks, busy, send, post) => match served {
                Served::Closed => {
                    // The window is gone: so are the notifications it
                    // would open.
                    (alerts.notify)(Notice::ClearAll);
                    return;
                }
                Served::LoggedOut => None,
                // A history request found the token revoked.
                Served::Revoked => Some(Ended::Revoked),
            }
        };
        // Signed out, or about to be: nothing of the account stays on the
        // desktop, and its notifications no longer open anything.
        if signs_out(ended.as_ref()) {
            (alerts.notify)(Notice::ClearAll);
        }
        match ended {
            Some(Ended::Revoked) => {
                log::info!("Discord no longer accepts the session; signing in again");
                if let Some(lost) = unsent_lost(busy) {
                    log::warn!("{lost}");
                }
                if let Err(error) = keyring.run(credentials::delete).await {
                    emit(Event::Session(Session::Failed(keyring_message(error))));
                    if !wait_for(&mut commands, |c| matches!(c, Command::Retry)).await {
                        return;
                    }
                }
                continue;
            }
            Some(ended @ (Ended::Refused(_) | Ended::Unreadable)) => {
                let mut message = match ended {
                    Ended::Refused(code) => format!("Discord refused the connection (code {code})."),
                    _ => "Discord sent account data this version cannot read. Try again, or update fastcord.".into(),
                };
                if let Some(lost) = unsent_lost(busy) {
                    message.push(' ');
                    message.push_str(&lost);
                }
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

/// Whether a signed-in phase that ended so leaves the account: logged out
/// (`None`) or revoked, rather than failed and waiting for Retry.
fn signs_out(ended: Option<&Ended>) -> bool {
    matches!(ended, None | Some(Ended::Revoked))
}

/// The messages the session took with it when it ended under them, said.
fn unsent_lost(busy: &Pending) -> Option<String> {
    let lost = busy.unsent.swap(0, Ordering::Relaxed);
    busy.working.store(false, Ordering::Relaxed);
    match lost {
        0 => None,
        1 => Some("A message could not be sent.".into()),
        n => Some(format!("{n} messages could not be sent.")),
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

/// Where the gateway's events go: the window, the acks waiting, and
/// notifications.
struct Connection<'a> {
    acks: &'a RefCell<AckQueue>,
    emit: Emit<'a>,
    notifications: RefCell<Notifications>,
    notify: &'a dyn Fn(Notice),
}

impl Connection<'_> {
    fn ready(&self, model: Model) {
        self.notifications.borrow_mut().ready(&model);
        (self.emit)(Event::Ready(Box::new(model)));
    }

    fn update(&self, update: Update, me: Id) {
        // At once, not after a round trip through the window.
        self.acks.borrow_mut().follow(&update, me);
        let notice = self
            .notifications
            .borrow_mut()
            .follow(&update, std::time::Instant::now());
        if let Some(notice) = notice {
            (self.notify)(notice);
        }
        (self.emit)(Event::Update(update));
    }
}

/// Keeps the gateway connected and reports what it delivers, reconnecting
/// with growing delays, until Discord ends the session for good. A token
/// Discord rotates replaces `token` at once and is queued for the keyring.
async fn stay_connected(
    api: &Api,
    keyring: &Keyring,
    token: &RefCell<Token>,
    connection: &Connection<'_>,
) -> Ended {
    let emit = connection.emit;
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
                    connection.ready(model);
                    return true;
                }
                match decoder.event(name, data) {
                    Ok(updates) => updates
                        .into_iter()
                        .for_each(|update| connection.update(update, me)),
                    Err(error) => log::warn!("unreadable {name}: {}", describe(&error)),
                }
                true
            },
            &mut || {
                established = true;
                emit(Event::Link(Link::Connected));
            },
            &mut || emit(Event::Alive),
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
/// side by side; acks wait in `acks` and go out through `send`; messages
/// wait in an [`Outbox`] and go out through `post`, one at a time. Logging
/// out or closing sends the acks still waiting, and closing the messages
/// too, giving them [`ACK_FLUSH`]; logging out drops the messages with the
/// account.
#[allow(clippy::too_many_arguments)]
async fn serve<S, F, P, G>(
    commands: &mut UnboundedReceiver<Command>,
    api: &Api,
    token: &RefCell<Token>,
    emit: Emit<'_>,
    acks: &RefCell<AckQueue>,
    busy: &Pending,
    send: S,
    post: P,
) -> Served
where
    S: Fn(Ack, Token) -> F,
    F: Future<Output = (Delivery, Token)>,
    P: Fn(Write, Token) -> G,
    G: Future<Output = (Attempt, Token)>,
{
    // Left from an earlier screen: dropped (see `wait_for`).
    while let Ok(command) = commands.try_recv() {
        if matches!(command, Command::Write(Write::Send(_))) {
            busy.taken();
        }
    }
    let mut loading = FuturesUnordered::new();
    let mut sending = FuturesUnordered::new();
    let mut reactions = Reactions::default();
    let send_reaction = |reaction: ReactionRequest, used: Token| async move {
        (api.react(&used, &reaction).await, used)
    };
    let fly = |reaction: ReactionRequest| {
        let send = &send_reaction;
        async move {
            let outcome = react(send, token, &reaction).await;
            (reaction, outcome)
        }
    };
    let mut reacting = FuturesUnordered::new();
    // Each request with the token in force when it leaves.
    let launch = |(ack, attempt): (Ack, u32)| {
        let flight = send(ack, Token::new(token.borrow().expose().to_owned()));
        async move {
            let (delivery, used) = flight.await;
            (ack, attempt, delivery, used)
        }
    };
    let mut outbox = Outbox::default();
    let mut writing = FuturesUnordered::new();
    let served = loop {
        if let Some(write) = outbox.next() {
            writing.push(send_one(&post, token, emit, write));
        }
        let working = !acks.borrow().is_empty() || !outbox.is_idle();
        busy.working.store(working, Ordering::Relaxed);
        busy.unsent.store(outbox.pending(), Ordering::Relaxed);
        let due = acks.borrow().next_due();
        tokio::select! {
            command = commands.recv() => match command {
                None => break Served::Closed,
                Some(Command::LogOut) => break Served::LoggedOut,
                Some(Command::LoadHistory { channel, guild, before }) => {
                    let used = Token::new(token.borrow().expose().to_owned());
                    loading.push(async move {
                        let revoked = load_history(api, &used, channel, guild, before, emit).await;
                        (revoked, used)
                    });
                }
                Some(Command::Ack(ack)) => acks.borrow_mut().push(ack, Instant::now()),
                Some(Command::DropAck(channel)) => acks.borrow_mut().cancel(channel),
                Some(Command::React(reaction)) => {
                    reacting.extend(reactions.request(reaction).map(&fly));
                }
                Some(Command::Write(write)) => {
                    if matches!(write, Write::Send(_)) {
                        busy.taken();
                    }
                    outbox.push(write);
                }
                Some(Command::Retry) => {}
            },
            Some((write, verdict, used)) = writing.next(), if !writing.is_empty() => {
                if posted(&mut outbox, acks, token, emit, write, verdict, &used) {
                    break Served::Revoked;
                }
            }
            // Only the token in force: one rotated meanwhile is fine.
            Some((revoked, used)) = loading.next(), if !loading.is_empty() => {
                if revoked && used.expose() == token.borrow().expose() {
                    break Served::Revoked;
                }
            }
            Some((reaction, outcome)) = reacting.next(), if !reacting.is_empty() => {
                match settle(&mut reactions, reaction, outcome, emit) {
                    Settled::Revoked => break Served::Revoked,
                    Settled::Next(next) => reacting.extend(next.map(&fly)),
                }
            }
            () = sleep_until(due), if due.is_some() => {
                let ready = acks.borrow_mut().take_due(Some(Instant::now()));
                sending.extend(ready.into_iter().map(&launch));
            }
            Some((ack, attempt, delivery, used)) = sending.next(), if !sending.is_empty() => {
                if landed(acks, token, emit, ack, attempt, delivery, &used) {
                    break Served::Revoked;
                }
            }
        }
    };
    if matches!(served, Served::Revoked) {
        // Nothing more goes out on a token Discord refused.
        busy.working.store(false, Ordering::Relaxed);
        return served;
    }
    let rest = acks.borrow_mut().take_due(None);
    sending.extend(rest.into_iter().map(&launch));
    let drain = async {
        while let Some((ack, attempt, delivery, used)) = sending.next().await {
            landed(acks, token, emit, ack, attempt, delivery, &used);
        }
    };
    let closing = matches!(served, Served::Closed);
    let flush = async {
        if !closing {
            return;
        }
        loop {
            if let Some(write) = outbox.next() {
                writing.push(send_one(&post, token, emit, write));
            }
            let Some((write, verdict, used)) = writing.next().await else {
                break;
            };
            posted(&mut outbox, acks, token, emit, write, verdict, &used);
        }
    };
    let _ = tokio::time::timeout(ACK_FLUSH, async { tokio::join!(drain, flush) }).await;
    busy.working.store(false, Ordering::Relaxed);
    busy.unsent.store(outbox.pending(), Ordering::Relaxed);
    served
}

/// How long logging out or closing waits for the acks (and, closing, the
/// messages) still to send.
const ACK_FLUSH: Duration = Duration::from_secs(2);

/// One write, made with the token in force at each try, rate limits
/// waited out in full (for a message, the window says Discord asked to
/// slow down); what its last try came to.
async fn send_one<P, G>(
    post: &P,
    token: &RefCell<Token>,
    emit: Emit<'_>,
    write: Write,
) -> (Write, Verdict, Token)
where
    P: Fn(Write, Token) -> G,
    G: Future<Output = (Attempt, Token)>,
{
    let mut retries = 0;
    loop {
        let used = Token::new(token.borrow().expose().to_owned());
        let (attempt, used) = post(write.clone(), used).await;
        match api::verdict(&attempt, retries) {
            Verdict::Wait(wait) => {
                retries += 1;
                log::info!("rate limited; trying again in {} ms", wait.as_millis());
                if let Write::Send(Outgoing { place, nonce, .. }) = &write {
                    let (channel, nonce) = (place.channel, *nonce);
                    emit(Event::SendHeld {
                        channel,
                        nonce,
                        wait,
                    });
                }
                tokio::time::sleep(wait).await;
            }
            verdict => return (write, verdict, used),
        }
    }
}

/// Reports what a write came to. A failed message takes the channel's
/// messages queued after it along, unsent. `true` when Discord refused the
/// token in force; a token rotated meanwhile makes the write again.
fn posted(
    outbox: &mut Outbox,
    acks: &RefCell<AckQueue>,
    token: &RefCell<Token>,
    emit: Emit<'_>,
    write: Write,
    verdict: Verdict,
    used: &Token,
) -> bool {
    let Place { channel, guild } = write.place();
    let refused = match verdict {
        Verdict::Unauthorized if used.expose() == token.borrow().expose() => return true,
        Verdict::Unauthorized => {
            outbox.again(write);
            return false;
        }
        Verdict::Sent(body) => {
            outbox.landed(channel, false);
            done(acks, emit, &write, guild, body.as_deref());
            return false;
        }
        // Lost: if it went through, the gateway says so.
        Verdict::Unsure => None,
        Verdict::Refused(reason) => Some(reason),
        Verdict::Wait(_) => unreachable!("waited out in `send_one`"),
    };
    let (id, change) = match write {
        Write::Send(Outgoing { nonce, .. }) => {
            emit(match refused {
                Some(reason) => Event::SendFailed {
                    channel,
                    nonce,
                    reason,
                },
                None => {
                    log::warn!("a message's answer was lost; waiting for the gateway");
                    Event::SendUnsure { channel, nonce }
                }
            });
            for later in outbox.landed(channel, true) {
                let reason = None;
                emit(Event::SendFailed {
                    channel,
                    nonce: later,
                    reason,
                });
            }
            return false;
        }
        Write::Edit { id, .. } => (id, Change::Edit),
        Write::Delete { id, .. } => (id, Change::Delete),
    };
    outbox.landed(channel, false);
    let result = Err(refused.flatten());
    emit(Event::Changed {
        channel,
        id,
        change,
        result,
    });
    false
}

/// A write Discord did, reported with the answer's message when it could
/// be read (else the gateway's copy confirms it).
fn done(
    acks: &RefCell<AckQueue>,
    emit: Emit<'_>,
    write: &Write,
    guild: Option<Id>,
    body: Option<&str>,
) {
    let channel = write.place().channel;
    let read = |update: serde_json::Result<Update>| match update {
        Ok(update) => emit(Event::Update(update)),
        Err(error) => log::warn!("unreadable answer: {}", describe(&error)),
    };
    let (id, change) = match write {
        Write::Send(_) => {
            // My message reads the channel: no ack is owed for it.
            acks.borrow_mut().cancel(channel);
            match body {
                Some(body) => read(crate::events::sent(body, guild)),
                None => log::warn!("a sent message's answer could not be read"),
            }
            return;
        }
        Write::Edit { id, .. } => {
            if let Some(body) = body {
                read(crate::events::edited(body));
            }
            (*id, Change::Edit)
        }
        Write::Delete { id, .. } => {
            let ids = vec![*id];
            emit(Event::Update(Update::MessageDelete { channel, ids }));
            (*id, Change::Delete)
        }
    };
    let result = Ok(());
    emit(Event::Changed {
        channel,
        id,
        change,
        result,
    });
}

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

/// What to do after a reaction request, as the web client does: a rate
/// limit is waited out once, another failure is tried again once at once;
/// a refusal, or a second failure, undoes the reaction. A 401 on a token
/// rotated since tries once more with the new one, whatever was tried
/// before; on the token in force, the session is over.
#[derive(Debug, PartialEq)]
enum Next {
    Done,
    Retry(Duration),
    /// Again at once with the token now in force.
    Rotated,
    Undo,
    Revoked,
}

/// `rotated`: the token used is no longer the one in force, and no try
/// was made again for that yet.
fn after_reaction(reacted: api::Reacted, retried: bool, rotated: bool) -> Next {
    match reacted {
        api::Reacted::Done => Next::Done,
        api::Reacted::Unauthorized if rotated => Next::Rotated,
        api::Reacted::Unauthorized => Next::Revoked,
        api::Reacted::RateLimited(wait) if !retried => Next::Retry(wait),
        api::Reacted::Failed if !retried => Next::Retry(Duration::ZERO),
        api::Reacted::RateLimited(_) | api::Reacted::Failed | api::Reacted::Refused => Next::Undo,
    }
}

/// How a reaction request ended.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Taken,
    Refused,
    Revoked,
}

/// Sends one reaction, with the token in force at each try.
async fn react<S, F>(send: &S, token: &RefCell<Token>, reaction: &ReactionRequest) -> Outcome
where
    S: Fn(ReactionRequest, Token) -> F,
    F: Future<Output = (api::Reacted, Token)>,
{
    let (mut retried, mut retried_rotated) = (false, false);
    loop {
        let current = || Token::new(token.borrow().expose().to_owned());
        let (reacted, used) = send(reaction.clone(), current()).await;
        let rotated = !retried_rotated && used.expose() != token.borrow().expose();
        match after_reaction(reacted, retried, rotated) {
            Next::Done => return Outcome::Taken,
            Next::Rotated => retried_rotated = true,
            Next::Retry(wait) => {
                retried = true;
                tokio::time::sleep(wait).await;
            }
            Next::Undo => return Outcome::Refused,
            Next::Revoked => return Outcome::Revoked,
        }
    }
}

/// One message's reaction with one emoji.
type ReactionKey = (Id, Option<Id>, String);

fn reaction_key(reaction: &ReactionRequest) -> ReactionKey {
    let name = match reaction.emoji.id {
        Some(_) => String::new(),
        None => reaction.emoji.name.clone(),
    };
    (reaction.message, reaction.emoji.id, name)
}

/// Reactions on their way: one request at a time per message and emoji, so
/// Discord gets them in the order they were clicked, and only the last
/// click made while one flies.
#[derive(Debug, Default)]
struct Reactions {
    /// Per reaction in flight, the click waiting behind it.
    flying: HashMap<ReactionKey, Option<ReactionRequest>>,
}

impl Reactions {
    /// The request to send now, or `None` when it waits for the one flying.
    fn request(&mut self, reaction: ReactionRequest) -> Option<ReactionRequest> {
        match self.flying.get_mut(&reaction_key(&reaction)) {
            Some(waiting) => {
                *waiting = Some(reaction);
                None
            }
            None => {
                self.flying.insert(reaction_key(&reaction), None);
                Some(reaction)
            }
        }
    }

    /// A request ended, `taken` by Discord or not. Returns the request to
    /// send next, if a later click wants something else than what Discord
    /// now has, and whether to undo this one on screen: only when no later
    /// click says what is wanted.
    fn finished(
        &mut self,
        reaction: &ReactionRequest,
        taken: bool,
    ) -> (Option<ReactionRequest>, bool) {
        let key = reaction_key(reaction);
        let waiting = self.flying.remove(&key).flatten();
        let on_discord = if taken { reaction.add } else { !reaction.add };
        match waiting {
            None => (None, !taken),
            Some(next) if next.add == on_discord => (None, false),
            Some(next) => {
                self.flying.insert(key, None);
                (Some(next), false)
            }
        }
    }
}

enum Settled {
    Next(Option<ReactionRequest>),
    Revoked,
}

/// Takes a reaction request's end: the window undoes one Discord did not
/// take (unless a later click decides), and the next waiting one leaves.
fn settle(
    reactions: &mut Reactions,
    reaction: ReactionRequest,
    outcome: Outcome,
    emit: Emit<'_>,
) -> Settled {
    if outcome == Outcome::Revoked {
        emit(Event::ReactionFailed(reaction));
        return Settled::Revoked;
    }
    let (next, undo) = reactions.finished(&reaction, outcome == Outcome::Taken);
    if undo {
        emit(Event::ReactionFailed(reaction));
    }
    Settled::Next(next)
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
    fn gateway_messages_reach_the_notifier_and_the_window() {
        let acks = RefCell::new(AckQueue::default());
        let events = std::sync::Mutex::new(Vec::new());
        let emit = |event: Event| events.lock().unwrap().push(event);
        let notices = RefCell::new(Vec::new());
        let notify = |notice: Notice| notices.borrow_mut().push(notice);
        let connection = Connection {
            acks: &acks,
            emit: &emit,
            notifications: RefCell::new(Notifications::new(Arc::default())),
            notify: &notify,
        };
        let model = crate::demo::model();
        let dm = model.dms_by_recency()[0].id;
        let me = model.me;
        let next = model.dm(dm).and_then(|d| d.last_message_id).unwrap() + 1;
        connection.ready(model);
        let message = |id, author| Update::MessageCreate {
            channel: dm,
            guild: None,
            message: crate::model::Message {
                id,
                author: User {
                    id: author,
                    ..User::default()
                },
                content: "hi".into(),
                attachments: vec![],
                embeds: vec![],
                reactions: vec![],
                ..Default::default()
            },
            nonce: None,
            ping: crate::model::Ping::default(),
        };
        connection.update(message(next, me), me);
        assert!(notices.borrow().is_empty(), "my own message");
        connection.update(message(next + 1, me + 1), me);
        assert!(matches!(&notices.borrow()[..], [Notice::Show(n)] if n.channel == dm));
        assert_eq!(events.lock().unwrap().len(), 3);
    }

    #[test]
    fn notifications_come_down_when_the_account_leaves() {
        assert!(signs_out(None), "logged out");
        assert!(signs_out(Some(&Ended::Revoked)));
        assert!(!signs_out(Some(&Ended::Refused(4004))));
        assert!(!signs_out(Some(&Ended::Unreadable)));
    }

    fn reaction(message: Id, add: bool) -> ReactionRequest {
        ReactionRequest {
            channel: 1,
            guild: None,
            message,
            emoji: crate::model::Emoji {
                id: None,
                name: "👍".into(),
                animated: false,
            },
            add,
        }
    }

    #[test]
    fn one_reaction_flies_at_a_time_and_only_the_last_click_waits() {
        let mut flying = Reactions::default();
        assert_eq!(flying.request(reaction(5, true)), Some(reaction(5, true)));
        assert_eq!(
            flying.request(reaction(6, true)),
            Some(reaction(6, true)),
            "another message"
        );
        assert_eq!(flying.request(reaction(5, false)), None);
        assert_eq!(
            flying.request(reaction(5, true)),
            None,
            "replaces the waiting one"
        );
        // Taken: Discord has it, as the last click wants: nothing more to do.
        assert_eq!(flying.finished(&reaction(5, true), true), (None, false));
        assert_eq!(flying.request(reaction(5, false)), Some(reaction(5, false)));

        // Refused with a later click waiting: the click decides, no undo.
        assert_eq!(flying.request(reaction(5, true)), None);
        let (next, undo) = flying.finished(&reaction(5, false), false);
        assert_eq!(
            (next, undo),
            (None, false),
            "Discord still has it, as wanted"
        );
        assert_eq!(flying.request(reaction(5, false)), Some(reaction(5, false)));
        assert_eq!(flying.request(reaction(5, true)), None);
        assert_eq!(flying.request(reaction(5, false)), None);
        let (next, undo) = flying.finished(&reaction(5, false), false);
        assert_eq!(
            (next, undo),
            (Some(reaction(5, false)), false),
            "tried again"
        );
        assert_eq!(
            flying.finished(&reaction(5, false), false),
            (None, true),
            "undone"
        );
        assert_eq!(flying.finished(&reaction(6, true), true), (None, false));
        assert!(flying.flying.is_empty());
    }

    #[test]
    fn a_reaction_retries_with_the_rotated_token_and_reports_revocation() {
        use api::Reacted::*;
        paused(async {
            let token = RefCell::new(Token::new("first".into()));
            let answers = RefCell::new(vec![Unauthorized, Done].into_iter());
            let used = RefCell::new(Vec::new());
            let send = |_: ReactionRequest, with: Token| {
                used.borrow_mut().push(with.expose().to_owned());
                *token.borrow_mut() = Token::new("second".into());
                let answer = answers.borrow_mut().next().expect("an answer");
                async move { (answer, with) }
            };
            assert_eq!(
                react(&send, &token, &reaction(5, true)).await,
                Outcome::Taken
            );
            assert_eq!(*used.borrow(), ["first", "second"]);

            // A failure retried, then a rotation: still one more try.
            *token.borrow_mut() = Token::new("third".into());
            let answers = RefCell::new(vec![Failed, Unauthorized, Done].into_iter());
            let rotating = |_: ReactionRequest, with: Token| {
                let answer = answers.borrow_mut().next().expect("an answer");
                // Rotated while the 401 was on its way.
                if answer == Unauthorized {
                    *token.borrow_mut() = Token::new("fourth".into());
                }
                async move { (answer, with) }
            };
            let outcome = react(&rotating, &token, &reaction(5, true)).await;
            assert_eq!(outcome, Outcome::Taken);

            let answers = RefCell::new(vec![Unauthorized].into_iter());
            let send = |_: ReactionRequest, with: Token| {
                let answer = answers.borrow_mut().next().expect("an answer");
                async move { (answer, with) }
            };
            assert_eq!(
                react(&send, &token, &reaction(5, true)).await,
                Outcome::Revoked
            );
        });
    }

    #[test]
    fn a_revoked_reaction_is_undone_and_ends_the_session() {
        let events = Mutex::new(Vec::new());
        let emit = |event: Event| events.lock().unwrap().push(event);
        let mut flying = Reactions::default();
        flying.request(reaction(5, true));
        let settled = settle(&mut flying, reaction(5, true), Outcome::Revoked, &emit);
        assert!(matches!(settled, Settled::Revoked));
        flying.request(reaction(6, false));
        let settled = settle(&mut flying, reaction(6, false), Outcome::Refused, &emit);
        assert!(matches!(settled, Settled::Next(None)));
        assert_eq!(
            events.into_inner().unwrap(),
            [
                Event::ReactionFailed(reaction(5, true)),
                Event::ReactionFailed(reaction(6, false))
            ]
        );
    }

    #[test]
    fn a_reaction_is_tried_twice_at_most_then_undone() {
        use api::Reacted::*;
        let wait = Duration::from_secs(2);
        let next = |reacted, retried| after_reaction(reacted, retried, false);
        assert_eq!(next(Done, false), Next::Done);
        assert_eq!(next(RateLimited(wait), false), Next::Retry(wait));
        assert_eq!(next(Failed, false), Next::Retry(Duration::ZERO));
        assert_eq!(next(Done, true), Next::Done);
        assert_eq!(next(RateLimited(wait), true), Next::Undo);
        assert_eq!(next(Failed, true), Next::Undo);
        assert_eq!(next(Refused, false), Next::Undo);
        assert_eq!(next(Unauthorized, false), Next::Revoked);
        let rotated = after_reaction(Unauthorized, false, true);
        assert_eq!(rotated, Next::Rotated, "a token rotated since");
        let after_a_retry = after_reaction(Unauthorized, true, true);
        assert_eq!(after_a_retry, Next::Rotated, "even after a first retry");
        assert_eq!(after_reaction(Unauthorized, true, false), Next::Revoked);
    }

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

    /// What a signed-in phase came to.
    struct Phase {
        served: Served,
        /// Acks sent.
        sent: usize,
        /// Messages posted, by nonce, in order.
        posted: Vec<Id>,
        events: Vec<Event>,
        /// Nothing left for closing to wait for.
        idle: bool,
    }

    /// A signed-in phase whose acks get `answers` and messages `posts`, in
    /// turn, after `commands` arrive; the window closes two minutes later.
    /// With `rotate`, the first token is replaced while a request flies.
    async fn session_with(
        commands: Vec<Command>,
        answers: Vec<Delivery>,
        posts: Vec<Attempt>,
        rotate: bool,
    ) -> Phase {
        let api = Api::new();
        let token = RefCell::new(Token::new("first".into()));
        let acks = RefCell::new(AckQueue::default());
        let busy = Pending::default();
        let events = Mutex::new(Vec::new());
        let emit = |event: Event| events.lock().unwrap().push(event);
        let answers = RefCell::new(answers.into_iter());
        let posts = RefCell::new(posts.into_iter());
        let sent = Cell::new(0);
        let posted = RefCell::new(Vec::new());
        let flying = Cell::new(0);
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
        let post = |write: Write, used: Token| {
            posted.borrow_mut().push(match &write {
                Write::Send(outgoing) => outgoing.nonce,
                Write::Edit { id, .. } | Write::Delete { id, .. } => *id,
            });
            let attempt = posts.borrow_mut().next().expect("an answer to a message");
            flying.set(flying.get() + 1);
            let (token, flying) = (&token, &flying);
            async move {
                assert_eq!(flying.get(), 1, "one message at a time");
                tokio::time::sleep(Duration::from_millis(50)).await;
                flying.set(flying.get() - 1);
                if rotate && used.expose() == "first" {
                    *token.borrow_mut() = Token::new("second".into());
                }
                (attempt, used)
            }
        };
        let (sender, mut receiver) = unbounded_channel();
        let (served, ()) = tokio::join!(
            serve(&mut receiver, &api, &token, &emit, &acks, &busy, send, post),
            async move {
                tokio::task::yield_now().await;
                for command in commands {
                    sender.send(command).unwrap();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                tokio::time::sleep(Duration::from_secs(120)).await;
            }
        );
        Phase {
            served,
            sent: sent.get(),
            posted: posted.into_inner(),
            events: events.into_inner().unwrap(),
            idle: busy.idle(),
        }
    }

    const PLACE: fn(Id) -> Place = |channel| Place {
        channel,
        guild: Some(1),
    };

    fn message(channel: Id, nonce: Id) -> Command {
        Command::Write(Write::Send(Outgoing {
            place: PLACE(channel),
            nonce,
            content: "salut".into(),
        }))
    }

    fn answered(status: u16, body: Option<&str>) -> Attempt {
        let body = body.map(str::to_owned);
        Attempt::Answered { status, body }
    }

    fn failures(events: &[Event]) -> Vec<(Id, Option<String>)> {
        let failed = events.iter().filter_map(|event| match event {
            Event::SendFailed { nonce, reason, .. } => Some((*nonce, reason.clone())),
            _ => None,
        });
        failed.collect()
    }

    #[test]
    fn messages_leave_one_at_a_time_in_order() {
        let outcome = paused(session_with(
            vec![message(7, 1), message(8, 2), message(7, 3)],
            vec![],
            vec![
                answered(200, None),
                answered(200, None),
                answered(200, None),
            ],
            false,
        ));
        assert!(matches!(outcome.served, Served::Closed));
        assert_eq!(outcome.posted, [1, 2, 3]);
        // Sent, even with an answer that could not be read: no failure, and
        // the gateway's copy will confirm them.
        assert!(failures(&outcome.events).is_empty());
    }

    #[test]
    fn a_confirmed_message_reads_its_channel() {
        let body = r#"{"id":"500","channel_id":"7","content":"salut","author":{"id":"1","username":"me"},"nonce":"1"}"#;
        let outcome = paused(session_with(
            vec![Command::Ack(ack(7, false)), message(7, 1)],
            vec![],
            vec![answered(200, Some(body))],
            false,
        ));
        assert_eq!(outcome.sent, 0, "no ack is owed for it");
        assert!(outcome.events.iter().any(|event| matches!(
            event,
            Event::Update(Update::MessageCreate {
                guild: Some(1),
                nonce: Some(1),
                ..
            })
        )));
    }

    #[test]
    fn a_failure_takes_its_channels_later_messages_along() {
        let outcome = paused(session_with(
            vec![message(7, 1), message(7, 2), message(8, 3)],
            vec![],
            vec![Attempt::Lost, answered(200, None)],
            false,
        ));
        assert_eq!(outcome.posted, [1, 3]);
        assert!(outcome.events.contains(&Event::SendUnsure {
            channel: 7,
            nonce: 1
        }));
        assert_eq!(failures(&outcome.events), [(2, None)]);
    }

    #[test]
    fn refusals_and_rate_limits() {
        let slow = r#"{"message":"You are being rate limited.","retry_after":30,"global":false}"#;
        let slowmode = r#"{"message":"Slowmode is enabled.","code":20016,"retry_after":3}"#;
        let outcome = paused(session_with(
            vec![message(7, 1), message(8, 2)],
            vec![],
            vec![
                answered(429, Some(slow)),
                answered(200, None),
                answered(429, Some(slowmode)),
            ],
            false,
        ));
        assert_eq!(outcome.posted, [1, 1, 2]);
        let wait = Duration::from_secs(30);
        assert!(outcome.events.contains(&Event::SendHeld {
            channel: 7,
            nonce: 1,
            wait
        }));
        assert_eq!(
            failures(&outcome.events),
            [(2, Some("Slowmode is enabled.".into()))]
        );
    }

    #[test]
    fn a_401_on_a_message_ends_the_session_only_for_the_token_in_force() {
        let refused = || answered(401, Some("{}"));
        let outcome = paused(session_with(
            vec![message(7, 1), message(7, 2)],
            vec![],
            vec![refused()],
            false,
        ));
        assert!(matches!(outcome.served, Served::Revoked));
        assert!(outcome.idle, "closing has nothing left to wait for");
        // The token was replaced while it flew: the message goes again.
        let outcome = paused(session_with(
            vec![message(7, 1)],
            vec![],
            vec![refused(), answered(200, None)],
            true,
        ));
        assert!(matches!(outcome.served, Served::Closed));
        assert_eq!(outcome.posted, [1, 1]);
    }

    #[test]
    fn closing_waits_for_messages_handed_over_and_says_what_was_lost() {
        let busy = Pending::default();
        assert!(busy.idle());
        busy.handed.fetch_add(1, Ordering::Relaxed);
        assert!(!busy.idle(), "handed over, not yet taken in");
        busy.taken();
        busy.taken();
        assert!(busy.idle());
        assert_eq!(unsent_lost(&busy), None);
        busy.unsent.store(2, Ordering::Relaxed);
        busy.working.store(true, Ordering::Relaxed);
        let lost = unsent_lost(&busy).unwrap();
        assert_eq!(lost, "2 messages could not be sent.");
        assert!(busy.idle() && unsent_lost(&busy).is_none(), "said once");
    }

    #[test]
    fn edits_and_deletions_report_what_became_of_them() {
        let edit = |id| {
            Command::Write(Write::Edit {
                place: PLACE(7),
                id,
                content: "non".into(),
            })
        };
        let delete = |id| {
            Command::Write(Write::Delete {
                place: PLACE(7),
                id,
            })
        };
        let answer = r#"{"id":"40","channel_id":"7","content":"non","edited_timestamp":"2026-10-07T12:00:00+00:00"}"#;
        let forbidden =
            r#"{"message":"Cannot edit a message authored by another user","code":50005}"#;
        let outcome = paused(session_with(
            vec![edit(40), message(7, 1), edit(41), delete(42), delete(43)],
            vec![],
            vec![
                answered(200, Some(answer)),
                answered(200, None),
                answered(403, Some(forbidden)),
                answered(204, None),
                Attempt::Lost,
            ],
            false,
        ));
        assert_eq!(outcome.posted, [40, 1, 41, 42, 43]);
        let changed = |id, change, result| Event::Changed {
            channel: 7,
            id,
            change,
            result,
        };
        let events = &outcome.events;
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Update(Update::MessageEdit {
                id: 40,
                edited: true,
                ..
            })
        )));
        assert!(events.contains(&changed(40, Change::Edit, Ok(()))));
        let refused = Some("Cannot edit a message authored by another user".into());
        assert!(events.contains(&changed(41, Change::Edit, Err(refused))));
        let deleted = Update::MessageDelete {
            channel: 7,
            ids: vec![42],
        };
        assert!(events.contains(&Event::Update(deleted)));
        assert!(events.contains(&changed(42, Change::Delete, Ok(()))));
        assert!(events.contains(&changed(43, Change::Delete, Err(None))));
        // A failed edit takes no message along.
        assert!(failures(events).is_empty());
    }

    #[test]
    fn logging_out_drops_the_messages_still_waiting() {
        let outcome = paused(session_with(
            vec![message(7, 1), message(7, 2), Command::LogOut],
            vec![],
            vec![answered(200, None)],
            false,
        ));
        assert!(matches!(outcome.served, Served::LoggedOut));
        assert_eq!(outcome.posted, [1]);
    }

    #[test]
    fn a_401_on_the_token_in_force_ends_the_session() {
        let Phase { served, sent, .. } = paused(session_with(
            vec![Command::Ack(ack(5, true))],
            vec![Delivery::Unauthorized],
            vec![],
            false,
        ));
        assert!(matches!(served, Served::Revoked));
        assert_eq!(sent, 1);
    }

    #[test]
    fn a_401_on_a_token_since_rotated_tries_again() {
        let Phase {
            served,
            sent,
            events,
            ..
        } = paused(session_with(
            vec![
                Command::Ack(ack(5, true)),
                Command::Ack(ack(6, false)),
                Command::LogOut,
            ],
            vec![Delivery::Unauthorized, Delivery::Saved, Delivery::Saved],
            vec![],
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
        let Phase { sent, events, .. } = paused(session_with(
            vec![Command::Ack(ack(5, true)), Command::LogOut],
            vec![Delivery::Retry(None), Delivery::Saved],
            vec![],
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
            let busy = Pending::default();
            let emit = |_: Event| {};
            let sent = Cell::new(0);
            let send = |_: Ack, used: Token| {
                sent.set(sent.get() + 1);
                async move { (Delivery::Saved, used) }
            };
            let post = |_: Write, used: Token| async move { (Attempt::Lost, used) };
            let (sender, mut receiver) = unbounded_channel();
            tokio::select! {
                _ = serve(&mut receiver, &api, &token, &emit, &acks, &busy, send, post) => {
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
