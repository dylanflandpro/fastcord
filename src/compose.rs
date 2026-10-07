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

/// The message a draft sends, or `None` when there is nothing to send
/// (blank) or too much (past the limit; the official client offers Nitro
/// instead). Surrounding whitespace goes, as Discord drops it anyway, and
/// shortcodes become emoji.
pub fn prepare(draft: &str) -> Option<String> {
    let text = draft.trim();
    if text.is_empty() || remaining(draft) < 0 {
        return None;
    }
    Some(shortcodes(text))
}

/// `:smile:` → 😄 for every shortcode the emoji table knows, as the official
/// client converts them on send. Unknown names stay as typed, and code
/// (`` `inline` `` or fenced) is left alone.
pub fn shortcodes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find([':', '`']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
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
        match (name, emoji) {
            (Some(name), Some(emoji)) => {
                out.push_str(emoji.as_str());
                rest = &rest[name.len() + 2..];
            }
            // The colon may open the next shortcode: move past it alone.
            _ => {
                out.push(':');
                rest = &rest[1..];
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
        assert_eq!(prepare(""), None);
        assert_eq!(prepare("  \n\t "), None);
        assert_eq!(prepare("\n  salut \n").as_deref(), Some("salut"));
        assert_eq!(prepare("a\nb").as_deref(), Some("a\nb"));
    }

    #[test]
    fn the_limit_counts_utf16_units_as_the_client_does() {
        let full = "a".repeat(MAX_LENGTH);
        assert_eq!(remaining(&full), 0);
        assert!(prepare(&full).is_some());
        assert_eq!(prepare(&format!("{full}b")), None);
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
        assert_eq!(prepare(" :tada: ").as_deref(), Some("🎉"));
    }

    #[test]
    fn code_keeps_its_shortcodes() {
        assert_eq!(shortcodes("`:smile:` :smile:"), "`:smile:` 😄");
        assert_eq!(
            shortcodes("```\nlet a = :smile:;\n```:smile:"),
            "```\nlet a = :smile:;\n```😄"
        );
        // An unclosed backtick is only a backtick.
        assert_eq!(shortcodes("`:smile:"), "`😄");
    }
}
