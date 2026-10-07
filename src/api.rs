//! Discord's HTTP API, the few calls fastcord makes.
//!
//! Discord scores every request for risk and answers a request that does not
//! look like its own client with a captcha fastcord cannot show. So requests
//! carry what the web client sends: a current browser's user agent, the
//! client properties with the live build number, the cookies and the
//! fingerprint Discord hands out before sign-in, and the page they come from.

use crate::acks::{Delivery, MAX_RETRY_WAIT};
use crate::credentials::Token;
use crate::model::{Ack, DISCORD_EPOCH_MS, Id, User};
use crate::remote_auth;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::RequestBuilder;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;

const SITE: &str = "https://discord.com";
const BASE: &str = "https://discord.com/api/v9";

/// Without limits a stalled network (a captive portal, a proxy that stops
/// answering) would leave sign-in waiting forever with nothing to retry.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The Chrome release the user agent names. Keep it current: a browser a
/// year old stands out. Bump it with [`USER_AGENT`].
const BROWSER_VERSION: &str = "155.0.0.0";
/// The same on every install, so it identifies nothing about the person.
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the session is no longer valid")]
    Unauthorized,
    #[error("this account cannot read here")]
    Forbidden,
    #[error("unable to reach Discord")]
    Network,
    #[error("Discord sent something this version cannot read")]
    Protocol,
}

/// What the web client learns before sign-in.
#[derive(Debug, Default)]
struct Web {
    /// Discord's identifier for a signed-out browser, sent until sign-in.
    fingerprint: Option<String>,
    /// The client properties, and `X-Super-Properties` built from them.
    properties: serde_json::Value,
    super_properties: String,
    /// Whether both visits worked. An incomplete context is not kept, so a
    /// network that comes back gets the full one.
    complete: bool,
}

pub struct Api {
    client: reqwest::Client,
    web: OnceCell<Arc<Web>>,
}

#[derive(serde::Deserialize)]
struct EncryptedToken {
    encrypted_token: String,
}

/// Discord's request for a captcha before it answers.
#[derive(Debug, PartialEq, serde::Deserialize)]
pub struct Challenge {
    #[serde(rename = "captcha_key")]
    _reasons: Vec<String>,
    #[serde(rename = "captcha_service")]
    pub service: Option<String>,
    #[serde(rename = "captcha_sitekey")]
    pub sitekey: String,
    #[serde(rename = "captcha_rqdata")]
    pub rqdata: Option<String>,
    #[serde(rename = "captcha_rqtoken")]
    rqtoken: Option<String>,
    #[serde(rename = "captcha_session_id")]
    session_id: Option<String>,
}

/// A solved captcha, sent along with the request Discord asked it for, as
/// the web client does.
pub struct Answer {
    key: String,
    rqtoken: Option<String>,
    session_id: Option<String>,
}

impl Challenge {
    pub fn answer(self, key: String) -> Answer {
        Answer {
            key,
            rqtoken: self.rqtoken,
            session_id: self.session_id,
        }
    }
}

impl Answer {
    fn apply(&self, request: RequestBuilder) -> RequestBuilder {
        let mut request = request.header("X-Captcha-Key", &self.key);
        if let Some(rqtoken) = &self.rqtoken {
            request = request.header("X-Captcha-Rqtoken", rqtoken);
        }
        if let Some(session_id) = &self.session_id {
            request = request.header("X-Captcha-Session-Id", session_id);
        }
        request
    }
}

#[derive(serde::Deserialize)]
struct Experiments {
    fingerprint: Option<String>,
}

/// How many times a rate-limited request is retried, and the longest wait
/// accepted between tries.
const RATE_LIMIT_RETRIES: u32 = 2;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);

#[derive(serde::Deserialize)]
struct RateLimited {
    /// Seconds, fractional.
    retry_after: f64,
}

