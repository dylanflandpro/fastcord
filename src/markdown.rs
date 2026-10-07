//! Discord's markdown: what a message's text says, as blocks of styled spans.
//! It has no egui types; `ui.rs` lays the result out.
//!
//! The rules follow the official client, which parses with simple-markdown
//! plus Discord's own rules: one pass from left to right, so whatever opens
//! first holds everything up to its end (a spoiler hides the code block
//! inside it); the same delimiters; the same choice when two could apply
//! (`***a***` is italic around bold); and the same wording for mentions it
//! cannot resolve and for timestamps.

use crate::model::{Id, Model, User};
use std::collections::HashMap;

/// A message's text, block by block.
pub fn parse(content: &str) -> Vec<Block> {
    // simple-markdown's own first step.
    let content = content
        .replace("\r\n", "\n")
        .replace(['\r'], "\n")
        .replace('\t', "    ");
    let mut parser = Parser::default();
    let mut sink = Sink::default();
    parser.run(&content, &Style::default(), MESSAGE, &mut sink);
    sink.finish()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// Running text. Line breaks inside it are kept, as Discord keeps them;
    /// an empty one is a blank line between two blocks.
    Text(Vec<Span>),
    /// `# `, `## ` or `### ` at the start of a line: level 1 to 3.
    Heading(u8, Vec<Span>),
    /// `-# ` at the start of a line: small, dim text.
    Subtext(Vec<Span>),
    /// A fenced block. The language is kept but not shown, like Discord
    /// without syntax highlighting. `style` is what surrounds it: a spoiler
    /// or a strike reaches the code inside.
    Code {
        language: Option<String>,
        code: String,
        style: Style,
    },
    /// `> ` lines, or everything after `>>> `. Quotes do not nest.
    Quote(Vec<Block>),
    List(List),
}

#[derive(Clone, Debug, PartialEq)]
pub struct List {
    /// The first number of a numbered list; `None` for bullets.
    pub start: Option<u64>,
    /// Each item's text, then the lists indented under it.
    pub items: Vec<Vec<Block>>,
}

impl List {
    /// What stands before an item: numbers count up from the list's first
    /// whatever the message wrote next, and bullets change shape with depth
    /// (disc, circle, square), as in the official client.
    pub fn marker(&self, index: usize, depth: usize) -> String {
        match self.start {
            Some(start) => format!("{}.", start.saturating_add(index as u64)),
            None => ["•", "◦", "▪"][depth % 3].to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub content: Content,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(String),
    /// `` `code` ``: shown as written, in monospace.
    Code(String),
    Mention(Mention),
    /// A server's emoji, `<:name:id>` or `<a:name:id>` when animated.
    Emoji {
        name: String,
        id: Id,
        animated: bool,
    },
    /// `<t:unix>` or `<t:unix:style>`, shown in the reader's time zone.
    Timestamp {
        at: jiff::Timestamp,
        style: TimestampStyle,
    },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Which spoiler of the message hides the span, counted from zero in
    /// reading order, so each can be revealed on its own.
    pub spoiler: Option<usize>,
    pub link: Option<Link>,
}

/// Where a span leads. Only http and https links are kept.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub url: String,
    /// Written `[text](url)`: the text is not the address, so the interface
    /// shows the address on hover.
    pub masked: bool,
}

/// What activating a stretch of text does.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Open(Link),
    Reveal(usize),
}

impl Style {
    /// Whether the span is behind a spoiler not revealed yet.
    pub fn hidden(&self, revealed: impl Fn(usize) -> bool) -> Option<usize> {
        self.spoiler.filter(|&spoiler| !revealed(spoiler))
    }

    /// A hidden spoiler takes the first click, even over a link inside it,
    /// as in the official client.
    pub fn action(&self, revealed: impl Fn(usize) -> bool) -> Option<Action> {
        match (self.hidden(revealed), &self.link) {
            (Some(spoiler), _) => Some(Action::Reveal(spoiler)),
            (None, Some(link)) => Some(Action::Open(link.clone())),
            (None, None) => None,
        }
    }
}

/// What Return or Space does on a focused paragraph: its one action, when
/// every active span shares it (a styled link is several spans). With two
/// different ones there is no telling which was meant.
pub fn keyboard_action(actions: &[Action]) -> Option<&Action> {
    let first = actions.first()?;
    actions.iter().all(|a| a == first).then_some(first)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mention {
    User(Id),
    Role(Id),
    Channel(Id),
    Everyone,
    Here,
}

/// Names for the ids mentions carry.
pub trait Names {
    fn user(&self, id: Id) -> Option<&str>;
    fn channel(&self, id: Id) -> Option<&str>;
    fn role(&self, id: Id) -> Option<&str>;
}

impl Mention {
    /// What the mention shows. Ids nobody can resolve get the official
    /// client's wording.
    pub fn label(self, names: &dyn Names) -> String {
        match self {
            Self::User(id) => names
                .user(id)
                .map_or_else(|| "@unknown-user".into(), |name| format!("@{name}")),
            Self::Role(id) => names
                .role(id)
                .map_or_else(|| "@deleted-role".into(), |name| format!("@{name}")),
            Self::Channel(id) => names
                .channel(id)
                .map_or_else(|| "#unknown".into(), |name| format!("#{name}")),
            Self::Everyone => "@everyone".into(),
            Self::Here => "@here".into(),
        }
    }
}

/// The names one open channel can mention, gathered once per frame rather
/// than searched for each mention.
pub struct Directory<'a> {
    model: &'a Model,
    users: HashMap<Id, &'a str>,
}

impl<'a> Directory<'a> {
    /// The people the client knows: the model's users (READY's, DM
    /// recipients, authors seen so far), DM recipients and the channel's
    /// authors. The official client also reads each message's `mentions`.
    pub fn new(model: &'a Model, channel: Id) -> Self {
        // The model's users are looked up in place; only the few people
        // the model may not list yet are gathered here.
        let users = model
            .dms
            .iter()
            .flat_map(|dm| &dm.recipients)
            .chain(model.messages(channel).iter().map(|m| &m.author))
            .map(|user| (user.id, user.display_name()))
            .collect();
        Self { model, users }
    }
}

impl Names for Directory<'_> {
    fn user(&self, id: Id) -> Option<&str> {
        self.model
            .users
            .get(&id)
            .map(User::display_name)
            .or_else(|| self.users.get(&id).copied())
    }

    fn channel(&self, id: Id) -> Option<&str> {
        self.model
            .guilds
            .iter()
            .find_map(|guild| guild.channel(id))
            .map(|channel| channel.name.as_str())
    }

    fn role(&self, id: Id) -> Option<&str> {
        self.model
            .guilds
            .iter()
            .flat_map(|guild| &guild.roles)
            .find(|role| role.id == id)
            .map(|role| role.name.as_str())
    }
}

/// The styles of `<t:…:style>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimestampStyle {
    /// `t`: 14:05
    ShortTime,
    /// `T`: 14:05:30
    LongTime,
    /// `d`: 07/10/2026
    ShortDate,
    /// `D`: 7 October 2026
    LongDate,
    /// `f`, the default: 7 October 2026 14:05
    ShortDateTime,
    /// `F`: Wednesday, 7 October 2026 14:05
    LongDateTime,
    /// `R`: 3 hours ago, in 2 days
    Relative,
}

