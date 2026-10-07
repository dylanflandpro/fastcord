//! Signing in by QR code: Discord's remote auth handshake, version 2.
//!
//! The desktop client opens a session with a fresh RSA key, shows the
//! session's fingerprint as a QR code, and the mobile app scanning it sends
//! back a ticket only this key can read. The password never passes through
//! fastcord. Protocol: <https://docs.discord.food/remote-authentication/desktop>.
//!
//! [`Handshake`] is the protocol without the network, so it can be tested;
//! [`run`] drives it over the socket.

use crate::credentials::Token;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{SinkExt as _, StreamExt as _};
use rsa::pkcs8::EncodePublicKey as _;
use rsa::sha2::{Digest as _, Sha256};
use rsa::{Oaep, RsaPrivateKey};
use std::time::Duration;
use zeroize::Zeroizing;

pub const GATEWAY_HOST: &str = "remote-auth-gateway.discord.gg";
pub const GATEWAY_PATH: &str = "/?v=2";
/// The gateway only answers pages served from Discord's own origins.
pub const ORIGIN: &str = "https://discord.com";

/// How long connecting and the gateway's hello may take.
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);

/// The gateway closes a session it has kept open for its `timeout_ms`.
const CLOSE_TIMED_OUT: u16 = 4003;

#[derive(Debug, PartialEq, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        heartbeat_interval: u64,
        timeout_ms: u64,
    },
    NonceProof {
        encrypted_nonce: String,
    },
    PendingRemoteInit {
        fingerprint: String,
    },
    PendingTicket {
        encrypted_user_payload: String,
    },
    PendingLogin {
        ticket: String,
    },
    Cancel,
    HeartbeatAck,
    /// An operation this client does not know; ignored.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientMessage {
    Init { encoded_public_key: String },
    NonceProof { nonce: String },
    Heartbeat,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Discord sent something this version cannot read")]
    Protocol,
    #[error("the session's fingerprint does not match this device's key")]
    FingerprintMismatch,
    #[error("the QR code expired")]
    Expired,
    #[error("sign-in was cancelled on the phone")]
    Cancelled,
    #[error("Discord stopped answering")]
    NoHeartbeatAck,
    #[error("the connection closed unexpectedly")]
    Closed,
    /// Discord wants a captcha before handing over the token.
    #[error("Discord asks for a captcha")]
    Captcha(Box<crate::api::Challenge>),
    #[error("Discord asks for a captcha and {0}")]
    CaptchaUnsolved(crate::captcha::Error),
    #[error("Discord did not accept the captcha")]
    CaptchaRefused,
    #[error("unable to reach Discord")]
    Network,
}

/// The session's key pair. The private half never leaves memory.
pub struct Keys {
    private: RsaPrivateKey,
    /// The public key as SPKI DER, what Discord fingerprints.
    public_der: Vec<u8>,
}

impl Keys {
    /// A fresh 2048-bit key, as the protocol requires.
    pub fn generate() -> Self {
        let private = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
            .expect("2048-bit RSA key generation");
        let public_der = private
            .to_public_key()
            .to_public_key_der()
            .expect("an RSA public key encodes as SPKI")
            .into_vec();
        Self {
            private,
            public_der,
        }
    }

    pub fn encoded_public_key(&self) -> String {
        STANDARD.encode(&self.public_der)
    }

    /// What Discord should announce for this key: the SHA-256 of its SPKI,
    /// URL-safe base64 without padding.
    pub fn fingerprint(&self) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(&self.public_der))
    }

    /// Opens something Discord sealed with this key (RSA-OAEP, SHA-256).
    pub fn decrypt(&self, base64: &str) -> Result<Zeroizing<Vec<u8>>, Error> {
        let sealed = STANDARD.decode(base64).map_err(|_| Error::Protocol)?;
        self.private
            .decrypt(Oaep::new::<Sha256>(), &sealed)
            .map(Zeroizing::new)
            .map_err(|_| Error::Protocol)
    }

    #[cfg(test)]
    fn seal(&self, plain: &[u8]) -> String {
        let sealed = self
            .private
            .to_public_key()
            .encrypt(&mut rsa::rand_core::OsRng, Oaep::new::<Sha256>(), plain)
            .unwrap();
        STANDARD.encode(sealed)
    }
}