/// The wait Discord asked for, within reason.
fn retry_after(seconds: f64) -> Duration {
    if seconds.is_finite() && seconds > 0.0 {
        Duration::from_secs_f64(seconds).min(MAX_RETRY_AFTER)
    } else {
        Duration::from_millis(500)
    }
}

/// A user as Discord sends one.
#[derive(Clone, serde::Deserialize)]
pub struct ApiUser {
    #[serde(deserialize_with = "snowflake")]
    pub id: u64,
    username: String,
    global_name: Option<String>,
    /// Discord's flags on the account, such as the spammer one.
    #[serde(default)]
    pub public_flags: Option<u64>,
}

impl From<ApiUser> for User {
    fn from(user: ApiUser) -> Self {
        User {
            id: user.id,
            username: user.username,
            global_name: user.global_name,
        }
    }
}

/// Discord sends snowflakes as strings so JavaScript keeps every digit.
pub fn snowflake<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let text: String = serde::Deserialize::deserialize(deserializer)?;
    text.parse().map_err(serde::de::Error::custom)
}

/// A snowflake that may be missing or null.
pub fn optional_snowflake<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    let text: Option<String> = serde::Deserialize::deserialize(deserializer)?;
    text.map(|text| text.parse().map_err(serde::de::Error::custom))
        .transpose()
}

/// The build number in the site's `GLOBAL_ENV` (`"BUILD_NUMBER":"630444"`).
fn build_number(html: &str) -> Option<u64> {
    let start = html.find("\"BUILD_NUMBER\":\"")? + "\"BUILD_NUMBER\":\"".len();
    let digits = html[start..].split('"').next()?;
    digits.parse().ok()
}

/// The person's language as Discord names it (`fr`, `en-US`), from the
/// usual locale variables.
fn locale() -> String {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .unwrap_or_else(|| "en_US".into());
    discord_locale(&raw)
}

/// `fr_FR.UTF-8` → `fr`; `en_US` → `en-US` and `pt_BR` → `pt-BR`, the
/// regional variants Discord lists; anything else by its language.
fn discord_locale(raw: &str) -> String {
    let tag = raw.split(['.', '@']).next().unwrap_or_default();
    let (language, region) = tag.split_once('_').unwrap_or((tag, ""));
    match (language, region) {
        // The C locale (`C`, `C.UTF-8`, `POSIX`) names no language.
        ("C" | "POSIX", _) => "en-US".into(),
        ("en", "GB") => "en-GB".into(),
        ("en", _) => "en-US".into(),
        ("es", "ES") | ("es", "") => "es-ES".into(),
        ("es", _) => "es-419".into(),
        ("pt", "BR") => "pt-BR".into(),
        ("sv", _) => "sv-SE".into(),
        ("zh", "TW") => "zh-TW".into(),
        ("zh", _) => "zh-CN".into(),
        ("", _) => "en-US".into(),
        (language, _) => language.into(),
    }
}

/// The client properties the web client sends, base64-encoded JSON.
/// The client properties the web client sends: as an object in the
/// gateway's IDENTIFY, base64-encoded in `X-Super-Properties`.
fn client_properties(build_number: Option<u64>, locale: &str) -> serde_json::Value {
    serde_json::json!({
        "os": "Linux",
        "browser": "Chrome",
        "device": "",
        "system_locale": locale,
        "has_client_mods": false,
        "browser_user_agent": USER_AGENT,
        "browser_version": BROWSER_VERSION,
        "os_version": "",
        "referrer": "",
        "referring_domain": "",
        "referrer_current": "",
        "referring_domain_current": "",
        "release_channel": "stable",
        "client_build_number": build_number,
        "client_event_source": null,
    })
}

