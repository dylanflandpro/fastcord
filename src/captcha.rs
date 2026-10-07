//! Solving Discord's captcha through `fastcord-captcha`.
//!
//! The captcha needs a browser engine. It runs in that separate program, so
//! WebKit loads only when Discord asks for a captcha and never into fastcord
//! itself (see `crates/captcha`).

use crate::api::Challenge;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt as _;

const HELPER: &str = "fastcord-captcha";

#[derive(Debug, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("fastcord-captcha is not installed (from the sources: cargo build --workspace)")]
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
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    find(beside, std::env::var_os("PATH"))
}

fn find(beside: Option<PathBuf>, path: Option<OsString>) -> Option<PathBuf> {
    let beside = beside.map(|dir| dir.join(HELPER));
    let on_path = path
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(HELPER));
    beside
        .into_iter()
        .chain(on_path)
        .find(|file| file.is_file())
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
    let mut line = request(challenge);
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|_| Error::Failed)?;
    // stdin stays open while the helper runs: when fastcord exits, even
    // without dropping this future, the pipe closes and the helper closes its
    // window instead of lingering.
    let output = child.wait_with_output().await.map_err(|_| Error::Failed)?;
    drop(stdin);
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

    /// A directory holding a file named like the helper.
    fn directory_with_helper(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fastcord-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(HELPER), b"").unwrap();
        dir
    }

    #[test]
    fn prefers_the_helper_beside_fastcord() {
        let beside = directory_with_helper("beside");
        let on_path = directory_with_helper("path");
        let path = std::env::join_paths([&on_path]).unwrap();
        assert_eq!(
            find(Some(beside.clone()), Some(path)),
            Some(beside.join(HELPER))
        );
    }

    #[test]
    fn finds_the_helper_on_path_without_knowing_where_fastcord_is() {
        let on_path = directory_with_helper("only-path");
        let path = std::env::join_paths([PathBuf::from("/nonexistent"), on_path.clone()]).unwrap();
        assert_eq!(find(None, Some(path)), Some(on_path.join(HELPER)));
        assert_eq!(find(None, None), None);
    }

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
