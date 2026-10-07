//! Discord's gateway: one WebSocket that delivers every change to the
//! account as it happens.
//!
//! [`Gateway`] is the protocol without the network (hello, identify,
//! heartbeats, resume, and what each way of ending means), so it can be
//! tested; [`connect`] drives one connection over the socket. The backend
//! keeps one `Gateway` across connections so a dropped one resumes where it
//! left off. Protocol: <https://docs.discord.food/topics/gateway>.

use crate::credentials::Token;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::value::RawValue;
use std::time::Duration;

pub const HOST: &str = "gateway.discord.gg";
const PATH: &str = "/?v=9&encoding=json";

/// What the official web client asks the gateway for (read from its
/// source): deduplicated users, READY split in two, versioned read states,
/// token refresh, and the rest of its usual set. Each one changes the shape
/// of what arrives, and `events` reads those shapes.
const CAPABILITIES: u64 = 1_734_653;

/// How long connecting and the first hello may take.
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);

mod op {
    pub const DISPATCH: u8 = 0;
    pub const HEARTBEAT: u8 = 1;
    pub const IDENTIFY: u8 = 2;
    pub const RESUME: u8 = 6;
    pub const RECONNECT: u8 = 7;
    pub const INVALID_SESSION: u8 = 9;
    pub const HELLO: u8 = 10;
    pub const HEARTBEAT_ACK: u8 = 11;
}

/// One message from the gateway, its data left unparsed until its event is
/// known.
#[derive(serde::Deserialize)]
struct Frame<'a> {
    op: u8,
    #[serde(borrow)]
    d: Option<&'a RawValue>,
    s: Option<u64>,
    t: Option<&'a str>,
}

#[derive(serde::Deserialize)]
struct Hello {
    heartbeat_interval: u64,
}

/// The parts of READY the connection itself needs.
#[derive(serde::Deserialize)]
struct ReadySession {
    session_id: String,
    resume_gateway_url: String,
}

/// Why a connection ended, and so what to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// Connect again and resume the session.
    Resume,
    /// Connect again and start a new session.
    Identify,
    /// Discord no longer accepts the token.
    AuthenticationFailed,
    /// Discord refuses this client outright; retrying would not help.
    Refused(u16),
}

/// What the connection does next.
#[derive(Debug)]
pub enum Step {
    Send(String),
    /// Heartbeat after `first`, then every `every`.
    Heartbeat {
        first: Duration,
        every: Duration,
    },
    /// An event for the model.
    Dispatch {
        name: String,
        data: Box<RawValue>,
    },
    /// READY (a new session) or RESUMED: the connection works, reconnect
    /// delays start over.
    Established {
        resumed: bool,
    },
}

#[derive(Debug, PartialEq)]
struct Resumable {
    session_id: String,
    /// The host READY says to resume on.
    host: String,
}

/// The session as it carries over from one connection to the next.
#[derive(Default)]
pub struct Gateway {
    seq: Option<u64>,
    resumable: Option<Resumable>,
    awaiting_ack: bool,
}

/// The host of a `wss://host[/...]` URL.
fn host_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("wss://")?;
    let host = rest.split(['/', '?']).next()?;
    (!host.is_empty()).then_some(host)
}

/// What a close code means, as the gateway documents them.
pub fn on_close(code: u16) -> End {
    match code {
        4004 => End::AuthenticationFailed,
        // Invalid shard, sharding required, invalid version, invalid or
        // disallowed intents: nothing this client does differently next time.
        4010..=4014 => End::Refused(code),
        // A normal close, an invalid sequence or a timed-out session ends the
        // session itself.
        1000 | 1001 | 4007 | 4009 => End::Identify,
        _ => End::Resume,
    }
}

impl Gateway {
    /// The host to connect to: the one READY named for resuming, if any.
    pub fn host(&self) -> &str {
        self.resumable.as_ref().map_or(HOST, |r| r.host.as_str())
    }

    /// Forgets the session, so the next connection identifies.
    pub fn forget(&mut self) {
        self.resumable = None;
        self.seq = None;
    }