impl TimestampStyle {
    fn from_flag(flag: &str) -> Option<Self> {
        Some(match flag {
            "t" => Self::ShortTime,
            "T" => Self::LongTime,
            "d" => Self::ShortDate,
            "D" => Self::LongDate,
            "f" => Self::ShortDateTime,
            "F" => Self::LongDateTime,
            "R" => Self::Relative,
            _ => return None,
        })
    }

    /// The official client's wording in British English, the locale whose
    /// clock (24-hour, day before month) the rest of the interface uses.
    pub fn label(
        self,
        at: jiff::Timestamp,
        now: jiff::Timestamp,
        tz: &jiff::tz::TimeZone,
    ) -> String {
        let format = match self {
            Self::ShortTime => "%H:%M",
            Self::LongTime => "%H:%M:%S",
            Self::ShortDate => "%d/%m/%Y",
            Self::LongDate => "%-d %B %Y",
            Self::ShortDateTime => "%-d %B %Y %H:%M",
            Self::LongDateTime => "%A, %-d %B %Y %H:%M",
            Self::Relative => return relative(at, now),
        };
        at.to_zoned(tz.clone()).strftime(format).to_string()
    }
}

/// "3 hours ago", "in a day": the official client's relative wording, which
/// rounds with moment.js's thresholds (45 seconds make a minute, 45 minutes
/// an hour, 22 hours a day, 26 days a month, 11 months a year).
fn relative(at: jiff::Timestamp, now: jiff::Timestamp) -> String {
    let difference = (at.as_millisecond() - now.as_millisecond()) as f64 / 1000.0;
    let seconds = difference.abs();
    let minutes = (seconds / 60.0).round();
    let hours = (seconds / 3600.0).round();
    let days = (seconds / 86400.0).round();
    // moment.js's average month: 146097 days make 4800 months.
    let months_exact = seconds / 86400.0 * 4800.0 / 146_097.0;
    let months = months_exact.round();
    let years = (months_exact / 12.0).round();
    let phrase = if seconds.round() < 45.0 {
        "a few seconds".to_owned()
    } else if minutes <= 1.0 {
        "a minute".to_owned()
    } else if minutes < 45.0 {
        format!("{minutes} minutes")
    } else if hours <= 1.0 {
        "an hour".to_owned()
    } else if hours < 22.0 {
        format!("{hours} hours")
    } else if days <= 1.0 {
        "a day".to_owned()
    } else if days < 26.0 {
        format!("{days} days")
    } else if months <= 1.0 {
        "a month".to_owned()
    } else if months < 11.0 {
        format!("{months} months")
    } else if years <= 1.0 {
        "a year".to_owned()
    } else {
        format!("{years} years")
    };
    if difference > 0.0 {
        format!("in {phrase}")
    } else {
        format!("{phrase} ago")
    }
}

/// Which rules apply to a stretch of text.
#[derive(Clone, Copy)]
struct Rules {
    /// Quotes, headings, subtext and lists at the start of a line. Inside a
    /// spoiler or any other inline span they are plain text.
    lines: bool,
    /// Quotes do not nest.
    quotes: bool,
    /// Not inside a heading or a list item, which hold one line of text.
    code_blocks: bool,
}

const MESSAGE: Rules = Rules {
    lines: true,
    quotes: true,
    code_blocks: true,
};

/// How deep bold, italic, underline and strike nest before their markers
/// stay text. Discord sets no limit; this one keeps `*` repeated
/// thousands of times from costing a scan per level.
const MAX_DEPTH: usize = 12;

#[derive(Default)]
struct Parser {
    /// Spoilers seen so far, to number the next one.
    spoilers: usize,
    /// Nested inline spans around the current position.
    depth: usize,
}

/// Collects spans into paragraphs, and paragraphs and blocks into the
/// message.
struct Sink {
    blocks: Vec<Block>,
    spans: Vec<Span>,
    /// Whether the running text began at the start of a line: only then is
    /// a lone line break a blank line rather than the end of the line a
    /// block sat on.
    line_start: bool,
}

impl Default for Sink {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            spans: Vec::new(),
            line_start: true,
        }
    }
}

impl Sink {
    fn push(&mut self, content: Content, style: &Style) {
        self.spans.push(Span {
            content,
            style: style.clone(),
        });
    }

    /// Adds text, joined to the span before when it looks the same.
    fn text(&mut self, text: &str, style: &Style) {
        if let Some(Span {
            content: Content::Text(previous),
            style: previous_style,
        }) = self.spans.last_mut()
            && previous_style == style
        {
            previous.push_str(text);
            return;
        }
        self.push(Content::Text(text.into()), style);
    }

    /// Adds a block; the text after it starts a line unless `inline`.
    fn block(&mut self, block: Block, inline: bool) {
        self.end_paragraph();
        self.blocks.push(block);
        self.line_start = !inline;
    }

    /// Ends the running text before a block. The line break before the
    /// block draws no line of its own, but a blank line still does.
    fn end_paragraph(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        if let Some(Span {
            content: Content::Text(last),
            ..
        }) = self.spans.last_mut()
            && last.ends_with('\n')
        {
            last.pop();
            if last.is_empty() && (self.spans.len() > 1 || !self.line_start) {
                self.spans.pop();
            }
        }
        if self.spans.is_empty() {
            return;
        }
        self.blocks
            .push(Block::Text(std::mem::take(&mut self.spans)));
    }

    fn finish(mut self) -> Vec<Block> {
        self.end_paragraph();
        self.blocks
    }

    /// The spans of text that cannot hold blocks (a heading, a list item).
    fn into_spans(self) -> Vec<Span> {
        self.finish()
            .into_iter()
            .flat_map(|block| match block {
                Block::Text(spans) => spans,
                _ => Vec::new(),
            })
            .collect()
    }
}

/// A line-level construct found at the start of a line.
enum LineBlock<'a> {
    Quote(String),
    Heading(u8, &'a str),
    Subtext(&'a str),
    List(Vec<ListLine<'a>>),
}

struct ListLine<'a> {
    indent: usize,
    number: Option<u64>,
    text: &'a str,
}

/// One stretch of text being read, with what was learnt about it so far,
/// so no marker sends a second scan to the end of the text.
struct Scan<'s> {
    src: &'s str,
    /// Delimiters (by `Delim` index) with no closing one left.
    unclosed: [bool; 4],
    /// The next `>` from some position on, once looked for.
    next_angle: Option<(usize, Option<usize>)>,
    /// Where each `[` and `(` closes, once worked out.
    partners: Option<HashMap<usize, usize>>,
}

#[derive(Clone, Copy)]
enum Delim {
    Bold,
    Underline,
    Strike,
    Spoiler,
}

impl Delim {
    fn marker(self) -> &'static str {
        match self {
            Self::Bold => "**",
            Self::Underline => "__",
            Self::Strike => "~~",
            Self::Spoiler => "||",
        }
    }
}

impl<'s> Scan<'s> {
    fn new(src: &'s str) -> Self {
        Self {
            src,
            unclosed: [false; 4],
            next_angle: None,
            partners: None,
        }
    }

