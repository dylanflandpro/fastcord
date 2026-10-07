//! Discord's HTTP API, the few calls fastcord makes.

use crate::credentials::Token;
use crate::model::User;
use crate::remote_auth;

const BASE: &str = "https://discord.com/api/v9";

/// Discord serves user accounts to its own clients and browsers. A browser's
/// user agent keeps fastcord from standing out; it is the same on every
/// install, so it identifies nothing about the person either.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:143.0) Gecko/20100101 Firefox/143.0";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the session is no longer valid")]
    Unauthorized,
    #[error("unable to reach Discord")]
    Network,
    #[error("Discord sent something this version cannot read")]
    Protocol,
}

#[derive(Clone)]
pub struct Api {
    client: reqwest::Client,
}

#[derive(serde::Deserialize)]
struct EncryptedToken {
    encrypted_token: String,
}

#[derive(serde::Deserialize)]
struct CaptchaRequired {
    #[allow(dead_code)]
    captcha_key: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ApiUser {
    #[serde(deserialize_with = "snowflake")]
    id: u64,
    username: String,
    global_name: Option<String>,
}

/// Discord sends snowflakes as strings so JavaScript keeps every digit.
fn snowflake<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let text: String = serde::Deserialize::deserialize(deserializer)?;
    text.parse().map_err(serde::de::Error::custom)
}

impl Api {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("an HTTP client with the bundled TLS roots");
        Self { client }
    }

    /// Trades the ticket the phone approved for the token, still sealed with
    /// the session's key.
    pub async fn exchange_ticket(&self, ticket: &str) -> Result<String, remote_auth::Error> {
        let response = self
            .client
            .post(format!("{BASE}/users/@me/remote-auth/login"))
            .json(&serde_json::json!({ "ticket": ticket }))
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
        if serde_json::from_slice::<CaptchaRequired>(&body).is_ok() {
            return Err(remote_auth::Error::Captcha);
        }
        log::warn!("ticket exchange refused with HTTP {status}");
        Err(remote_auth::Error::Protocol)
    }

    /// The signed-in account. Also how a stored token is checked.
    pub async fn me(&self, token: &Token) -> Result<User, Error> {
        let response = self
            .client
            .get(format!("{BASE}/users/@me"))
            .header(reqwest::header::AUTHORIZATION, token.expose())
            .send()
            .await
            .map_err(|_| Error::Network)?;
        match response.status() {
            status if status.is_success() => {
                let user: ApiUser = response.json().await.map_err(|_| Error::Protocol)?;
                Ok(User {
                    id: user.id,
                    username: user.username,
                    global_name: user.global_name,
                })
            }
            reqwest::StatusCode::UNAUTHORIZED => Err(Error::Unauthorized),
            status => {
                log::warn!("reading the account failed with HTTP {status}");
                Err(Error::Protocol)
            }
        }
    }

    /// Ends the session on Discord's side, so the token stops working even
    /// if a copy survived somewhere.
    pub async fn logout(&self, token: &Token) -> Result<(), Error> {
        let response = self
            .client
            .post(format!("{BASE}/auth/logout"))
            .header(reqwest::header::AUTHORIZATION, token.expose())
            .json(&serde_json::json!({ "provider": null, "voip_provider": null }))
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let body = br#"{"captcha_key":["captcha-required"],"captcha_sitekey":"x","captcha_service":"hcaptcha"}"#;
        assert!(serde_json::from_slice::<CaptchaRequired>(body).is_ok());
        assert!(serde_json::from_slice::<CaptchaRequired>(br#"{"message":"Invalid"}"#).is_err());
    }
}