    fn heartbeat_payload(&self) -> String {
        serde_json::json!({ "op": op::HEARTBEAT, "d": self.seq }).to_string()
    }

    /// The heartbeat to send now, or [`End::Resume`] when the last one went
    /// unanswered: the connection is dead even if the socket looks open.
    pub fn heartbeat(&mut self) -> Result<String, End> {
        if std::mem::replace(&mut self.awaiting_ack, true) {
            return Err(End::Resume);
        }
        Ok(self.heartbeat_payload())
    }

    /// Identify, or resume when a session is there to resume.
    fn greeting(&self, token: &Token, properties: &serde_json::Value) -> String {
        match &self.resumable {
            Some(resumable) => serde_json::json!({
                "op": op::RESUME,
                "d": {
                    "token": token.expose(),
                    "session_id": resumable.session_id,
                    "seq": self.seq,
                },
            }),
            None => serde_json::json!({
                "op": op::IDENTIFY,
                "d": {
                    "token": token.expose(),
                    "capabilities": CAPABILITIES,
                    "properties": properties,
                    // "unknown" keeps the status the person last chose.
                    "presence": { "status": "unknown", "since": 0, "activities": [], "afk": false },
                    "compress": false,
                    "client_state": { "guild_versions": {} },
                },
            }),
        }
        .to_string()
    }

    /// Reads one text frame. `jitter` (0 to 1) spreads first heartbeats, as
    /// the gateway asks.
    pub fn on_frame(
        &mut self,
        text: &str,
        token: &Token,
        properties: &serde_json::Value,
        jitter: f64,
    ) -> Result<Vec<Step>, End> {
        let frame: Frame<'_> = serde_json::from_str(text).map_err(|_| End::Resume)?;
        if let Some(seq) = frame.s {
            self.seq = Some(seq);
        }
        Ok(match frame.op {
            op::HELLO => {
                let hello: Hello = frame
                    .d
                    .and_then(|d| serde_json::from_str(d.get()).ok())
                    .ok_or(End::Resume)?;
                if hello.heartbeat_interval == 0 {
                    return Err(End::Resume);
                }
                let every = Duration::from_millis(hello.heartbeat_interval);
                self.awaiting_ack = false;
                vec![
                    Step::Heartbeat {
                        first: every.mul_f64(jitter.clamp(0.0, 1.0)),
                        every,
                    },
                    Step::Send(self.greeting(token, properties)),
                ]
            }
            op::HEARTBEAT => vec![Step::Send(self.heartbeat_payload())],
            op::HEARTBEAT_ACK => {
                self.awaiting_ack = false;
                Vec::new()
            }
            op::RECONNECT => return Err(End::Resume),
            op::INVALID_SESSION => {
                let resumable = frame.d.is_some_and(|d| d.get() == "true");
                if !resumable {
                    self.forget();
                }
                return Err(if resumable {
                    End::Resume
                } else {
                    End::Identify
                });
            }
            op::DISPATCH => {
                let (Some(name), Some(data)) = (frame.t, frame.d) else {
                    return Ok(Vec::new());
                };
                let mut steps = Vec::new();
                match name {
                    "READY" => {
                        let ready: ReadySession =
                            serde_json::from_str(data.get()).map_err(|_| End::Identify)?;
                        self.resumable = host_of(&ready.resume_gateway_url).map(|host| Resumable {
                            session_id: ready.session_id,
                            host: host.to_owned(),
                        });
                        steps.push(Step::Established { resumed: false });
                    }
                    "RESUMED" => steps.push(Step::Established { resumed: true }),
                    _ => {}
                }
                steps.push(Step::Dispatch {
                    name: name.to_owned(),
                    data: data.to_owned(),
                });
                steps
            }
            _ => Vec::new(),
        })
    }
}

