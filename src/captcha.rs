//! Solving Discord's captcha through `fastcord-captcha`.
//!
//! The captcha needs a browser engine. It runs in that separate program, so
//! WebKit loads only when Discord asks for a captcha and never into fastcord
//! itself (see `crates/captcha`).

use crate::api::Challenge;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncWriteExt as _;

const HELPER: &str = "fastcord-captcha";

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("fastcord-captcha is not installed")]
    Missing,
    #[error("the captcha window was closed")]
    Closed,
    #[error("the captcha window failed to open")]
    Failed,
}

/// The helper next to fastcord's own executable (a build or an archive),
/// else the one on `PATH` (a package).
fn helper() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()?
        .parent()
        .map(|dir| dir.join(HELPER));
    beside.filter(|path| path.is_file()).or_else(|| {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(HELPER))
                .find(|path| path.is_file())
        })
    })
}

/// The helper's input: what hCaptcha needs, nothing else.
fn request(challenge: &Challenge) -> String {
    serde_json::json!({
        "sitekey": challenge.sitekey,
        "rqdata": challenge.rqdata,
    })
    .to_string()
}

/// Shows the captcha and waits for the person to solve or close it.
pub async fn solve(challenge: &Challenge) -> Result<String, Error> {
    let helper = helper().ok_or(Error::Missing)?;
    let mut child = tokio::process::Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Error::Failed)?;
    let mut stdin = child.stdin.take().ok_or(Error::Failed)?;
    stdin
        .write_all(request(challenge).as_bytes())
        .await
        .map_err(|_| Error::Failed)?;
    drop(stdin);
    let output = child.wait_with_output().await.map_err(|_| Error::Failed)?;
    match output.status.code() {
        Some(0) => {
            let answer = String::from_utf8(output.stdout).map_err(|_| Error::Failed)?;
            let answer = answer.trim();
            if answer.is_empty() {
                Err(Error::Failed)
            } else {
                Ok(answer.to_owned())
            }
        }
        Some(1) => Err(Error::Closed),
        _ => Err(Error::Failed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sends_the_helper_only_what_hcaptcha_needs() {
        let challenge: Challenge = serde_json::from_str(
            r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"key","captcha_rqdata":"data","captcha_rqtoken":"secret-ish","captcha_session_id":"s"}"#,
        )
        .unwrap();
        let sent: serde_json::Value = serde_json::from_str(&request(&challenge)).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({ "sitekey": "key", "rqdata": "data" })
        );
    }
}