impl Api {
    pub fn new() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::ORIGIN,
            reqwest::header::HeaderValue::from_static(SITE),
        );
        if let Ok(locale) = reqwest::header::HeaderValue::from_str(&locale()) {
            headers.insert("X-Discord-Locale", locale);
        }
        if let Some(zone) = jiff::tz::TimeZone::system().iana_name()
            && let Ok(zone) = reqwest::header::HeaderValue::from_str(zone)
        {
            headers.insert("X-Discord-Timezone", zone);
        }
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            // Discord's cookies, kept in memory for this run only.
            .cookie_store(true)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("an HTTP client with the bundled TLS roots");
        Self {
            client,
            web: OnceCell::new(),
        }
    }

    /// Visits the sign-in page as a browser would, once per run. A visit
    /// that failed is tried again next time rather than kept.
    async fn web(&self) -> Arc<Web> {
        let kept = self
            .web
            .get_or_try_init(|| async {
                let web = Arc::new(self.visit().await);
                if web.complete { Ok(web) } else { Err(web) }
            })
            .await;
        match kept {
            Ok(web) => web.clone(),
            Err(incomplete) => incomplete,
        }
    }

    /// What the sign-in page and the experiments tell a signed-out browser.
    /// Failures leave the parts out rather than stopping sign-in.
    async fn visit(&self) -> Web {
        let build_number = match self.client.get(format!("{SITE}/login")).send().await {
            Ok(response) => response.text().await.ok().and_then(|h| build_number(&h)),
            Err(_) => None,
        };
        let fingerprint = match self
            .client
            .get(format!("{BASE}/experiments?with_guild_experiments=false"))
            .header(reqwest::header::REFERER, format!("{SITE}/login"))
            .send()
            .await
        {
            Ok(response) => response
                .json::<Experiments>()
                .await
                .ok()
                .and_then(|e| e.fingerprint),
            Err(_) => None,
        };
        let properties = client_properties(build_number, &locale());
        log::debug!(
            "web client context: build number {}, fingerprint {}",
            build_number.map_or("missing".into(), |n| n.to_string()),
            if fingerprint.is_some() {
                "received"
            } else {
                "missing"
            },
        );
        Web {
            super_properties: STANDARD.encode(properties.to_string()),
            properties,
            complete: build_number.is_some() && fingerprint.is_some(),
            fingerprint,
        }
    }

    /// A request as the web client sends it from `page`.
    fn dress(request: RequestBuilder, web: &Web, page: &str) -> RequestBuilder {
        request
            .header("X-Super-Properties", &web.super_properties)
            .header(reqwest::header::REFERER, format!("{SITE}{page}"))
    }

    /// Fetches what the web client knows before sign-in, so the QR session
    /// does not wait for it after the phone approves.
    pub async fn prepare(&self) {
        self.web().await;
    }

    /// The client properties for the gateway's IDENTIFY.
    pub async fn client_properties(&self) -> serde_json::Value {
        self.web().await.properties.clone()
    }

    /// Trades the ticket the phone approved for the token, still sealed with
    /// the session's key. `answer` solves the captcha a previous attempt
    /// asked for.
    pub async fn exchange_ticket(
        &self,
        ticket: &str,
        answer: Option<&Answer>,
    ) -> Result<String, remote_auth::Error> {
        let web = self.web().await;
        let mut request = Self::dress(
            self.client
                .post(format!("{BASE}/users/@me/remote-auth/login"))
                .json(&serde_json::json!({ "ticket": ticket })),
            &web,
            "/login",
        );
        if let Some(fingerprint) = &web.fingerprint {
            request = request.header("X-Fingerprint", fingerprint);
        }
        if let Some(answer) = answer {
            request = answer.apply(request);
        }
        let response = request
            .send()
            .await
            .map_err(|_| remote_auth::Error::Network)?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|_| remote_auth::Error::Network)?;
        if status.is_success() {
            return serde_json::from_slice::<EncryptedToken>(&body)
                .map(|t| t.encrypted_token)
                .map_err(|_| remote_auth::Error::Protocol);
        }
        if let Ok(challenge) = serde_json::from_slice::<Challenge>(&body) {
            log::info!(
                "ticket exchange needs a captcha ({})",
                challenge.service.as_deref().unwrap_or("unknown service")
            );
            return Err(remote_auth::Error::Captcha(Box::new(challenge)));
        }
        log::warn!("ticket exchange refused with HTTP {status}");
        Err(remote_auth::Error::Protocol)
    }

    /// The signed-in account. Also how a stored token is checked.
    pub async fn me(&self, token: &Token) -> Result<User, Error> {
        let web = self.web().await;
        let response = Self::dress(
            self.client
                .get(format!("{BASE}/users/@me"))
                .header(reqwest::header::AUTHORIZATION, token.expose()),
            &web,
            "/channels/@me",
        )
        .send()
        .await
        .map_err(|_| Error::Network)?;
        match response.status() {
            status if status.is_success() => {
                let user: ApiUser = response.json().await.map_err(|_| Error::Protocol)?;
                Ok(User::from(user))
            }
            reqwest::StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
            status => {
                log::warn!("reading the account failed with HTTP {status}");
                Err(Error::Protocol)
            }
        }
    }

    /// A page of a channel's history, newest first as Discord sends it:
    /// the latest messages, or those just before `before`.
    ///
    /// Rate limits are waited out, up to [`RATE_LIMIT_RETRIES`] times and
    /// [`MAX_RETRY_AFTER`] each, as Discord's `retry_after` asks.
    pub async fn messages(
        &self,
        token: &Token,
        channel: Id,
        guild: Option<Id>,
        before: Option<Id>,
    ) -> Result<String, Error> {
        let mut url = format!(
            "{BASE}/channels/{channel}/messages?limit={}",
            crate::events::PAGE
        );
        if let Some(before) = before {
            url.push_str(&format!("&before={before}"));
        }
        let web = self.web().await;
        // The page the web client would be on.
        let page = match guild {
            Some(guild) => format!("/channels/{guild}/{channel}"),
            None => format!("/channels/@me/{channel}"),
        };
        let mut retries = 0;
        let response = loop {
            let response = Self::dress(
                self.client
                    .get(&url)
                    .header(reqwest::header::AUTHORIZATION, token.expose()),
                &web,
                &page,
            )
            .send()
            .await
            .map_err(|_| Error::Network)?;
            if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS
                || retries == RATE_LIMIT_RETRIES
            {
                break response;
            }
            retries += 1;
            let wait = response
                .json::<RateLimited>()
                .await
                .map_or(MAX_RETRY_AFTER, |limited| retry_after(limited.retry_after));
            log::info!("rate limited; retrying in {} ms", wait.as_millis());
            tokio::time::sleep(wait).await;
        };
        match response.status() {
            status if status.is_success() => response.text().await.map_err(|_| Error::Network),
            reqwest::StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
            reqwest::StatusCode::FORBIDDEN => Err(Error::Forbidden),
            status => {
                log::warn!("loading history failed with HTTP {status}");
                Err(Error::Protocol)
            }
        }
    }

    /// Tells Discord I read `ack.channel` up to `ack.message`, as the web
    /// client's read state store does. Never fails: what went wrong says
    /// whether to try again.
    pub async fn ack(&self, token: &Token, ack: &Ack) -> Delivery {
        let web = self.web().await;
        let page = match ack.guild {
            Some(guild) => format!("/channels/{guild}/{}", ack.channel),
            None => format!("/channels/@me/{}", ack.channel),
        };
        let sent = Self::dress(
            self.client
                .post(format!(
                    "{BASE}/channels/{}/messages/{}/ack",
                    ack.channel, ack.message
                ))
                .header(reqwest::header::AUTHORIZATION, token.expose())
                .json(&ack_body(ack, jiff::Timestamp::now())),
            &web,
            &page,
        )
        .send()
        .await;
        let Ok(response) = sent else {
            return Delivery::Retry(None);
        };
        let status = response.status();
        let wait = match status {
            reqwest::StatusCode::TOO_MANY_REQUESTS => response
                .json::<RateLimited>()
                .await
                .ok()
                .map(|limited| limited.retry_after),
            _ => None,
        };
        let delivery = delivery(status.as_u16(), wait);
        if matches!(delivery, Delivery::Dropped | Delivery::Retry(_)) && status.is_client_error() {
            log::info!("ack answered HTTP {status}");
        }
        delivery
    }

    /// Ends the session on Discord's side, so the token stops working even
    /// if a copy survived somewhere.
    pub async fn logout(&self, token: &Token) -> Result<(), Error> {
        let web = self.web().await;
        let response = Self::dress(
            self.client
                .post(format!("{BASE}/auth/logout"))
                .header(reqwest::header::AUTHORIZATION, token.expose())
                .json(&serde_json::json!({ "provider": null, "voip_provider": null })),
            &web,
            "/channels/@me",
        )
        .send()
        .await
        .map_err(|_| Error::Network)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(Error::Protocol)
        }
    }
}