/// Who scanned the code, from the payload Discord sends before the phone
/// confirms.
#[derive(Clone, Debug, PartialEq)]
pub struct ScannedUser {
    pub id: u64,
    pub username: String,
    pub avatar: Option<String>,
}

/// `user_id:discriminator:avatar:username`, the avatar `0` when there is none.
/// The username comes last because it is the only part that could hold a
/// colon.
fn parse_user_payload(payload: &str) -> Option<ScannedUser> {
    let mut parts = payload.splitn(4, ':');
    let id = parts.next()?.parse().ok()?;
    let _discriminator = parts.next()?;
    let avatar = parts.next()?;
    let username = parts.next()?;
    Some(ScannedUser {
        id,
        username: username.to_owned(),
        avatar: (avatar != "0").then(|| avatar.to_owned()),
    })
}

/// What the network side does next.
#[derive(Debug, PartialEq)]
pub enum Step {
    Send(ClientMessage),
    /// Heartbeat at this interval from now on.
    Heartbeat(Duration),
    /// Show this as a QR code.
    ShowQr(String),
    Scanned(ScannedUser),
    /// Trade this ticket for the token.
    Ticket(String),
    Nothing,
}

pub struct Handshake {
    keys: Keys,
    awaiting_ack: bool,
}

impl Handshake {
    pub fn new(keys: Keys) -> Self {
        Self {
            keys,
            awaiting_ack: false,
        }
    }

    pub fn keys(&self) -> &Keys {
        &self.keys
    }

    pub fn on_message(&mut self, message: ServerMessage) -> Result<Vec<Step>, Error> {
        Ok(match message {
            // A zero interval would make the heartbeat timer panic.
            ServerMessage::Hello {
                heartbeat_interval: 0,
                ..
            } => return Err(Error::Protocol),
            ServerMessage::Hello {
                heartbeat_interval, ..
            } => vec![
                Step::Heartbeat(Duration::from_millis(heartbeat_interval)),
                Step::Send(ClientMessage::Init {
                    encoded_public_key: self.keys.encoded_public_key(),
                }),
            ],
            ServerMessage::NonceProof { encrypted_nonce } => {
                let nonce = self.keys.decrypt(&encrypted_nonce)?;
                vec![Step::Send(ClientMessage::NonceProof {
                    nonce: URL_SAFE_NO_PAD.encode(nonce.as_slice()),
                })]
            }
            ServerMessage::PendingRemoteInit { fingerprint } => {
                // A fingerprint for another key means someone else's session:
                // scanning it would sign that device in.
                if fingerprint != self.keys.fingerprint() {
                    return Err(Error::FingerprintMismatch);
                }
                vec![Step::ShowQr(format!(
                    "https://discord.com/ra/{fingerprint}"
                ))]
            }
            ServerMessage::PendingTicket {
                encrypted_user_payload,
            } => {
                let payload = self.keys.decrypt(&encrypted_user_payload)?;
                let payload = std::str::from_utf8(&payload).map_err(|_| Error::Protocol)?;
                vec![Step::Scanned(
                    parse_user_payload(payload).ok_or(Error::Protocol)?,
                )]
            }
            ServerMessage::PendingLogin { ticket } => vec![Step::Ticket(ticket)],
            ServerMessage::Cancel => return Err(Error::Cancelled),
            ServerMessage::HeartbeatAck => {
                self.awaiting_ack = false;
                vec![Step::Nothing]
            }
            ServerMessage::Unknown => vec![Step::Nothing],
        })
    }

