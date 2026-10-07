//! Everything that talks to Discord or the keyring, on its own thread.
//!
//! The interface sends [`Command`]s and reads the [`Session`] it reports; it never waits on
//! the network. Each change asks the window for a repaint.

use crate::api::{self, Api};
use crate::credentials::{self, Token};
use crate::model::User;
use crate::remote_auth::{self, Progress};
use std::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

#[derive(Debug)]
pub enum Command {
    /// Try again after a failure.
    Retry,
    LogOut,
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
    SignedIn(User),
    /// Something went wrong; [`Command::Retry`] starts over.
    Failed(String),
}

pub struct Backend {
    commands: UnboundedSender<Command>,
    events: mpsc::Receiver<Session>,
}

impl Backend {
    pub fn start(ctx: egui::Context) -> Self {
        let (commands, receiver) = unbounded_channel();
        let (sender, events) = mpsc::channel();
        let emit = move |event: Session| {
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
                runtime.block_on(session(receiver, &emit));
            })
            .expect("the backend thread");
        Self { commands, events }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    pub fn events(&self) -> impl Iterator<Item = Session> + '_ {
        self.events.try_iter()
    }
}

type Emit<'a> = &'a (dyn Fn(Session) + Send + Sync);

/// Restore or sign in, stay signed in until logged out, then start over.
/// Returns when the window is gone.
async fn session(mut commands: UnboundedReceiver<Command>, emit: Emit<'_>) {
    let api = Api::new();
    loop {
        emit(Session::Checking);
        let signed_in = match restore(&api).await {
            Ok(Some(signed_in)) => Some(signed_in),
            Ok(None) => sign_in(&api, emit).await,
            Err(message) => {
                emit(Session::Failed(message));
                None
            }
        };
        let Some((token, user)) = signed_in else {
            if !wait_for(&mut commands, |c| matches!(c, Command::Retry)).await {
                return;
            }
            continue;
        };
        emit(Session::SignedIn(user));
        if !wait_for(&mut commands, |c| matches!(c, Command::LogOut)).await {
            return;
        }
        if let Err(error) = log_out(&api, token).await {
            emit(Session::Failed(error));
            if !wait_for(&mut commands, |c| matches!(c, Command::Retry)).await {
                return;
            }
        }
    }
}

/// Waits for a command matching `wanted`, ignoring others. `false` once the
/// window has closed.
async fn wait_for(
    commands: &mut UnboundedReceiver<Command>,
    wanted: impl Fn(&Command) -> bool,
) -> bool {
    while let Some(command) = commands.recv().await {
        if wanted(&command) {
            return true;
        }
    }
    false
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(work)
        .await
        .expect("keyring call panicked")
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
async fn restore(api: &Api) -> Result<Option<(Token, User)>, String> {
    let Some(token) = blocking(credentials::load).await.map_err(keyring_message)? else {
        return Ok(None);
    };
    match api.me(&token).await {
        Ok(user) => Ok(Some((token, user))),
        Err(api::Error::Unauthorized) => {
            log::info!("the stored session was revoked; signing in again");
            blocking(credentials::delete)
                .await
                .map_err(keyring_message)?;
            Ok(None)
        }
        Err(error) => Err(format!("Unable to check the session: {error}.")),
    }
}

/// QR codes until one is scanned and approved. An expired or cancelled code
/// is replaced at once, as the official client does.
async fn sign_in(api: &Api, emit: Emit<'_>) -> Option<(Token, User)> {
    let progress = |progress: Progress| match progress {
        Progress::Qr(url) => emit(Session::Qr(url)),
        Progress::Scanned(user) => emit(Session::Scanned {
            username: user.username,
        }),
    };
    let token = loop {
        match remote_auth::run(api, &progress).await {
            Ok(token) => break token,
            Err(remote_auth::Error::Expired | remote_auth::Error::Cancelled) => {
                log::info!("QR session ended; showing a new code");
            }
            Err(error) => {
                log::warn!("QR sign-in failed: {error}");
                emit(Session::Failed(format!("Sign-in failed: {error}.")));
                return None;
            }
        }
    };
    let token = match blocking(move || credentials::save(&token).map(|()| token)).await {
        Ok(token) => token,
        Err(error) => {
            emit(Session::Failed(keyring_message(error)));
            return None;
        }
    };
    match api.me(&token).await {
        Ok(user) => Some((token, user)),
        Err(error) => {
            emit(Session::Failed(format!(
                "Unable to read the account: {error}."
            )));
            None
        }
    }
}

/// Ends the session on Discord's side, then forgets the token. A failed
/// remote logout still removes it locally.
async fn log_out(api: &Api, token: Token) -> Result<(), String> {
    if let Err(error) = api.logout(&token).await {
        log::warn!("remote logout failed: {error}");
    }
    drop(token);
    blocking(credentials::delete).await.map_err(|error| {
        format!(
            "The session could not be removed: {}",
            keyring_message(error)
        )
    })
}