/// What an ack's answer means: 401 ends the session; 403 and 404 (no
/// longer mine to read, or gone) and other refusals drop it; rate limits
/// (after Discord's wait, capped), server errors and the rest come back.
fn delivery(status: u16, retry_after: Option<f64>) -> Delivery {
    match status {
        200..=299 => Delivery::Saved,
        401 => Delivery::Unauthorized,
        429 => Delivery::Retry(Some(
            retry_after
                .filter(|s| s.is_finite() && *s > 0.0)
                .map_or(Duration::from_secs(1), Duration::from_secs_f64)
                .min(MAX_RETRY_WAIT),
        )),
        400..=499 => Delivery::Dropped,
        _ => Delivery::Retry(None),
    }
}

/// Days since Discord's epoch, rounded up, as the web client reports when
/// a channel was last viewed.
fn last_viewed(now: jiff::Timestamp) -> i64 {
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    (now.as_millisecond() - DISCORD_EPOCH_MS + DAY_MS - 1).div_euclid(DAY_MS)
}

/// What the web client posts with an ack: no ack token (Discord ignores
/// them now), the day viewed, and the flags only when they changed.
fn ack_body(ack: &Ack, now: jiff::Timestamp) -> serde_json::Value {
    let mut body = serde_json::json!({ "token": null, "last_viewed": last_viewed(now) });
    if let Some(flags) = ack.flags {
        body["flags"] = flags.into();
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_answers_say_what_comes_next() {
        assert_eq!(delivery(204, None), Delivery::Saved);
        assert_eq!(delivery(401, None), Delivery::Unauthorized);
        assert_eq!(delivery(404, None), Delivery::Dropped);
        assert_eq!(delivery(403, None), Delivery::Dropped);
        assert_eq!(
            delivery(429, Some(2.5)),
            Delivery::Retry(Some(Duration::from_millis(2500)))
        );
        assert_eq!(
            delivery(429, Some(3600.0)),
            Delivery::Retry(Some(MAX_RETRY_WAIT))
        );
        assert_eq!(delivery(502, None), Delivery::Retry(None));
    }

    #[test]
    fn an_ack_sends_what_the_web_client_does() {
        let ack = Ack {
            guild: Some(1),
            channel: 2,
            message: 3,
            flags: None,
            immediate: false,
        };
        // 4297.5 days after 2015-01-01, rounded up as the web client does.
        let now: jiff::Timestamp = "2026-10-07T12:00:00Z".parse().unwrap();
        assert_eq!(
            ack_body(&ack, now),
            serde_json::json!({ "token": null, "last_viewed": 4298 })
        );
        let flagged = Ack {
            flags: Some(1),
            ..ack
        };
        assert_eq!(ack_body(&flagged, now)["flags"], 1);
    }

    #[test]
    fn reads_a_user_with_a_string_snowflake() {
        let user: ApiUser = serde_json::from_str(
            r#"{"id":"852892297661906993","username":"dolfies","global_name":null,"discriminator":"0"}"#,
        )
        .unwrap();
        assert_eq!(user.id, 852892297661906993);
        assert_eq!(user.username, "dolfies");
        assert_eq!(user.global_name, None);
    }

    #[test]
    fn recognises_a_captcha_request() {
        let body = br#"{"captcha_key":["captcha-required"],"captcha_sitekey":"x","captcha_service":"hcaptcha","captcha_rqdata":"d","captcha_rqtoken":"t","captcha_session_id":"s"}"#;
        let challenge = serde_json::from_slice::<Challenge>(body).unwrap();
        assert_eq!(challenge.service.as_deref(), Some("hcaptcha"));
        assert_eq!(challenge.sitekey, "x");
        assert_eq!(challenge.rqdata.as_deref(), Some("d"));
        assert!(serde_json::from_slice::<Challenge>(br#"{"message":"Invalid"}"#).is_err());
    }

    #[test]
    fn an_answer_carries_the_challenges_tokens() {
        let body = br#"{"captcha_key":["captcha-required"],"captcha_sitekey":"x","captcha_rqtoken":"t","captcha_session_id":"s"}"#;
        let answer = serde_json::from_slice::<Challenge>(body)
            .unwrap()
            .answer("solved".into());
        let request = answer
            .apply(reqwest::Client::new().post("https://discord.com/"))
            .build()
            .unwrap();
        let header = |name: &str| request.headers()[name].to_str().unwrap().to_owned();
        assert_eq!(header("X-Captcha-Key"), "solved");
        assert_eq!(header("X-Captcha-Rqtoken"), "t");
        assert_eq!(header("X-Captcha-Session-Id"), "s");
    }

    #[test]
    fn rate_limit_waits_stay_reasonable() {
        assert_eq!(retry_after(1.5), Duration::from_millis(1500));
        assert_eq!(retry_after(3600.0), MAX_RETRY_AFTER);
        assert_eq!(retry_after(-1.0), Duration::from_millis(500));
        assert_eq!(retry_after(f64::NAN), Duration::from_millis(500));
    }

    #[test]
    fn finds_the_build_number_in_the_site() {
        let html = r#"window.GLOBAL_ENV = {"NODE_ENV":"production","BUILD_NUMBER":"630444","RELEASE_CHANNEL":"stable"}"#;
        assert_eq!(build_number(html), Some(630444));
        assert_eq!(build_number("<html></html>"), None);
        assert_eq!(build_number(r#""BUILD_NUMBER":"abc""#), None);
    }

    #[test]
    fn user_agent_names_the_browser_version() {
        assert!(USER_AGENT.contains(&format!("Chrome/{BROWSER_VERSION} ")));
    }

    #[test]
    fn client_properties_match_the_web_client() {
        let json = client_properties(Some(630444), "fr");
        assert_eq!(json["client_build_number"], 630444);
        assert_eq!(json["browser"], "Chrome");
        assert_eq!(json["system_locale"], "fr");
        assert_eq!(json["browser_user_agent"], USER_AGENT);
    }

    #[test]
    fn locales_follow_discords_names() {
        assert_eq!(discord_locale("fr_FR.UTF-8"), "fr");
        assert_eq!(discord_locale("en_US.UTF-8"), "en-US");
        assert_eq!(discord_locale("en_GB"), "en-GB");
        assert_eq!(discord_locale("pt_BR.UTF-8"), "pt-BR");
        assert_eq!(discord_locale("de_DE@euro"), "de");
        assert_eq!(discord_locale(""), "en-US");
        assert_eq!(discord_locale("C.UTF-8"), "en-US");
        assert_eq!(discord_locale("C"), "en-US");
        assert_eq!(discord_locale("POSIX"), "en-US");
    }
}