/// One connection, until it ends. `dispatch` receives every event and
/// `established` is told of READY and RESUMED. Dropping the future (the
/// person logged out) closes the socket.
pub async fn connect(
    gateway: &mut Gateway,
    token: &Token,
    properties: &serde_json::Value,
    dispatch: &mut (dyn FnMut(&str, &RawValue) + Send),
    established: &mut (dyn FnMut() + Send),
) -> End {
    let host = gateway.host().to_owned();
    let connecting = crate::websocket::connect(
        &host,
        PATH,
        &[
            ("Origin", "https://discord.com"),
            ("User-Agent", crate::api::USER_AGENT),
        ],
    );
    let mut socket = match tokio::time::timeout(HELLO_TIMEOUT, connecting).await {
        Ok(Ok(socket)) => socket,
        Ok(Err(error)) => {
            // Connection errors name hosts and statuses, nothing private.
            log::info!("gateway connection failed: {error}");
            return End::Resume;
        }
        Err(_) => {
            log::info!("gateway connection timed out");
            return End::Resume;
        }
    };

    // Replaced at hello; until then only the hello deadline runs.
    let mut heartbeat = tokio::time::interval(Duration::from_secs(3600));
    heartbeat.reset();
    let hello = tokio::time::sleep(HELLO_TIMEOUT);
    tokio::pin!(hello);
    let mut greeted = false;

    loop {
        let steps = tokio::select! {
            frame = socket.next() => {
                let Some(Ok(frame)) = frame else {
                    // Dropped without a close frame: the session survives.
                    return End::Resume;
                };
                if let Some((code, _)) = frame.as_close() {
                    let code = u16::from(code);
                    log::info!("gateway closed with code {code}");
                    return on_close(code);
                }
                let Some(text) = frame.as_text() else {
                    continue;
                };
                match gateway.on_frame(text, token, properties, rand_jitter()) {
                    Ok(steps) => steps,
                    Err(end) => return end,
                }
            }
            _ = heartbeat.tick() => match gateway.heartbeat() {
                Ok(beat) => vec![Step::Send(beat)],
                Err(end) => {
                    log::info!("gateway stopped acknowledging heartbeats");
                    return end;
                }
            },
            _ = &mut hello, if !greeted => {
                log::info!("gateway sent no hello");
                return End::Resume;
            }
        };
        for step in steps {
            match step {
                Step::Send(text) => {
                    if socket
                        .send(tokio_websockets::Message::text(text))
                        .await
                        .is_err()
                    {
                        return End::Resume;
                    }
                }
                Step::Heartbeat { first, every } => {
                    greeted = true;
                    heartbeat =
                        tokio::time::interval_at(tokio::time::Instant::now() + first, every);
                }
                Step::Established { resumed } => {
                    log::info!(
                        "gateway session {}",
                        if resumed { "resumed" } else { "started" }
                    );
                    established();
                }
                Step::Dispatch { name, data } => dispatch(&name, &data),
            }
        }
    }
}