    /// `delimited`, remembering when a delimiter never closes again: a
    /// later opener would only find fewer candidates.
    fn delimited(&mut self, at: usize, delim: Delim) -> Option<(&'s str, usize)> {
        let rest = &self.src[at..];
        let marker = delim.marker();
        if self.unclosed[delim as usize] || !rest.starts_with(marker) {
            return None;
        }
        let found = delimited(rest, marker, !matches!(delim, Delim::Spoiler));
        if found.is_none() {
            self.unclosed[delim as usize] = true;
        }
        found
    }

    /// The next `>` at or after `at`.
    fn next_angle(&mut self, at: usize) -> Option<usize> {
        if let Some((from, found)) = self.next_angle
            && from <= at
            && found.is_none_or(|end| end >= at)
        {
            return found;
        }
        let found = self.src[at..].find('>').map(|i| at + i);
        self.next_angle = Some((at, found));
        found
    }

    /// Where the bracket at `at` closes, nested pairs and escapes skipped.
    fn partner(&mut self, at: usize) -> Option<usize> {
        let src = self.src;
        let partners = self.partners.get_or_insert_with(|| {
            let mut partners = HashMap::new();
            let (mut squares, mut rounds) = (Vec::new(), Vec::new());
            let mut escaped = false;
            for (i, c) in src.char_indices() {
                if escaped {
                    escaped = false;
                    continue;
                }
                let (stack, opens) = match c {
                    '\\' => {
                        escaped = true;
                        continue;
                    }
                    '[' => (&mut squares, true),
                    ']' => (&mut squares, false),
                    '(' => (&mut rounds, true),
                    ')' => (&mut rounds, false),
                    _ => continue,
                };
                if opens {
                    stack.push(i);
                } else if let Some(open) = stack.pop() {
                    partners.insert(open, i);
                }
            }
            partners
        });
        partners.get(&at).copied()
    }
}

impl Parser {
    /// Reads `src` left to right into `sink`: at the start of a line the
    /// line rules first, then a code block, then the inline rules, then a
    /// plain character.
    fn run(&mut self, src: &str, style: &Style, rules: Rules, sink: &mut Sink) {
        let mut scan = Scan::new(src);
        let mut i = 0;
        let mut line_start = true;
        while i < src.len() {
            let rest = &src[i..];
            if line_start
                && rules.lines
                && let Some((found, len)) = line_block(rest, rules.quotes)
            {
                sink.end_paragraph();
                self.line_block(found, sink);
                i += len;
                continue;
            }
            if rules.code_blocks
                && style.link.is_none()
                && let Some((language, code, len)) = code_block(rest)
            {
                i += len;
                // The line break after the fence belongs to the block.
                line_start = src[i..].starts_with('\n');
                if line_start {
                    i += 1;
                }
                let block = Block::Code {
                    language: language.map(Into::into),
                    code: code.into(),
                    style: style.clone(),
                };
                sink.block(block, !line_start);
                continue;
            }
            let len = match self.token(&mut scan, i, style, rules, sink) {
                Some(len) => len,
                None => {
                    let len = rest.chars().next().map_or(1, char::len_utf8);
                    sink.text(&rest[..len], style);
                    len
                }
            };
            line_start = rest[..len].ends_with('\n');
            i += len;
        }
    }

    fn line_block(&mut self, found: LineBlock<'_>, sink: &mut Sink) {
        match found {
            LineBlock::Quote(body) => {
                let mut inner = Sink::default();
                let rules = Rules {
                    quotes: false,
                    ..MESSAGE
                };
                self.run(&body, &Style::default(), rules, &mut inner);
                sink.block(Block::Quote(inner.finish()), false);
            }
            LineBlock::Heading(level, text) => {
                let spans = self.line_spans(text);
                sink.block(Block::Heading(level, spans), false);
            }
            LineBlock::Subtext(text) => {
                let spans = self.line_spans(text);
                sink.block(Block::Subtext(spans), false);
            }
            LineBlock::List(lines) => {
                let mut at = 0;
                while at < lines.len() {
                    let list = self.list(&lines, &mut at);
                    sink.block(Block::List(list), false);
                }
            }
        }
    }

    /// One line of text: no line rules, no code blocks.
    fn line_spans(&mut self, text: &str) -> Vec<Span> {
        let mut sink = Sink::default();
        let rules = Rules {
            lines: false,
            quotes: false,
            code_blocks: false,
        };
        self.run(text, &Style::default(), rules, &mut sink);
        sink.into_spans()
    }

