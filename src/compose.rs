//! What the composer sends: the rules the official client applies to a draft
//! before it leaves.

/// The longest message an account without Nitro may send, counted as the
/// official client counts it: UTF-16 code units (`textValue.length`).
pub const MAX_LENGTH: usize = 2000;

/// The counter shows once this few characters remain: a tenth of the limit,
/// as the official client does.
const COUNTER_FROM: i64 = (MAX_LENGTH / 10) as i64;

/// Characters left before the limit; negative past it.
pub fn remaining(draft: &str) -> i64 {
    MAX_LENGTH as i64 - draft.encode_utf16().count() as i64
}

/// What the counter under the draft shows, when it shows.
pub fn counter(draft: &str) -> Option<i64> {
    Some(remaining(draft)).filter(|&left| left <= COUNTER_FROM)
}

/// Why a draft stays in the composer.
#[derive(Debug, PartialEq)]
pub enum Unsent {
    Blank,
    /// Past the limit: the official client offers Nitro instead.
    TooLong,
    /// Something the official client does rather than sends (a command,
    /// `s/old/new`, `+:emoji:`) that fastcord does not do yet: the words
    /// to show.
    Unsupported(String),
}

/// The built-in commands the web client runs as text, and the face each
/// one adds after the message (its `execute`: `${message} face`, trimmed).
const FACES: [(&str, &str); 3] = [
    ("shrug", "¯\\_(ツ)_/¯"),
    ("tableflip", "(╯°□°)╯︵ ┻━┻"),
    ("unflip", "┬─┬ノ( º _ ºノ)"),
];

/// The message a draft sends. Surrounding whitespace goes, as Discord drops
/// it anyway, `/shrug`, `/tableflip`, `/unflip` and `/me` become the text
/// the web client makes of them, and shortcodes become emoji.
pub fn prepare(draft: &str) -> Result<String, Unsent> {
    let text = draft.trim();
    if text.is_empty() {
        return Err(Unsent::Blank);
    }
    if remaining(draft) < 0 {
        return Err(Unsent::TooLong);
    }
    let text = command(text)?;
    if text.is_empty() {
        return Err(Unsent::Blank);
    }
    Ok(shortcodes(&text))
}

/// A draft the web client would act on instead of sending, as it would
/// send it, or why it stays. Any `/command` but the four the web client
/// turns into text stays: posted as text it could ping (`/msg @Sam…`).
fn command(text: &str) -> Result<String, Unsent> {
    let unsupported = |what: &str| Err(Unsent::Unsupported(what.to_owned()));
    if text.starts_with("s/") {
        return unsupported("Editing with s/old/new is not available in fastcord yet.");
    }
    if text == "+" || text.starts_with("+:") {
        return unsupported("Reacting with +:emoji: is not available in fastcord yet.");
    }
    let Some(command) = text.strip_prefix('/') else {
        return Ok(text.to_owned());
    };
    let (name, message) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    // `/home/dylan` names no command: text.
    let named = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-'));
    if !named {
        return Ok(text.to_owned());
    }
    let message = message.trim();
    if let Some((_, face)) = FACES.iter().find(|(n, _)| *n == name) {
        return Ok(format!("{message} {face}").trim().to_owned());
    }
    match name {
        // `_${message}_`; without a message there is nothing to send.
        "me" if message.is_empty() => Ok(String::new()),
        "me" => Ok(format!("_{message}_")),
        // Built-in or a bot's (`/ban`, `/msg`, `/remind`…): the web client
        // runs them, it never posts them. `\/text` sends text that starts
        // with a slash: Discord shows the escaped slash as a slash.
        _ => unsupported(SLASH),
    }
}

/// What the composer says of a slash command it keeps.
pub const SLASH: &str = "Slash commands aren't supported yet. Start with \\/ to send a slash.";

/// Discord's skin tone shortcodes, `:skin-tone-1:` (lightest) to `-5:`,
/// as they follow an emoji.
fn skin_tone(rest: &str) -> Option<emojis::SkinTone> {
    let mut tail = rest.strip_prefix(":skin-tone-")?.chars();
    let digit = tail.next()?;
    if tail.next() != Some(':') {
        return None;
    }
    Some(match digit {
        '1' => emojis::SkinTone::Light,
        '2' => emojis::SkinTone::MediumLight,
        '3' => emojis::SkinTone::Medium,
        '4' => emojis::SkinTone::MediumDark,
        '5' => emojis::SkinTone::Dark,
        _ => return None,
    })
}

const SKIN_TONE: usize = ":skin-tone-1:".len();

