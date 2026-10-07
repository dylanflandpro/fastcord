//! The session token, kept in the system keyring and nowhere else.
//!
//! Every call blocks on the platform's store (Secret Service over D-Bus on
//! Linux), so the backend runs them off its async tasks.

use zeroize::Zeroizing;

const SERVICE: &str = "fastcord";
const ACCOUNT: &str = "discord-token";

/// A Discord user token. It has no `Debug` or `Display` so it cannot reach a
/// log by accident, and its memory is wiped when dropped.
pub struct Token(Zeroizing<String>);

impl Token {
    pub fn new(token: String) -> Self {
        Self(Zeroizing::new(token))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the system keyring is unavailable")]
    Unavailable,
    #[error("the system keyring is locked")]
    Locked,
}

fn native(error: keyring_core::Error) -> Error {
    // Provider errors can carry platform data, even the secret: keep only
    // what kind of failure it was.
    match error {
        keyring_core::Error::NoStorageAccess(_) => Error::Locked,
        _ => Error::Unavailable,
    }
}

#[cfg(target_os = "linux")]
fn entry() -> Result<keyring_core::Entry, Error> {
    use keyring_core::api::CredentialStoreApi as _;
    zbus_secret_service_keyring_store::Store::new()
        .map_err(native)?
        .build(SERVICE, ACCOUNT, None)
        .map_err(native)
}

#[cfg(not(target_os = "linux"))]
fn entry() -> Result<keyring_core::Entry, Error> {
    let _ = (SERVICE, ACCOUNT);
    Err(Error::Unavailable)
}

/// The stored token, `None` when there is none.
pub fn load() -> Result<Option<Token>, Error> {
    match entry()?.get_password() {
        Ok(token) => Ok(Some(Token::new(token))),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(error) => Err(native(error)),
    }
}

pub fn save(token: &Token) -> Result<(), Error> {
    entry()?.set_password(token.expose()).map_err(native)
}

pub fn delete() -> Result<(), Error> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(error) => Err(native(error)),
    }
}