    /// One list from `lines[*at]`: items at its indentation, deeper lines
    /// nested under the item above them.
    fn list(&mut self, lines: &[ListLine<'_>], at: &mut usize) -> List {
        let first = &lines[*at];
        let indent = first.indent;
        let numbered = first.number.is_some();
        let mut list = List {
            start: first.number,
            items: Vec::new(),
        };
        while let Some(line) = lines.get(*at) {
            if line.indent > indent
                && let Some(item) = list.items.last_mut()
            {
                let nested = self.list(lines, at);
                item.push(Block::List(nested));
            } else if line.indent == indent && line.number.is_some() == numbered {
                let text = self.line_spans(line.text);
                list.items.push(vec![Block::Text(text)]);
                *at += 1;
            } else {
                break;
            }
        }
        list
    }

    /// Reads `content` styled as `style`, one level deeper.
    fn nested(&mut self, content: &str, style: &Style, rules: Rules, sink: &mut Sink) {
        self.depth += 1;
        let rules = Rules {
            lines: false,
            quotes: false,
            ..rules
        };
        self.run(content, style, rules, sink);
        self.depth -= 1;
    }

    /// The inline rule that matches at `at`, if any: it fills the sink and
    /// returns how much it read.
    fn token(
        &mut self,
        scan: &mut Scan<'_>,
        at: usize,
        style: &Style,
        rules: Rules,
        sink: &mut Sink,
    ) -> Option<usize> {
        let rest = &scan.src[at..];
        let linked = style.link.is_some();
        let nests = self.depth < MAX_DEPTH;
        match rest.as_bytes()[0] {
            b'\\' => {
                // `\*` shows a `*`; a backslash before a letter, a digit or
                // a space stays.
                let c = rest[1..].chars().next()?;
                if c.is_ascii_alphanumeric() || c.is_whitespace() {
                    return None;
                }
                sink.text(&rest[1..1 + c.len_utf8()], style);
                Some(1 + c.len_utf8())
            }
            b'`' => {
                let (code, len) = inline_code(rest)?;
                sink.push(Content::Code(code.into()), style);
                Some(len)
            }
            b'<' => {
                let end = scan.next_angle(at)? - at;
                if !linked && let Some(url) = autolink(&rest[..end]) {
                    let link = Link {
                        url: url.into(),
                        masked: false,
                    };
                    sink.text(url, &with_link(style, link));
                    return Some(end + 1);
                }
                let content = angle_token(&rest[1..end])?;
                sink.push(content, style);
                Some(end + 1)
            }
            b'h' if !linked => {
                let url = bare_url(rest)?;
                let link = Link {
                    url: url.into(),
                    masked: false,
                };
                sink.text(url, &with_link(style, link));
                Some(url.len())
            }
            b'[' if !linked => {
                let close = scan.partner(at)?;
                let open = close + 1;
                if scan.src.as_bytes().get(open) != Some(&b'(') {
                    return None;
                }
                let end = scan.partner(open)?;
                let label = &scan.src[at + 1..close];
                let target = scan.src[open + 1..end].trim();
                let url = target
                    .strip_prefix('<')
                    .and_then(|t| t.strip_suffix('>'))
                    .unwrap_or(target);
                if label.trim().is_empty() || !is_web(url) || url.contains(char::is_whitespace) {
                    return None;
                }
                if deceptive(label, url) {
                    // The text passes for another address: show the real one.
                    let link = Link {
                        url: url.into(),
                        masked: false,
                    };
                    sink.text(url, &with_link(style, link));
                } else {
                    let link = Link {
                        url: url.into(),
                        masked: true,
                    };
                    self.nested(label, &with_link(style, link), rules, sink);
                }
                Some(end + 1 - at)
            }
            b'*' | b'_' if nests => {
                let (kind, content, len) = emphasis(scan, at)?;
                let mut style = style.clone();
                match kind {
                    Emphasis::Italic => style.italic = true,
                    Emphasis::Bold => style.bold = true,
                    Emphasis::Underline => style.underline = true,
                }
                self.nested(content, &style, rules, sink);
                Some(len)
            }
            b'~' if nests => {
                let (content, len) = scan.delimited(at, Delim::Strike)?;
                let style = Style {
                    strike: true,
                    ..style.clone()
                };
                self.nested(content, &style, rules, sink);
                Some(len)
            }
            // Spoilers nest at any depth: their markers must never show
            // what they hide.
            b'|' => {
                let (content, len) = scan.delimited(at, Delim::Spoiler)?;
                let style = Style {
                    spoiler: Some(self.spoilers),
                    ..style.clone()
                };
                self.spoilers += 1;
                self.nested(content, &style, rules, sink);
                Some(len)
            }
            b'@' => {
                let (mention, len) = if rest.starts_with("@everyone") {
                    (Mention::Everyone, "@everyone".len())
                } else if rest.starts_with("@here") {
                    (Mention::Here, "@here".len())
                } else {
                    return None;
                };
                sink.push(Content::Mention(mention), style);
                Some(len)
            }
            _ => None,
        }
    }
}

fn with_link(style: &Style, link: Link) -> Style {
    Style {
        link: Some(link),
        ..style.clone()
    }
}

/// The quote, heading, subtext or list that starts `rest`, and how much of
/// it the block takes, its last line break included.
fn line_block(rest: &str, quotes: bool) -> Option<(LineBlock<'_>, usize)> {
    let line = rest.split('\n').next().unwrap_or(rest);
    let with_break = (line.len() + 1).min(rest.len());
    let unindented = line.trim_start_matches(' ');
    if quotes {
        if let Some(body) = rest.trim_start_matches(' ').strip_prefix(">>> ") {
            return Some((LineBlock::Quote(body.into()), rest.len()));
        }
        if unindented.starts_with("> ") {
            let mut body = Vec::new();
            let mut len = 0;
            for line in rest.split_inclusive('\n') {
                let Some(text) = line.trim_start_matches(' ').strip_prefix("> ") else {
                    break;
                };
                body.push(text.strip_suffix('\n').unwrap_or(text));
                len += line.len();
            }
            return Some((LineBlock::Quote(body.join("\n")), len));
        }
    }
    if let Some(after) = unindented.strip_prefix("-#") {
        let text = after.strip_prefix([' ', '\t'])?.trim();
        return (!text.is_empty()).then_some((LineBlock::Subtext(text), with_break));
    }
    let hashes = unindented.bytes().take_while(|&b| b == b'#').count();
    if (1..=3).contains(&hashes) {
        let text = unindented[hashes..].strip_prefix([' ', '\t'])?.trim();
        return (!text.is_empty() && !text.starts_with('#'))
            .then_some((LineBlock::Heading(hashes as u8, text), with_break));
    }
    let mut lines = Vec::new();
    let mut len = 0;
    for line in rest.split_inclusive('\n') {
        let Some(item) = list_line(line.strip_suffix('\n').unwrap_or(line)) else {
            break;
        };
        lines.push(item);
        len += line.len();
    }
    (!lines.is_empty()).then_some((LineBlock::List(lines), len))
}

/// `- item`, `* item` or `1. item`, indented by spaces when nested.
fn list_line(line: &str) -> Option<ListLine<'_>> {
    let unindented = line.trim_start_matches(' ');
    let indent = line.len() - unindented.len();
    let (number, after) = if let Some(after) = unindented
        .strip_prefix("- ")
        .or_else(|| unindented.strip_prefix("* "))
    {
        (None, after)
    } else {
        let digits = unindented.bytes().take_while(u8::is_ascii_digit).count();
        let after = unindented.get(digits..)?.strip_prefix(". ")?;
        if !(1..=9).contains(&digits) {
            return None;
        }
        (Some(unindented[..digits].parse().ok()?), after)
    };
    let text = after.trim();
    (!text.is_empty()).then_some(ListLine {
        indent,
        number,
        text,
    })
}

/// A fenced block: ```` ```lang\ncode``` ````. The language line is only one
/// when a line break follows it; blank lines around the code are dropped.
/// Returns the language, the code and the length up to the closing fence.
fn code_block(rest: &str) -> Option<(Option<&str>, &str, usize)> {
    let after = rest.strip_prefix("```")?;
    let name = after
        .bytes()
        .take_while(|&b| b.is_ascii_alphanumeric() || b"_+-.#".contains(&b))
        .count();
    let language = (name > 0 && after[name..].starts_with('\n')).then(|| &after[..name]);
    let fenced = |skip: usize| {
        let from = &rest[3 + skip..];
        let start = 3 + skip + from.len() - from.trim_start_matches('\n').len();
        let body = &rest[start..];
        // At least one character of code before the closing fence.
        let first = body.chars().next().filter(|&c| c != '\n')?;
        let end = body[first.len_utf8()..].find("```")? + first.len_utf8();
        Some((body[..end].trim_end_matches('\n'), start + end + 3))
    };
    if let Some(language) = language
        && let Some((code, len)) = fenced(language.len() + 1)
    {
        return Some((Some(language), code, len));
    }
    let (code, len) = fenced(0)?;
    Some((None, code, len))
}

/// `` `code` `` or ``` ``co`de`` ```: as many backticks close it as opened
/// it. One space against backticks inside is padding and goes.
fn inline_code(rest: &str) -> Option<(&str, usize)> {
    let ticks = rest.bytes().take_while(|&b| b == b'`').count();
    if ticks == 0 {
        return None;
    }
    let fence = &rest[..ticks];
    let body = &rest[ticks..];
    let mut from = body.chars().next()?.len_utf8();
    loop {
        let close = from + body[from..].find(fence)?;
        let after = close + ticks;
        if !body[..close].ends_with('`') && !body[after..].starts_with('`') {
            let mut code = &body[..close];
            if code.starts_with(' ') && code.trim_start_matches(' ').starts_with('`') {
                code = &code[1..];
            }
            if code.ends_with(' ') && code.trim_end_matches(' ').ends_with('`') {
                code = &code[..code.len() - 1];
            }
            return Some((code, ticks + after));
        }
        from = close + 1;
    }
}

