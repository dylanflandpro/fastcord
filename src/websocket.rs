//! Opening a WebSocket with header names spelled as given.
//!
//! The `http` crate lowercases header names, which HTTP allows, but Discord's
//! remote auth gateway answers 403 to `origin:` and 101 to `Origin:`. So the
//! upgrade request is written here by hand, and tokio-websockets takes over
//! the stream once the server has switched protocols.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rsa::rand_core::{OsRng, RngCore as _};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_websockets::{MaybeTlsStream, WebSocketStream};

/// A response head larger than this is not a WebSocket upgrade.
const MAX_HEAD: usize = 16 * 1024;

pub type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("the server refused the upgrade: {0}")]
    Refused(String),
}

/// The upgrade request for `wss://{host}{path}`, with `headers` written
/// exactly as given.
fn upgrade_request(host: &str, path: &str, key: &str, headers: &[(&str, &str)]) -> String {
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request
}

/// The status line of a response head, when it is not `101`.
fn refusal(head: &str) -> Option<String> {
    let status = head.lines().next().unwrap_or_default();
    let code = status.split_whitespace().nth(1);
    (code != Some("101")).then(|| status.to_owned())
}

/// Connects to `wss://{host}{path}` over TLS.
pub async fn connect(host: &str, path: &str, headers: &[(&str, &str)]) -> Result<Socket, Error> {
    let connect = |error: &dyn std::fmt::Display| Error::Connect(error.to_string());
    let tcp = TcpStream::connect((host, 443))
        .await
        .map_err(|e| connect(&e))?;
    let tls = tokio_websockets::Connector::new().map_err(|e| connect(&e))?;
    let mut stream = tls.wrap(host, tcp).await.map_err(|e| connect(&e))?;

    let mut key = [0u8; 16];
    OsRng.fill_bytes(&mut key);
    let request = upgrade_request(host, path, &STANDARD.encode(key), headers);
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| connect(&e))?;

    // Read the head a byte at a time: the first frame may follow it in the
    // same packet, and it belongs to the WebSocket.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > MAX_HEAD {
            return Err(Error::Refused("response head too large".into()));
        }
        head.push(stream.read_u8().await.map_err(|e| connect(&e))?);
    }
    if let Some(status) = refusal(&String::from_utf8_lossy(&head)) {
        return Err(Error::Refused(status));
    }
    Ok(tokio_websockets::ClientBuilder::new().take_over(stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_header_names_as_given() {
        let request = upgrade_request(
            "remote-auth-gateway.discord.gg",
            "/?v=2",
            "dGhlIHNhbXBsZSBub25jZQ==",
            &[("Origin", "https://discord.com")],
        );
        assert_eq!(
            request,
            "GET /?v=2 HTTP/1.1\r\n\
             Host: remote-auth-gateway.discord.gg\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Origin: https://discord.com\r\n\
             \r\n"
        );
    }

    #[test]
    fn accepts_only_switching_protocols() {
        assert_eq!(refusal("HTTP/1.1 101 Switching Protocols\r\n\r\n"), None);
        assert_eq!(
            refusal("HTTP/1.1 403 Forbidden\r\nServer: cloudflare\r\n\r\n"),
            Some("HTTP/1.1 403 Forbidden".into())
        );
        assert_eq!(refusal(""), Some(String::new()));
    }
}