/// `:smile:` → 😄 for every shortcode the emoji table knows, as the official
/// client converts them on send; `:thumbsup::skin-tone-2:` takes the tone
/// when the emoji has tones, or stays as typed. Unknown names stay as
/// typed, and code (`` `inline` `` or fenced) and Discord's own markup
/// (`<:custom:id>`, `<t:…:R>`, `<@id>`, links in `<>`) are left alone.
pub fn shortcodes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find([':', '`', '<']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if rest.starts_with('<') {
            // Markup runs to its `>`, without spaces; anything else is text.
            let end = rest
                .find(['>', ' ', '\n'])
                .filter(|&end| rest[end..].starts_with('>'))
                .map_or(1, |end| end + 1);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        let fence = if rest.starts_with("```") { "```" } else { "`" };
        if rest.starts_with('`') {
            // Code runs to its closing fence; an unclosed one is plain text.
            let end = rest[fence.len()..]
                .find(fence)
                .map_or(fence.len(), |end| 2 * fence.len() + end);
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        let name = rest[1..].find(':').map(|end| &rest[1..1 + end]);
        let emoji = name
            .filter(|name| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-'))
            })
            .and_then(emojis::get_by_shortcode);
        let (Some(name), Some(emoji)) = (name, emoji) else {
            // The colon may open the next shortcode: move past it alone.
            out.push(':');
            rest = &rest[1..];
            continue;
        };
        let after = &rest[name.len() + 2..];
        match skin_tone(after) {
            Some(tone) => match emoji.with_skin_tone(tone) {
                Some(toned) => {
                    out.push_str(toned.as_str());
                    rest = &after[SKIN_TONE..];
                }
                // No tones for this one: both stay as typed.
                None => {
                    out.push_str(&rest[..name.len() + 2 + SKIN_TONE]);
                    rest = &after[SKIN_TONE..];
                }
            },
            None => {
                out.push_str(emoji.as_str());
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_drafts_send_nothing() {
        assert_eq!(prepare(""), Err(Unsent::Blank));
        assert_eq!(prepare("  \n\t "), Err(Unsent::Blank));
        assert_eq!(prepare("\n  salut \n").as_deref(), Ok("salut"));
        assert_eq!(prepare("a\nb").as_deref(), Ok("a\nb"));
    }

    #[test]
    fn the_limit_counts_utf16_units_as_the_client_does() {
        let full = "a".repeat(MAX_LENGTH);
        assert_eq!(remaining(&full), 0);
        assert!(prepare(&full).is_ok());
        assert_eq!(prepare(&format!("{full}b")), Err(Unsent::TooLong));
        // An emoji outside the BMP is two units.
        assert_eq!(remaining("😄"), MAX_LENGTH as i64 - 2);
    }

    #[test]
    fn the_counter_shows_near_the_limit() {
        assert_eq!(counter("salut"), None);
        assert_eq!(counter(&"a".repeat(1799)), None);
        assert_eq!(counter(&"a".repeat(1800)), Some(200));
        assert_eq!(counter(&"a".repeat(2003)), Some(-3));
    }

    #[test]
    fn shortcodes_become_emoji() {
        assert_eq!(shortcodes(":smile: ok :+1:"), "😄 ok 👍");
        assert_eq!(shortcodes("à 12:30:smile:"), "à 12:30😄");
        assert_eq!(shortcodes(":not_an_emoji: : ::"), ":not_an_emoji: : ::");
        assert_eq!(prepare(" :tada: ").as_deref(), Ok("🎉"));
    }

    #[test]
    fn skin_tones_follow_their_emoji() {
        assert_eq!(shortcodes(":thumbsup::skin-tone-2:"), "👍🏼");
        assert_eq!(shortcodes(":wave::skin-tone-5: hi"), "👋🏿 hi");
        // An emoji without tones keeps both as typed.
        assert_eq!(shortcodes(":tada::skin-tone-2:"), ":tada::skin-tone-2:");
        assert_eq!(shortcodes(":thumbsup::skin-tone-9:"), "👍:skin-tone-9:");
    }

    #[test]
    fn code_and_markup_keep_their_colons() {
        assert_eq!(shortcodes("`:smile:` :smile:"), "`:smile:` 😄");
        assert_eq!(
            shortcodes("```\nlet a = :smile:;\n```:smile:"),
            "```\nlet a = :smile:;\n```😄"
        );
        // An unclosed backtick is only a backtick.
        assert_eq!(shortcodes("`:smile:"), "`😄");
        // Custom emoji, timestamps (`:1234:` is a shortcode), mentions.
        for markup in [
            "<:smile:123>",
            "<a:tada:45>",
            "<t:1234:R>",
            "<@1>",
            "<https://a.b/:smile:>",
        ] {
            assert_eq!(shortcodes(markup), markup);
        }
        assert_eq!(shortcodes("1 < 2 :smile:"), "1 < 2 😄");
    }

    #[test]
    fn built_in_commands_send_what_the_web_client_makes_of_them() {
        assert_eq!(prepare("/shrug").as_deref(), Ok("¯\\_(ツ)_/¯"));
        assert_eq!(prepare("/shrug bof").as_deref(), Ok("bof ¯\\_(ツ)_/¯"));
        assert_eq!(prepare("/tableflip").as_deref(), Ok("(╯°□°)╯︵ ┻━┻"));
        assert_eq!(prepare("/unflip ok").as_deref(), Ok("ok ┬─┬ノ( º _ ºノ)"));
        assert_eq!(prepare("/me danse").as_deref(), Ok("_danse_"));
        assert_eq!(prepare("/me"), Err(Unsent::Blank));
        // Not commands the web client knows: text.
        assert_eq!(prepare("/home/dylan").as_deref(), Ok("/home/dylan"));
        assert_eq!(prepare("+1 pour moi").as_deref(), Ok("+1 pour moi"));
    }

    #[test]
    fn what_the_web_client_does_instead_of_sending_stays() {
        for draft in [
            "/nick Dyl",
            "/tts bonjour",
            "/gif chat",
            "/spoiler x",
            "/msg @Sam secret",
            "/ban",
            "/timeout sam 10m",
            "/thread idée",
            "/giphy chat",
            "/remind-me demain",
            "s/foo/bar",
            "+:tada:",
            "+",
        ] {
            assert!(
                matches!(prepare(draft), Err(Unsent::Unsupported(_))),
                "{draft}"
            );
        }
        assert_eq!(
            prepare("/msg @Sam secret"),
            Err(Unsent::Unsupported(SLASH.into()))
        );
        // An escaped slash is text, and Discord shows it as a slash.
        assert_eq!(
            prepare("\\/ban est une commande").as_deref(),
            Ok("\\/ban est une commande")
        );
        assert_eq!(prepare("/home/dylan").as_deref(), Ok("/home/dylan"));
        assert_eq!(prepare("/ est seul").as_deref(), Ok("/ est seul"));
    }
}