#[derive(Clone, Copy)]
enum Emphasis {
    Italic,
    Bold,
    Underline,
}

/// `*italic*`, `_italic_`, `**bold**` or `__underline__`. When several fit,
/// the longest wins and italic wins ties, as simple-markdown weighs them:
/// `***a***` is italic around bold.
fn emphasis<'s>(scan: &mut Scan<'s>, at: usize) -> Option<(Emphasis, &'s str, usize)> {
    let rest = &scan.src[at..];
    let italic = if rest.starts_with('*') {
        star_italic(rest)
    } else {
        underscore_italic(rest)
    };
    let bold = scan.delimited(at, Delim::Bold);
    let underline = scan.delimited(at, Delim::Underline);
    [
        (Emphasis::Italic, italic, 0.2),
        (Emphasis::Bold, bold, 0.1),
        (Emphasis::Underline, underline, 0.0),
    ]
    .into_iter()
    .filter_map(|(kind, found, bonus)| found.map(|(content, len)| (kind, content, len, bonus)))
    .max_by(|a, b| (a.2 as f64 + a.3).total_cmp(&(b.2 as f64 + b.3)))
    .map(|(kind, content, len, _)| (kind, content, len))
}

/// `delim content delim`, the closing one the first after some content.
/// `guard`: a closing delimiter followed by its own character does not
/// close (`**a***` is bold `a*`). Escaped characters never close.
fn delimited<'a>(rest: &'a str, delim: &str, guard: bool) -> Option<(&'a str, usize)> {
    let body = rest.strip_prefix(delim)?;
    let repeat = delim.chars().next()?;
    let mut j = 0;
    loop {
        if j > 0 && body[j..].starts_with(delim) {
            let after = &body[j + delim.len()..];
            if !(guard && after.starts_with(repeat)) {
                return Some((&body[..j], j + 2 * delim.len()));
            }
        }
        let c = body[j..].chars().next()?;
        j += c.len_utf8();
        if c == '\\' {
            j += body[j..].chars().next()?.len_utf8();
        }
    }
}

/// `*italic*`: no space just inside the stars, and `**` pairs inside are
/// left for bold.
fn star_italic(rest: &str) -> Option<(&str, usize)> {
    let body = rest.strip_prefix('*')?;
    if body.starts_with(char::is_whitespace) {
        return None;
    }
    let mut j = 0;
    loop {
        let tail = &body[j..];
        if j > 0 && tail.starts_with('*') && !tail[1..].starts_with('*') {
            return Some((&body[..j], j + 2));
        }
        // One unit: `**`, an escape, or a character with any spaces before.
        let spaces = tail.len() - tail.trim_start().len();
        let unit = &tail[spaces..];
        let len = if unit.starts_with("**") {
            2
        } else if let Some(escaped) = unit.strip_prefix('\\') {
            1 + escaped.chars().next()?.len_utf8()
        } else {
            match unit.chars().next()? {
                '*' => return None,
                c => c.len_utf8(),
            }
        };
        j += spaces + len;
    }
}

/// `_italic_`: closes on a `_` that ends a word, so `snake_case_name` stays
/// as it is.
fn underscore_italic(rest: &str) -> Option<(&str, usize)> {
    let body = rest.strip_prefix('_')?;
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut j = 0;
    loop {
        let tail = &body[j..];
        if j > 0 && tail.starts_with('_') && !tail[1..].starts_with(word) {
            return Some((&body[..j], j + 2));
        }
        j += if tail.starts_with("__") {
            2
        } else if let Some(escaped) = tail.strip_prefix('\\') {
            1 + escaped.chars().next()?.len_utf8()
        } else {
            match tail.chars().next()? {
                '_' => return None,
                c => c.len_utf8(),
            }
        };
    }
}

/// Only web links open: anything else stays text.
fn is_web(url: &str) -> bool {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .is_some_and(|rest| !rest.is_empty())
}

/// `<https://…>` up to its `>`: a link Discord shows without its preview.
fn autolink(token: &str) -> Option<&str> {
    let url = &token[1..];
    (is_web(url) && !url.contains(char::is_whitespace)).then_some(url)
}

/// A bare `https://…` link. Punctuation that ends a sentence is left out,
/// and a closing parenthesis too unless the link opened one.
fn bare_url(rest: &str) -> Option<&str> {
    if !rest.starts_with("http://") && !rest.starts_with("https://") {
        return None;
    }
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '<')
        .unwrap_or(rest.len());
    let mut url = &rest[..end];
    let opened = url.matches('(').count();
    let mut closed = url.matches(')').count();
    loop {
        match url.chars().last() {
            Some('.' | ',' | ':' | ';' | '"' | '\'' | ']' | '!' | '?') => {}
            Some(')') if opened < closed => closed -= 1,
            _ => break,
        }
        url = &url[..url.len() - 1];
    }
    is_web(url).then_some(url)
}

/// Whether a masked link's text passes for an address other than the one
/// it opens, like `[https://bank.com](https://evil.com)`. Such a link is
/// shown as its real address instead.
fn deceptive(label: &str, url: &str) -> bool {
    let shown: String = label
        .chars()
        .filter(|c| !"*_~|`\\<>".contains(*c))
        .collect();
    let shown = shown.trim();
    let looks_like_address =
        shown.contains("http://") || shown.contains("https://") || is_domain(shown);
    let bare = |s: &str| {
        s.strip_prefix("https://")
            .or_else(|| s.strip_prefix("http://"))
            .unwrap_or(s)
            .trim_end_matches('/')
            .to_lowercase()
    };
    looks_like_address && bare(shown) != bare(url)
}

/// `example.com` or `www.example.com/path`: a host whose last label is
/// two letters or more.
fn is_domain(text: &str) -> bool {
    if text.contains(char::is_whitespace) {
        return false;
    }
    let host = text.split('/').next().unwrap_or(text);
    let labels: Vec<&str> = host.split('.').collect();
    labels.len() >= 2
        && labels
            .iter()
            .all(|l| !l.is_empty() && l.chars().all(|c| c.is_alphanumeric() || c == '-'))
        && labels
            .last()
            .is_some_and(|tld| tld.chars().count() >= 2 && tld.chars().all(char::is_alphabetic))
}

