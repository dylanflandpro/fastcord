//! fastcord-captcha: shows Discord's hCaptcha in a window of its own and
//! prints the answer.
//!
//! fastcord starts this only when Discord asks for a captcha, so WebKit never
//! loads into the client itself. The protocol is one line of JSON on stdin
//! (`{"sitekey": "...", "rqdata": "..."}`) and, once solved, the hCaptcha
//! response on one line of stdout with exit status 0. Closing the window
//! prints nothing and exits with status 1, and so does stdin closing: that
//! means fastcord is gone, and the window should not outlive it.
//!
//! hCaptcha only serves a site key on the domains it belongs to, so the page
//! is loaded with `https://discord.com/` as its address. Nothing is kept: the
//! web view is ephemeral, with no cookies or cache on disk.

use gtk::prelude::*;
use javascriptcore::ValueExt as _;
use std::io::{BufRead as _, Read as _, Write as _};
use webkit2gtk::{UserContentManagerExt as _, WebViewExt as _};

/// The page hCaptcha is told it runs on.
const BASE_URI: &str = "https://discord.com/";
/// The name the page posts the answer under.
const HANDLER: &str = "fastcord";

#[derive(Debug, PartialEq, serde::Deserialize)]
struct Challenge {
    sitekey: String,
    rqdata: Option<String>,
}

/// A value as a JavaScript literal that cannot close the `<script>` it sits in.
fn js_literal(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value)
        .expect("strings serialise")
        .replace("</", "<\\/")
}

/// The captcha page: hCaptcha's widget, with Discord's request data when
/// there is some, posting its answer to [`HANDLER`].
fn page(challenge: &Challenge) -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width">
<style>
  html, body {{ margin: 0; height: 100%; background: #1e1f24; }}
  body {{ display: flex; align-items: center; justify-content: center; }}
</style>
<script>
  function ready() {{
    const id = hcaptcha.render("captcha", {{
      sitekey: {sitekey},
      theme: "dark",
      callback: (answer) => window.webkit.messageHandlers.{HANDLER}.postMessage(answer),
      "expired-callback": () => hcaptcha.reset(id),
    }});
    const rqdata = {rqdata};
    if (rqdata) hcaptcha.setData(id, {{ rqdata }});
  }}
</script>
<script src="https://js.hcaptcha.com/1/api.js?render=explicit&onload=ready" async defer></script>
</head>
<body><div id="captcha"></div></body>
</html>"#,
        sitekey = js_literal(&challenge.sitekey),
        rqdata = js_literal(&challenge.rqdata),
    )
}

fn main() {
    let mut input = String::new();
    let mut stdin = std::io::stdin().lock();
    if stdin.read_line(&mut input).is_err() {
        std::process::exit(2);
    }
    drop(stdin);
    std::thread::spawn(|| {
        // Nothing more is sent: reading returns only once fastcord has
        // closed its end, by exiting or by giving up on the captcha.
        let _ = std::io::stdin().read_to_end(&mut Vec::new());
        std::process::exit(1);
    });
    let Ok(challenge) = serde_json::from_str::<Challenge>(&input) else {
        eprintln!("fastcord-captcha: expected {{\"sitekey\": ..., \"rqdata\": ...}} on stdin");
        std::process::exit(2);
    };
    if gtk::init().is_err() {
        eprintln!("fastcord-captcha: no display to show the captcha on");
        std::process::exit(2);
    }

    let manager = webkit2gtk::UserContentManager::new();
    manager.register_script_message_handler(HANDLER);
    manager.connect_script_message_received(Some(HANDLER), |_, result| {
        let Some(answer) = result.js_value().map(|value| value.to_str()) else {
            return;
        };
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{answer}");
        let _ = stdout.flush();
        std::process::exit(0);
    });
    let view = webkit2gtk::WebView::builder()
        .user_content_manager(&manager)
        .is_ephemeral(true)
        .build();
    view.load_html(&page(&challenge), Some(BASE_URI));

    let window = gtk::Window::new(gtk::WindowType::Toplevel);
    window.set_title("fastcord · Verify you are human");
    window.set_default_size(420, 620);
    window.add(&view);
    window.connect_destroy(|_| std::process::exit(1));
    window.show_all();
    gtk::main();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_challenge() {
        let challenge: Challenge =
            serde_json::from_str(r#"{"sitekey":"key","rqdata":"data"}"#).unwrap();
        assert_eq!(
            challenge,
            Challenge {
                sitekey: "key".into(),
                rqdata: Some("data".into())
            }
        );
        let challenge: Challenge = serde_json::from_str(r#"{"sitekey":"key"}"#).unwrap();
        assert_eq!(challenge.rqdata, None);
    }

    #[test]
    fn the_page_carries_the_challenge_as_literals() {
        let page = page(&Challenge {
            sitekey: "a9b5fb07".into(),
            rqdata: None,
        });
        assert!(page.contains(r#"sitekey: "a9b5fb07","#));
        assert!(page.contains("const rqdata = null;"));
    }

    #[test]
    fn values_cannot_break_out_of_the_script() {
        let page = page(&Challenge {
            sitekey: "x\"</script><script>alert(1)</script>".into(),
            rqdata: Some("y".into()),
        });
        assert!(!page.contains("</script><script>alert(1)"));
        assert!(page.contains(r#"sitekey: "x\"<\/script><script>alert(1)<\/script>","#));
    }
}