fn rand_jitter() -> f64 {
    use rsa::rand_core::RngCore as _;
    f64::from(rsa::rand_core::OsRng.next_u32()) / f64::from(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> Token {
        Token::new("token".into())
    }

    fn frame(gateway: &mut Gateway, text: &str) -> Result<Vec<Step>, End> {
        gateway.on_frame(text, &token(), &serde_json::json!({ "os": "Linux" }), 0.5)
    }

    fn sent(steps: &[Step]) -> Vec<serde_json::Value> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Send(text) => Some(serde_json::from_str(text).unwrap()),
                _ => None,
            })
            .collect()
    }

    const HELLO: &str =
        r#"{"op":10,"d":{"heartbeat_interval":41250,"_trace":["x"]},"s":null,"t":null}"#;
    const READY: &str = r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","resume_gateway_url":"wss://gateway-us-east1-b.discord.gg","v":9}}"#;

    #[test]
    fn hello_identifies_with_the_web_clients_capabilities() {
        let mut gateway = Gateway::default();
        let steps = frame(&mut gateway, HELLO).unwrap();
        assert!(matches!(
            steps[0],
            Step::Heartbeat { first, every }
                if first == Duration::from_millis(20625) && every == Duration::from_millis(41250)
        ));
        let identify = &sent(&steps)[0];
        assert_eq!(identify["op"], 2);
        assert_eq!(identify["d"]["token"], "token");
        assert_eq!(identify["d"]["capabilities"], CAPABILITIES);
        assert_eq!(identify["d"]["properties"]["os"], "Linux");
        assert_eq!(
            identify["d"]["client_state"],
            serde_json::json!({ "guild_versions": {} })
        );
    }

    #[test]
    fn ready_makes_the_session_resumable_on_its_host() {
        let mut gateway = Gateway::default();
        frame(&mut gateway, HELLO).unwrap();
        let steps = frame(&mut gateway, READY).unwrap();
        assert!(matches!(steps[0], Step::Established { resumed: false }));
        assert!(matches!(&steps[1], Step::Dispatch { name, .. } if name == "READY"));
        assert_eq!(gateway.host(), "gateway-us-east1-b.discord.gg");

        frame(&mut gateway, r#"{"op":0,"s":7,"t":"TYPING_START","d":{}}"#).unwrap();
        let resume = &sent(&frame(&mut gateway, HELLO).unwrap())[0];
        assert_eq!(resume["op"], 6);
        assert_eq!(resume["d"]["session_id"], "abc");
        assert_eq!(resume["d"]["seq"], 7);
    }

    #[test]
    fn heartbeats_carry_the_last_sequence() {
        let mut gateway = Gateway::default();
        frame(&mut gateway, HELLO).unwrap();
        frame(&mut gateway, READY).unwrap();
        let beat: serde_json::Value = serde_json::from_str(&gateway.heartbeat().unwrap()).unwrap();
        assert_eq!(beat, serde_json::json!({ "op": 1, "d": 1 }));
    }

    #[test]
    fn a_missed_ack_ends_the_connection_but_keeps_the_session() {
        let mut gateway = Gateway::default();
        frame(&mut gateway, HELLO).unwrap();
        gateway.heartbeat().unwrap();
        frame(&mut gateway, r#"{"op":11,"d":null,"s":null,"t":null}"#).unwrap();
        gateway.heartbeat().unwrap();
        assert_eq!(gateway.heartbeat(), Err(End::Resume));
    }

    #[test]
    fn the_server_can_ask_for_a_heartbeat_now() {
        let mut gateway = Gateway::default();
        let steps = frame(&mut gateway, r#"{"op":1,"d":null,"s":null,"t":null}"#).unwrap();
        assert_eq!(sent(&steps)[0]["op"], 1);
    }

    #[test]
    fn reconnect_and_invalid_session() {
        let mut gateway = Gateway::default();
        frame(&mut gateway, READY).unwrap();
        assert_eq!(
            frame(&mut gateway, r#"{"op":7,"d":null}"#).unwrap_err(),
            End::Resume
        );
        assert_eq!(
            frame(&mut gateway, r#"{"op":9,"d":true}"#).unwrap_err(),
            End::Resume
        );
        assert_eq!(gateway.host(), "gateway-us-east1-b.discord.gg");
        assert_eq!(
            frame(&mut gateway, r#"{"op":9,"d":false}"#).unwrap_err(),
            End::Identify
        );
        assert_eq!(gateway.host(), HOST);
        assert_eq!(sent(&frame(&mut gateway, HELLO).unwrap())[0]["op"], 2);
    }

    #[test]
    fn a_zero_heartbeat_interval_is_refused() {
        let mut gateway = Gateway::default();
        let hello = r#"{"op":10,"d":{"heartbeat_interval":0}}"#;
        assert_eq!(frame(&mut gateway, hello).unwrap_err(), End::Resume);
    }

    #[test]
    fn close_codes() {
        assert_eq!(on_close(4004), End::AuthenticationFailed);
        assert_eq!(on_close(4013), End::Refused(4013));
        assert_eq!(on_close(4009), End::Identify);
        assert_eq!(on_close(1000), End::Identify);
        assert_eq!(on_close(4000), End::Resume);
        assert_eq!(on_close(1006), End::Resume);
    }

    #[test]
    fn hosts_from_resume_urls() {
        assert_eq!(
            host_of("wss://gateway-us-east1-b.discord.gg"),
            Some("gateway-us-east1-b.discord.gg")
        );
        assert_eq!(host_of("wss://g.discord.gg/?v=9"), Some("g.discord.gg"));
        assert_eq!(host_of("https://x"), None);
        assert_eq!(host_of("wss://"), None);
    }
}