fn snowflake(digits: &str) -> Option<Id> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Discord's `<…>` tokens, given what is between the brackets: mentions,
/// server emoji and timestamps.
fn angle_token(inner: &str) -> Option<Content> {
    Some(if let Some(id) = inner.strip_prefix("@&") {
        Content::Mention(Mention::Role(snowflake(id)?))
    } else if let Some(id) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
        Content::Mention(Mention::User(snowflake(id)?))
    } else if let Some(id) = inner.strip_prefix('#') {
        Content::Mention(Mention::Channel(snowflake(id)?))
    } else if let Some(timestamp) = inner.strip_prefix("t:") {
        let (seconds, flag) = timestamp.split_once(':').unwrap_or((timestamp, "f"));
        let unsigned = seconds.strip_prefix('-').unwrap_or(seconds);
        if unsigned.is_empty() || !unsigned.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Content::Timestamp {
            at: jiff::Timestamp::from_second(seconds.parse().ok()?).ok()?,
            style: TimestampStyle::from_flag(flag)?,
        }
    } else {
        let (animated, emoji) = match inner.strip_prefix("a:") {
            Some(emoji) => (true, emoji),
            None => (false, inner.strip_prefix(':')?),
        };
        let (name, id) = emoji.split_once(':')?;
        let valid = name.len() >= 2 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid {
            return None;
        }
        Content::Emoji {
            name: name.into(),
            id: snowflake(id)?,
            animated,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Span {
        styled(s, Style::default())
    }

    fn styled(s: &str, style: Style) -> Span {
        Span {
            content: Content::Text(s.into()),
            style,
        }
    }

    fn bold() -> Style {
        Style {
            bold: true,
            ..Style::default()
        }
    }

    fn italic() -> Style {
        Style {
            italic: true,
            ..Style::default()
        }
    }

    fn spoiler(index: usize) -> Style {
        Style {
            spoiler: Some(index),
            ..Style::default()
        }
    }

    fn linked(url: &str, masked: bool) -> Style {
        Style {
            link: Some(Link {
                url: url.into(),
                masked,
            }),
            ..Style::default()
        }
    }

    fn code(language: Option<&str>, code: &str, style: Style) -> Block {
        Block::Code {
            language: language.map(Into::into),
            code: code.into(),
            style,
        }
    }

    fn paragraph(s: &str) -> Block {
        Block::Text(vec![text(s)])
    }

    /// The spans of a message made of one paragraph.
    fn spans(content: &str) -> Vec<Span> {
        match parse(content).as_slice() {
            [Block::Text(spans)] => spans.clone(),
            other => panic!("not one paragraph: {other:?}"),
        }
    }

    #[test]
    fn plain_text_is_one_span_with_its_line_breaks() {
        assert_eq!(spans("hello\nworld"), [text("hello\nworld")]);
        assert!(parse("").is_empty());
    }

    #[test]
    fn line_endings_and_tabs_are_normalised_first() {
        assert_eq!(spans("a\r\nb\rc"), [text("a\nb\nc")]);
        assert_eq!(spans("a\tb"), [text("a    b")]);
        // A tab indents a nested item like four spaces.
        let blocks = parse("- a\r\n\t- b");
        let [Block::List(list)] = blocks.as_slice() else {
            panic!("not one list: {blocks:?}");
        };
        assert!(matches!(list.items[0][1], Block::List(_)));
    }

    #[test]
    fn bold_italic_underline_strike() {
        assert_eq!(
            spans("a **b** *c* _d_ __e__ ~~f~~"),
            [
                text("a "),
                styled("b", bold()),
                text(" "),
                styled("c", italic()),
                text(" "),
                styled("d", italic()),
                text(" "),
                styled(
                    "e",
                    Style {
                        underline: true,
                        ..Style::default()
                    }
                ),
                text(" "),
                styled(
                    "f",
                    Style {
                        strike: true,
                        ..Style::default()
                    }
                ),
            ]
        );
    }

    #[test]
    fn styles_nest() {
        let bold_italic = Style {
            bold: true,
            italic: true,
            ..Style::default()
        };
        assert_eq!(spans("***both***"), [styled("both", bold_italic.clone())]);
        assert_eq!(
            spans("__*under it*__"),
            [styled(
                "under it",
                Style {
                    underline: true,
                    italic: true,
                    ..Style::default()
                }
            )]
        );
        assert_eq!(
            spans("**bold *and italic***"),
            [styled("bold ", bold()), styled("and italic", bold_italic)]
        );
    }

    #[test]
    fn unclosed_or_spaced_delimiters_stay_text() {
        assert_eq!(spans("2 * 3 * 4"), [text("2 * 3 * 4")]);
        assert_eq!(spans("**open"), [text("**open")]);
        assert_eq!(spans("snake_case_name"), [text("snake_case_name")]);
        assert_eq!(spans("a ~~ b"), [text("a ~~ b")]);
    }

    #[test]
    fn backslash_escapes_markdown() {
        assert_eq!(spans(r"\*not italic\*"), [text("*not italic*")]);
        assert_eq!(spans(r"\<@1>"), [text("<@1>")]);
        // Before a letter the backslash stays.
        assert_eq!(spans(r"C:\Users"), [text(r"C:\Users")]);
        assert_eq!(spans(r"**a\*\*b**"), [styled("a**b", bold())]);
    }

    #[test]
    fn inline_code_is_literal() {
        let code = |s: &str| Span {
            content: Content::Code(s.into()),
            style: Style::default(),
        };
        assert_eq!(
            spans("run `cargo *test*` now"),
            [text("run "), code("cargo *test*"), text(" now")]
        );
        assert_eq!(spans("``a`b``"), [code("a`b")]);
        assert_eq!(spans("`` `tick` ``"), [code("`tick`")]);
        assert_eq!(spans("`open"), [text("`open")]);
    }

    #[test]
    fn spoilers_are_numbered_in_reading_order() {
        assert_eq!(
            spans("||one|| and ||**two**||"),
            [
                styled("one", spoiler(0)),
                text(" and "),
                styled(
                    "two",
                    Style {
                        bold: true,
                        ..spoiler(1)
                    }
                ),
            ]
        );
        // Numbering goes on across blocks.
        let blocks = parse("||a||\n# ||b||");
        assert_eq!(blocks[1], Block::Heading(1, vec![styled("b", spoiler(1))]));
    }

    #[test]
    fn spoilers_hide_code_blocks_inside_them() {
        assert_eq!(
            parse("||```secret```||"),
            [code(None, "secret", spoiler(0))]
        );
        assert_eq!(
            parse("x ||```rust\nsecret\n```||"),
            [paragraph("x "), code(Some("rust"), "secret", spoiler(0)),]
        );
        assert_eq!(
            parse("||before ```a``` after||"),
            [
                Block::Text(vec![styled("before ", spoiler(0))]),
                code(None, "a", spoiler(0)),
                Block::Text(vec![styled(" after", spoiler(0))]),
            ]
        );
    }

    #[test]
    fn spoilers_keep_line_markers_as_hidden_text() {
        for (message, hidden) in [
            ("||a\n# b||", "a\n# b"),
            ("||a\n> b||", "a\n> b"),
            ("||a\n- b||", "a\n- b"),
            ("||a\n-# b||", "a\n-# b"),
        ] {
            assert_eq!(
                parse(message),
                [Block::Text(vec![styled(hidden, spoiler(0))])],
                "{message}"
            );
        }
    }

    #[test]
    fn other_spans_also_hold_what_opens_inside_them() {
        assert_eq!(spans("**a\n- b**"), [styled("a\n- b", bold())]);
        let strike = Style {
            strike: true,
            ..Style::default()
        };
        assert_eq!(parse("~~```a```~~"), [code(None, "a", strike)]);
    }

    #[test]
    fn links_bare_angled_and_masked() {
        let url = "https://example.com/a_b_c";
        assert_eq!(
            spans(&format!("see {url}.")),
            [text("see "), styled(url, linked(url, false)), text(".")]
        );
        assert_eq!(
            spans("<https://example.com>"),
            [styled(
                "https://example.com",
                linked("https://example.com", false)
            )]
        );
        assert_eq!(
            spans("[the **docs**](https://docs.rs)!"),
            [
                styled("the ", linked("https://docs.rs", true)),
                styled(
                    "docs",
                    Style {
                        bold: true,
                        ..linked("https://docs.rs", true)
                    }
                ),
                text("!"),
            ]
        );
    }

    #[test]
    fn link_parentheses_and_schemes() {
        let wiki = "https://en.wikipedia.org/wiki/Rust_(language)";
        assert_eq!(
            spans(&format!("({wiki})")),
            [text("("), styled(wiki, linked(wiki, false)), text(")")]
        );
        assert_eq!(
            spans("[x](javascript:alert(1))"),
            [text("[x](javascript:alert(1))")]
        );
        assert_eq!(spans("ftp://host"), [text("ftp://host")]);
        assert_eq!(spans("https://"), [text("https://")]);
        assert_eq!(spans("[[a]"), [text("[[a]")]);
    }

    #[test]
    fn masked_links_that_pass_for_another_address_show_the_real_one() {
        let evil = "https://evil.com";
        for label in [
            "https://google.com",
            "**google.com**",
            "www.google.com/login",
            "log in at https://google.com",
        ] {
            assert_eq!(
                spans(&format!("[{label}]({evil})")),
                [styled(evil, linked(evil, false))],
                "{label}"
            );
        }
        // The address it says is the one it opens.
        assert_eq!(
            spans("[docs.rs](https://docs.rs/)"),
            [styled("docs.rs", linked("https://docs.rs/", true))]
        );
        // Words with dots are not addresses.
        assert_eq!(
            spans("[v1.2 e.g.](https://x.org)"),
            [styled("v1.2 e.g.", linked("https://x.org", true))]
        );
    }

    #[test]
    fn mentions_emoji_and_timestamps() {
        let token = |content| Span {
            content,
            style: Style::default(),
        };
        assert_eq!(
            spans("<@1><@!2><@&3><#4>@everyone @here"),
            [
                token(Content::Mention(Mention::User(1))),
                token(Content::Mention(Mention::User(2))),
                token(Content::Mention(Mention::Role(3))),
                token(Content::Mention(Mention::Channel(4))),
                token(Content::Mention(Mention::Everyone)),
                text(" "),
                token(Content::Mention(Mention::Here)),
            ]
        );
        assert_eq!(
            spans("<a:party:12>"),
            [token(Content::Emoji {
                name: "party".into(),
                id: 12,
                animated: true
            })]
        );
        assert_eq!(
            spans("<t:1791000000:R><t:0>"),
            [
                token(Content::Timestamp {
                    at: jiff::Timestamp::from_second(1_791_000_000).unwrap(),
                    style: TimestampStyle::Relative,
                }),
                token(Content::Timestamp {
                    at: jiff::Timestamp::UNIX_EPOCH,
                    style: TimestampStyle::ShortDateTime,
                }),
            ]
        );
    }

    #[test]
    fn malformed_tokens_stay_text() {
        for raw in [
            "<@abc>",
            "<@>",
            "<t:12:x>",
            "<t:99999999999999999>",
            "<:a:1>",
            "<#1 >",
            "a < b",
            "<< <@1",
        ] {
            assert_eq!(spans(raw), [text(raw)], "{raw}");
        }
    }

    #[test]
    fn code_blocks_with_and_without_language() {
        assert_eq!(
            parse("```rust\nfn main() {}\n```"),
            [code(Some("rust"), "fn main() {}", Style::default())]
        );
        assert_eq!(
            parse("```just code```"),
            [code(None, "just code", Style::default())]
        );
        // A lone word on the first line is code when nothing follows it.
        assert_eq!(
            parse("```rust\n```"),
            [code(None, "rust", Style::default())]
        );
    }

    #[test]
    fn code_blocks_split_the_text_and_hide_markdown() {
        assert_eq!(
            parse("before ```# *not* a heading``` after\nnext"),
            [
                paragraph("before "),
                code(None, "# *not* a heading", Style::default()),
                paragraph(" after\nnext"),
            ]
        );
        assert_eq!(
            parse("```\n- a\n> b\n```\nafter"),
            [code(None, "- a\n> b", Style::default()), paragraph("after")]
        );
        assert_eq!(spans("```unclosed"), [text("```unclosed")]);
    }

    #[test]
    fn quotes_take_their_lines_or_the_rest() {
        assert_eq!(
            parse("> one\n> **two**\nout"),
            [
                Block::Quote(vec![Block::Text(vec![
                    text("one\n"),
                    styled("two", bold())
                ])]),
                paragraph("out"),
            ]
        );
        assert_eq!(
            parse("in\n>>> all\nof this"),
            [
                paragraph("in"),
                Block::Quote(vec![paragraph("all\nof this")])
            ]
        );
        // No nesting, and the space is required.
        assert_eq!(parse("> > x"), [Block::Quote(vec![paragraph("> x")])]);
        assert_eq!(spans(">no space"), [text(">no space")]);
    }

    #[test]
    fn quotes_hold_other_blocks() {
        assert_eq!(
            parse("> # title\n> ```\n> code\n> ```"),
            [Block::Quote(vec![
                Block::Heading(1, vec![text("title")]),
                code(None, "code", Style::default()),
            ])]
        );
    }

    #[test]
    fn the_line_a_spoiled_block_ends_is_no_blank_line() {
        assert_eq!(
            parse("||```a```||\n> b"),
            [
                code(None, "a", spoiler(0)),
                Block::Quote(vec![paragraph("b")]),
            ]
        );
    }

    #[test]
    fn blank_lines_between_blocks_stay() {
        let quote = |s: &str| Block::Quote(vec![paragraph(s)]);
        let blank = paragraph("");
        assert_eq!(parse("> a\n\n> b"), [quote("a"), blank.clone(), quote("b")]);
        assert_eq!(parse("> a\n> b"), [quote("a\nb")]);
        let bullets = |s: &str| {
            Block::List(List {
                start: None,
                items: vec![vec![paragraph(s)]],
            })
        };
        assert_eq!(
            parse("- a\n\n> b"),
            [bullets("a"), blank.clone(), quote("b")]
        );
        let numbered = |start, s: &str| {
            Block::List(List {
                start: Some(start),
                items: vec![vec![paragraph(s)]],
            })
        };
        assert_eq!(
            parse("5. x\n\n3. y"),
            [numbered(5, "x"), blank.clone(), numbered(3, "y")]
        );
        assert_eq!(
            parse("text\n\n# h"),
            [paragraph("text\n"), Block::Heading(1, vec![text("h")])]
        );
    }

    #[test]
    fn headings_and_subtext_only_at_line_start() {
        assert_eq!(
            parse("# One\n## Two\n### Three\n#### Four\n-# small\ntext # not"),
            [
                Block::Heading(1, vec![text("One")]),
                Block::Heading(2, vec![text("Two")]),
                Block::Heading(3, vec![text("Three")]),
                paragraph("#### Four"),
                Block::Subtext(vec![text("small")]),
                paragraph("text # not"),
            ]
        );
        assert_eq!(spans("#tag"), [text("#tag")]);
        assert_eq!(spans("# "), [text("# ")]);
    }

    #[test]
    fn bullet_lists_nest_by_indentation() {
        let item = |s: &str| vec![paragraph(s)];
        assert_eq!(
            parse("- a\n  * b\n- c"),
            [Block::List(List {
                start: None,
                items: vec![
                    vec![
                        paragraph("a"),
                        Block::List(List {
                            start: None,
                            items: vec![item("b")],
                        }),
                    ],
                    item("c"),
                ],
            })]
        );
    }

    #[test]
    fn numbered_lists_keep_their_first_number() {
        let item = |s: &str| vec![paragraph(s)];
        assert_eq!(
            parse("intro\n3. a\n3. b\nend"),
            [
                paragraph("intro"),
                Block::List(List {
                    start: Some(3),
                    items: vec![item("a"), item("b")],
                }),
                paragraph("end"),
            ]
        );
        // Bullets after numbers start another list.
        assert_eq!(parse("1. a\n- b").len(), 2);
        assert_eq!(spans("*not a list*"), [styled("not a list", italic())]);
    }

    #[test]
    fn list_markers_count_up_or_change_with_depth() {
        let numbered = List {
            start: Some(3),
            items: Vec::new(),
        };
        assert_eq!(numbered.marker(0, 0), "3.");
        assert_eq!(numbered.marker(2, 5), "5.");
        let bullets = List {
            start: None,
            items: Vec::new(),
        };
        let shapes: Vec<String> = (0..4).map(|depth| bullets.marker(0, depth)).collect();
        assert_eq!(shapes, ["•", "◦", "▪", "•"]);
    }

    #[test]
    fn hidden_spoilers_take_the_click_before_links() {
        let link = Link {
            url: "https://a.org".into(),
            masked: false,
        };
        let style = Style {
            spoiler: Some(0),
            link: Some(link.clone()),
            ..Style::default()
        };
        assert_eq!(style.action(|_| false), Some(Action::Reveal(0)));
        assert_eq!(style.action(|_| true), Some(Action::Open(link)));
        assert_eq!(Style::default().action(|_| false), None);
    }

    #[test]
    fn the_keyboard_activates_a_paragraphs_only_action() {
        let open = Action::Open(Link {
            url: "https://a.org".into(),
            masked: true,
        });
        assert_eq!(keyboard_action(&[]), None);
        assert_eq!(keyboard_action(&[open.clone(), open.clone()]), Some(&open));
        assert_eq!(keyboard_action(&[open, Action::Reveal(0)]), None);
    }

    #[test]
    fn pathological_messages_stay_cheap() {
        // Each of these used to cost a scan to the end per character.
        for message in [
            "*".repeat(4000),
            format!("https://a{}", ")".repeat(3990)),
            "[".repeat(4000),
            "<".repeat(4000),
            "**a ".repeat(1000),
            "~".repeat(4000),
            "|".repeat(4000),
            "_".repeat(4000),
            "[a](".repeat(1000),
            "*a ".repeat(1300),
            "_a ".repeat(1300),
            "` `` ".repeat(800),
            "<@".repeat(2000),
            "||a\n".repeat(1000),
            "*a**".repeat(1000),
            "https://x(".repeat(400),
        ] {
            let started = std::time::Instant::now();
            parse(&message);
            assert!(
                started.elapsed() < std::time::Duration::from_millis(500),
                "{}…",
                &message[..10]
            );
        }
    }

    struct Fixed;

    impl Names for Fixed {
        fn user(&self, id: Id) -> Option<&str> {
            (id == 1).then_some("Léa")
        }
        fn channel(&self, id: Id) -> Option<&str> {
            (id == 2).then_some("général")
        }
        fn role(&self, id: Id) -> Option<&str> {
            (id == 3).then_some("modo")
        }
    }

    #[test]
    fn mention_labels_fall_back_to_discords_wording() {
        let names = Fixed;
        assert_eq!(Mention::User(1).label(&names), "@Léa");
        assert_eq!(Mention::Channel(2).label(&names), "#général");
        assert_eq!(Mention::Role(3).label(&names), "@modo");
        assert_eq!(Mention::User(9).label(&names), "@unknown-user");
        assert_eq!(Mention::Channel(9).label(&names), "#unknown");
        assert_eq!(Mention::Role(9).label(&names), "@deleted-role");
        assert_eq!(Mention::Everyone.label(&names), "@everyone");
    }

    #[test]
    fn the_directory_knows_the_channels_authors_and_dm_recipients() {
        let model = crate::demo::model();
        let general = Directory::new(&model, 111);
        assert_eq!(general.user(3), Some("marc"));
        // Léa writes in #général and has a DM open.
        assert_eq!(general.user(2), Some("Léa"));
        assert_eq!(general.channel(111), Some("général"));
        assert_eq!(general.user(999), None);
        // Known from the model's users wherever he writes.
        assert_eq!(Directory::new(&model, 112).user(1), Some("Dylan"));
        assert_eq!(general.role(150), Some("role-150"));
    }

    #[test]
    fn timestamp_styles_in_local_time() {
        let paris = jiff::tz::TimeZone::get("Europe/Paris").unwrap();
        let at: jiff::Timestamp = "2026-10-07T12:05:30Z".parse().unwrap();
        let label = |style: TimestampStyle| style.label(at, at, &paris);
        assert_eq!(label(TimestampStyle::ShortTime), "14:05");
        assert_eq!(label(TimestampStyle::LongTime), "14:05:30");
        assert_eq!(label(TimestampStyle::ShortDate), "07/10/2026");
        assert_eq!(label(TimestampStyle::LongDate), "7 October 2026");
        assert_eq!(label(TimestampStyle::ShortDateTime), "7 October 2026 14:05");
        assert_eq!(
            label(TimestampStyle::LongDateTime),
            "Wednesday, 7 October 2026 14:05"
        );
    }

    #[test]
    fn relative_timestamps_round_like_discord() {
        let now: jiff::Timestamp = "2026-10-07T12:00:00Z".parse().unwrap();
        let ago = |seconds: i64| relative(now - jiff::SignedDuration::from_secs(seconds), now);
        let ahead = |seconds: i64| relative(now + jiff::SignedDuration::from_secs(seconds), now);
        assert_eq!(ago(0), "a few seconds ago");
        assert_eq!(ago(44), "a few seconds ago");
        assert_eq!(ago(45), "a minute ago");
        assert_eq!(ago(5 * 60), "5 minutes ago");
        assert_eq!(ago(45 * 60), "an hour ago");
        assert_eq!(ago(3 * 3600), "3 hours ago");
        assert_eq!(ago(22 * 3600), "a day ago");
        assert_eq!(ahead(2 * 86400), "in 2 days");
        assert_eq!(ago(26 * 86400), "a month ago");
        assert_eq!(ago(90 * 86400), "3 months ago");
        assert_eq!(ago(330 * 86400), "a year ago");
        assert_eq!(ahead(3 * 365 * 86400), "in 3 years");
    }
}