    /// The heartbeat to send now, or an error when the last one went
    /// unanswered.
    pub fn heartbeat(&mut self) -> Result<ClientMessage, Error> {
        if std::mem::replace(&mut self.awaiting_ack, true) {
            return Err(Error::NoHeartbeatAck);
        }
        Ok(ClientMessage::Heartbeat)
    }
}

/// What [`run`] reports while the handshake goes on.
pub enum Progress {
    Qr(String),
    Scanned(ScannedUser),
    /// The captcha window is open.
    Captcha,
}

/// How many captchas to show for one sign-in: Discord asking again after
/// two solved ones means it will keep asking.
const MAX_CAPTCHAS: usize = 2;

/// Trades the approved ticket for the token, solving the captchas Discord
/// asks for on the way.
async fn exchange(
    api: &crate::api::Api,
    keys: &Keys,
    ticket: &str,
    progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<Token, Error> {
    let sealed = with_captchas(
        async |answer| api.exchange_ticket(ticket, answer).await,
        async |challenge| {
            progress(Progress::Captcha);
            crate::captcha::solve(challenge).await
        },
    )
    .await?;
    let token = keys.decrypt(&sealed)?;
    let token = String::from_utf8(token.to_vec()).map_err(|_| Error::Protocol)?;
    Ok(Token::new(token))
}

/// Makes `attempt`, solving each captcha it is refused with through
/// `solve` and sending the answer with the next attempt, up to
/// [`MAX_CAPTCHAS`].
async fn with_captchas(
    mut attempt: impl AsyncFnMut(Option<&crate::api::Answer>) -> Result<String, Error>,
    mut solve: impl AsyncFnMut(&crate::api::Challenge) -> Result<String, crate::captcha::Error>,
) -> Result<String, Error> {
    let mut answer = None;
    let mut solved = 0;
    loop {
        match attempt(answer.as_ref()).await {
            Err(Error::Captcha(challenge)) => {
                // Asking again after the answers it got: another one would
                // be refused the same way, so do not show it.
                if solved == MAX_CAPTCHAS {
                    return Err(Error::CaptchaRefused);
                }
                let key = solve(&challenge).await.map_err(Error::CaptchaUnsolved)?;
                answer = Some((*challenge).answer(key));
                solved += 1;
            }
            result => return result,
        }
    }
}

/// One QR session, from connecting to the token. Returns
/// [`Error::Expired`] or [`Error::Cancelled`] when a new code should be shown.
pub async fn run(
    api: &crate::api::Api,
    progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<Token, Error> {
    let (keys, ()) = tokio::join!(tokio::task::spawn_blocking(Keys::generate), api.prepare());
    let keys = keys.map_err(|_| Error::Protocol)?;
    let mut handshake = Handshake::new(keys);

    let connect = crate::websocket::connect(
        GATEWAY_HOST,
        GATEWAY_PATH,
        &[("Origin", ORIGIN), ("User-Agent", crate::api::USER_AGENT)],
    );
    let mut socket = tokio::time::timeout(HELLO_TIMEOUT, connect)
        .await
        .map_err(|_| Error::Network)?
        .map_err(|error| {
            // Connection errors name hosts and statuses, nothing private.
            log::debug!("remote auth gateway: {error}");
            Error::Network
        })?;

    // Replaced by Hello's interval; nothing is sent before it.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(3600));
    heartbeat.reset();
    // A gateway that accepts the connection but never says hello would
    // otherwise keep the spinner turning forever.
    let hello = tokio::time::sleep(HELLO_TIMEOUT);
    tokio::pin!(hello);
    let mut greeted = false;

    loop {
        let steps = tokio::select! {
            frame = socket.next() => {
                let Some(Ok(frame)) = frame else {
                    return Err(Error::Closed);
                };
                if let Some((code, _)) = frame.as_close() {
                    return Err(if u16::from(code) == CLOSE_TIMED_OUT {
                        Error::Expired
                    } else {
                        Error::Closed
                    });
                }
                let Some(text) = frame.as_text() else {
                    continue;
                };
                let message = serde_json::from_str(text).map_err(|_| Error::Protocol)?;
                handshake.on_message(message)?
            }
            _ = heartbeat.tick() => vec![Step::Send(handshake.heartbeat()?)],
            _ = &mut hello, if !greeted => return Err(Error::NoHeartbeatAck),
        };
        for step in steps {
            match step {
                Step::Send(message) => {
                    let text = serde_json::to_string(&message).map_err(|_| Error::Protocol)?;
                    socket
                        .send(tokio_websockets::Message::text(text))
                        .await
                        .map_err(|_| Error::Closed)?;
                }
                Step::Heartbeat(every) => {
                    greeted = true;
                    heartbeat = tokio::time::interval(every);
                    heartbeat.reset();
                }
                Step::ShowQr(url) => progress(Progress::Qr(url)),
                Step::Scanned(user) => progress(Progress::Scanned(user)),
                Step::Ticket(ticket) => {
                    // The gateway's part is done; a captcha can take longer
                    // than its heartbeats allow.
                    let _ = socket.close().await;
                    return exchange(api, handshake.keys(), &ticket, progress).await;
                }
                Step::Nothing => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// One key for every test: generating one is the slow part.
    fn keys() -> &'static Keys {
        static KEYS: OnceLock<Keys> = OnceLock::new();
        KEYS.get_or_init(Keys::generate)
    }

    fn handshake() -> Handshake {
        let keys = keys();
        Handshake::new(Keys {
            private: keys.private.clone(),
            public_der: keys.public_der.clone(),
        })
    }

    #[test]
    fn reads_the_server_messages() {
        let hello: ServerMessage = serde_json::from_str(
            r#"{"op":"hello","heartbeat_interval":41250,"timeout_ms":150000}"#,
        )
        .unwrap();
        assert_eq!(
            hello,
            ServerMessage::Hello {
                heartbeat_interval: 41250,
                timeout_ms: 150000
            }
        );
        let unknown: ServerMessage = serde_json::from_str(r#"{"op":"something_new"}"#).unwrap();
        assert_eq!(unknown, ServerMessage::Unknown);
    }

    #[test]
    fn writes_the_client_messages() {
        let init = ClientMessage::Init {
            encoded_public_key: "abc".into(),
        };
        assert_eq!(
            serde_json::to_string(&init).unwrap(),
            r#"{"op":"init","encoded_public_key":"abc"}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::Heartbeat).unwrap(),
            r#"{"op":"heartbeat"}"#
        );
    }

    #[test]
    fn hello_starts_heartbeats_and_sends_the_public_key() {
        let mut handshake = handshake();
        let steps = handshake
            .on_message(ServerMessage::Hello {
                heartbeat_interval: 41250,
                timeout_ms: 150000,
            })
            .unwrap();
        assert_eq!(steps[0], Step::Heartbeat(Duration::from_millis(41250)));
        let Step::Send(ClientMessage::Init { encoded_public_key }) = &steps[1] else {
            panic!("expected init, got {steps:?}");
        };
        assert_eq!(
            STANDARD.decode(encoded_public_key).unwrap(),
            keys().public_der
        );
    }

    #[test]
    fn proves_the_nonce_by_decrypting_it() {
        let mut handshake = handshake();
        let nonce = b"a nonce from discord";
        let steps = handshake
            .on_message(ServerMessage::NonceProof {
                encrypted_nonce: keys().seal(nonce),
            })
            .unwrap();
        assert_eq!(
            steps,
            [Step::Send(ClientMessage::NonceProof {
                nonce: URL_SAFE_NO_PAD.encode(nonce)
            })]
        );
    }

    #[test]
    fn shows_the_qr_only_for_its_own_fingerprint() {
        let mut handshake = handshake();
        let fingerprint = keys().fingerprint();
        assert_eq!(
            handshake
                .on_message(ServerMessage::PendingRemoteInit {
                    fingerprint: fingerprint.clone()
                })
                .unwrap(),
            [Step::ShowQr(format!(
                "https://discord.com/ra/{fingerprint}"
            ))]
        );
        assert!(matches!(
            handshake.on_message(ServerMessage::PendingRemoteInit {
                fingerprint: "someone-else".into()
            }),
            Err(Error::FingerprintMismatch)
        ));
    }

    #[test]
    fn reads_who_scanned() {
        let mut handshake = handshake();
        let steps = handshake
            .on_message(ServerMessage::PendingTicket {
                encrypted_user_payload: keys()
                    .seal(b"852892297661906993:0:05145cc5646fbcba277b6d5ea2030610:dolfies"),
            })
            .unwrap();
        assert_eq!(
            steps,
            [Step::Scanned(ScannedUser {
                id: 852892297661906993,
                username: "dolfies".into(),
                avatar: Some("05145cc5646fbcba277b6d5ea2030610".into()),
            })]
        );
    }

    #[test]
    fn user_payload_without_avatar() {
        assert_eq!(
            parse_user_payload("1:0:0:name:with:colons"),
            Some(ScannedUser {
                id: 1,
                username: "name:with:colons".into(),
                avatar: None,
            })
        );
        assert_eq!(parse_user_payload("not-an-id:0:0:x"), None);
        assert_eq!(parse_user_payload("1:0"), None);
    }

    #[test]
    fn a_missed_ack_ends_the_session() {
        let mut handshake = handshake();
        assert_eq!(handshake.heartbeat().unwrap(), ClientMessage::Heartbeat);
        handshake.on_message(ServerMessage::HeartbeatAck).unwrap();
        assert_eq!(handshake.heartbeat().unwrap(), ClientMessage::Heartbeat);
        assert!(matches!(handshake.heartbeat(), Err(Error::NoHeartbeatAck)));
    }

    #[test]
    fn a_zero_heartbeat_interval_is_refused() {
        assert!(matches!(
            handshake().on_message(ServerMessage::Hello {
                heartbeat_interval: 0,
                timeout_ms: 150000
            }),
            Err(Error::Protocol)
        ));
    }

    fn challenge() -> Box<crate::api::Challenge> {
        Box::new(
            serde_json::from_str(
                r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"key","captcha_rqtoken":"t"}"#,
            )
            .unwrap(),
        )
    }

    fn run_blocking<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn a_solved_captcha_goes_with_the_next_attempt() {
        let mut attempts = Vec::new();
        let result = run_blocking(with_captchas(
            async |answer| {
                attempts.push(answer.is_some());
                if answer.is_some() {
                    Ok("sealed".into())
                } else {
                    Err(Error::Captcha(challenge()))
                }
            },
            async |_| Ok("solved".into()),
        ));
        assert_eq!(result.unwrap(), "sealed");
        assert_eq!(attempts, [false, true]);
    }

    #[test]
    fn stops_before_showing_a_third_captcha() {
        let mut shown = 0;
        let result = run_blocking(with_captchas(
            async |_| Err(Error::Captcha(challenge())),
            async |_| {
                shown += 1;
                Ok("solved".into())
            },
        ));
        assert!(matches!(result, Err(Error::CaptchaRefused)));
        assert_eq!(shown, MAX_CAPTCHAS);
    }

    #[test]
    fn a_closed_captcha_window_ends_the_exchange() {
        let result = run_blocking(with_captchas(
            async |_| Err(Error::Captcha(challenge())),
            async |_| Err(crate::captcha::Error::Closed),
        ));
        assert!(matches!(
            result,
            Err(Error::CaptchaUnsolved(crate::captcha::Error::Closed))
        ));
    }

    #[test]
    fn cancel_on_the_phone_ends_the_session() {
        assert!(matches!(
            handshake().on_message(ServerMessage::Cancel),
            Err(Error::Cancelled)
        ));
    }
}
