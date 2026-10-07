//! Pictures in messages: which attachments and embeds show one, at what
//! size, where it may come from, and the textures, kept in memory only.
//!
//! Like the official client, fastcord loads pictures from Discord's own
//! hosts only: an embed's picture comes through Discord's media proxy, never
//! from the site the link points to.

use crate::model::{Attachment, Embed, EmbedField, EmbedImage};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

/// The largest an image attachment is drawn, in points, as in the official
/// client.
pub const ATTACHMENT_BOX: [f32; 2] = [550.0, 350.0];
/// An embed's large picture.
pub const EMBED_IMAGE_BOX: [f32; 2] = [400.0, 300.0];
/// An embed's thumbnail, beside its text.
pub const THUMBNAIL_BOX: [f32; 2] = [80.0, 80.0];
/// The widest an embed's text runs, its padding on each side, and the gap
/// before its thumbnail.
const EMBED_TEXT_WIDTH: f32 = 432.0;
pub const EMBED_PADDING: f32 = 16.0;
pub const THUMBNAIL_GAP: f32 = 16.0;
/// Between inline fields side by side.
pub const FIELD_GAP: f32 = 8.0;
/// The longest side of any texture, in pixels: the full-size viewer's copy.
pub const MAX_SIDE: u32 = 2048;
/// A hidden spoiler is drawn from a copy this much smaller, stretched: a
/// blur, as the official client shows it.
pub const SPOILER_SCALE: f32 = 1.0 / 20.0;
/// What decoded pictures may take in memory, in bytes. At most a screenful
/// is drawn at once, well under it, so the least recently drawn go first.
const CACHE_BYTES: usize = 64 << 20;
/// The heaviest download accepted; the largest picture and allocation one
/// decode may make.
const MAX_DOWNLOAD: usize = 32 << 20;
const MAX_DIMENSION: u32 = 12_000;
const MAX_DECODE: u64 = 96 << 20;
/// Downloads at once, and decodes, which take more memory.
const DOWNLOADS: usize = 4;
const DECODES: usize = 2;
/// Redirects followed, all within Discord's hosts.
const MAX_REDIRECTS: usize = 5;
/// A request not drawn for this many frames is no longer wanted.
const STALE_FRAMES: u64 = 2;
/// How long a picture that failed shows as failed before it is tried again.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Extensions the official client shows as pictures.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Whether an attachment shows as a picture rather than a file card: an
/// image Discord could measure. SVGs show as files, as in the official
/// client.
pub fn is_image(attachment: &Attachment) -> bool {
    let typed = attachment
        .content_type
        .as_deref()
        .is_some_and(|t| t.starts_with("image/") && t != "image/svg+xml");
    let named = attachment
        .filename
        .rsplit_once('.')
        .is_some_and(|(_, ext)| IMAGE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)));
    (typed || named) && attachment.width.is_some() && attachment.height.is_some()
}

/// Whether an attachment was sent behind a spoiler: Discord's flag, or the
/// `SPOILER_` prefix its clients give the file's name.
pub fn is_spoiler(attachment: &Attachment) -> bool {
    attachment.flags & Attachment::SPOILER != 0 || attachment.filename.starts_with("SPOILER_")
}

/// The picture an embed shows on its own, without a frame: a link to an
/// image or a GIF, as the official client draws them. Of its thumbnail and
/// image, the one Discord kept a copy of.
pub fn standalone(embed: &Embed) -> Option<&EmbedImage> {
    if !matches!(embed.kind.as_deref(), Some("image" | "gifv")) {
        return None;
    }
    let pictures = [embed.thumbnail.as_ref(), embed.image.as_ref()];
    pictures
        .into_iter()
        .flatten()
        .find(|p| p.proxy_url.as_deref().and_then(loadable).is_some())
        .or(pictures.into_iter().flatten().next())
}

/// The size to draw a picture at, in points: within `bounds`, its aspect
/// ratio kept, never enlarged. A picture of unknown size takes the box.
pub fn fit(size: Option<[u32; 2]>, bounds: [f32; 2]) -> [f32; 2] {
    let Some([width, height]) = size.filter(|[w, h]| *w > 0 && *h > 0) else {
        return bounds;
    };
    let (width, height) = (width as f32, height as f32);
    let scale = (bounds[0] / width).min(bounds[1] / height).min(1.0);
    [
        (width * scale).round().max(1.0),
        (height * scale).round().max(1.0),
    ]
}

/// [`fit`], from Discord's dimensions, or else from the copy loaded
/// (`loaded` pixels, asked for at `pixels_per_point`), so a picture Discord
/// did not measure takes its own shape once it arrives.
pub fn drawn_size(
    size: Option<[u32; 2]>,
    loaded: Option<[f32; 2]>,
    bounds: [f32; 2],
    pixels_per_point: f32,
) -> [f32; 2] {
    let measured =
        size.or_else(|| loaded.map(|px| px.map(|side| (side / pixels_per_point).round() as u32)));
    fit(measured, bounds)
}

/// The box the full-size viewer fits a picture in: most of the window,
/// with room below for its link.
pub fn viewer_bounds(window: [f32; 2]) -> [f32; 2] {
    [
        (window[0] * 0.9).max(0.0),
        (window[1] * 0.9 - 32.0).max(0.0),
    ]
}

/// How an embed card shares the room it has.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EmbedLayout {
    /// The text column's width.
    pub text: f32,
    /// The box its large picture fits in.
    pub image: [f32; 2],
}

/// An embed card within `available` points, padding included, as the
/// official client lays it out: the text runs up to 432 points, narrowing
/// to leave the thumbnail its room, and the picture fits inside the card.
pub fn embed_layout(available: f32, thumbnail: bool) -> EmbedLayout {
    let inner = (available - 2.0 * EMBED_PADDING).max(0.0);
    let beside = if thumbnail {
        THUMBNAIL_GAP + THUMBNAIL_BOX[0]
    } else {
        0.0
    };
    EmbedLayout {
        text: (inner - beside).clamp(0.0, EMBED_TEXT_WIDTH),
        image: [EMBED_IMAGE_BOX[0].min(inner), EMBED_IMAGE_BOX[1]],
    }
}

/// "18.36 KB", in powers of 1024, as the official client writes sizes.
pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.2} {}", UNITS[unit])
}

/// The narrowest an inline field gets before fewer share a row.
const MIN_FIELD_WIDTH: f32 = 120.0;

/// How many inline fields share a row `width` points wide: three in the
/// official client, two beside a thumbnail, fewer when they would not fit.
pub fn fields_per_row(thumbnail: bool, width: f32) -> usize {
    let most = if thumbnail { 2 } else { 3 };
    most.min((width / MIN_FIELD_WIDTH) as usize).max(1)
}

/// The width of each of `columns` fields sharing a row `width` wide.
pub fn field_width(width: f32, columns: usize) -> f32 {
    let columns = columns.max(1) as f32;
    ((width - FIELD_GAP * (columns - 1.0)) / columns).max(0.0)
}

/// The embed's fields in rows: inline fields side by side, up to
/// `per_row`, and every other field on a row of its own.
pub fn field_rows(fields: &[EmbedField], per_row: usize) -> Vec<Range<usize>> {
    let mut rows: Vec<Range<usize>> = Vec::new();
    for (index, field) in fields.iter().enumerate() {
        match rows.last_mut() {
            Some(row) if field.inline && fields[row.start].inline && row.len() < per_row => {
                row.end = index + 1;
            }
            _ => rows.push(index..index + 1),
        }
    }
    rows
}

/// Discord's media proxy, which resizes and converts what it serves.
const MEDIA_PROXY: &str = "media.discordapp.net";

/// The proxy for pictures from other sites: `images-ext-1.discordapp.net`…
fn is_external_proxy(host: &str) -> bool {
    host.strip_prefix("images-ext-")
        .and_then(|rest| rest.strip_suffix(".discordapp.net"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `url` when it is one fastcord may load: https on Discord's CDN or media
/// proxies, nothing else.
pub fn loadable(url: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?;
    let discord = host == "cdn.discordapp.com" || host == MEDIA_PROXY || is_external_proxy(host);
    let plain = url.port().is_none() && url.username().is_empty() && url.password().is_none();
    (url.scheme() == "https" && discord && plain).then_some(url)
}

/// One copy of a picture: where to load it from, and the most pixels it
/// may keep once decoded.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Request {
    pub url: String,
    pub max: [u32; 2],
}

/// A picture to draw, borrowed from the message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Picture<'a> {
    /// What to load. An embed picture Discord kept no copy of has none.
    pub source: Option<&'a str>,
    /// What "Open in browser" opens.
    pub link: &'a str,
    /// In pixels, when Discord measured it.
    pub size: Option<[u32; 2]>,
}

fn dimensions(width: Option<u32>, height: Option<u32>) -> Option<[u32; 2]> {
    Some([width?, height?])
}

impl<'a> Picture<'a> {
    pub fn attachment(attachment: &'a Attachment) -> Self {
        let source = if attachment.proxy_url.is_empty() {
            &attachment.url
        } else {
            &attachment.proxy_url
        };
        Self {
            source: Some(source),
            link: &attachment.url,
            size: dimensions(attachment.width, attachment.height),
        }
    }

    pub fn embed(image: &'a EmbedImage) -> Self {
        Self {
            source: image.proxy_url.as_deref(),
            link: &image.url,
            size: dimensions(image.width, image.height),
        }
    }

    /// The copy to load for drawing within `bounds` points, `None` when it
    /// may not be loaded. The media proxies resize it to the pixels drawn
    /// (and the main one converts it to WebP), as the official client asks.
    pub fn request(&self, bounds: [f32; 2], pixels_per_point: f32) -> Option<Request> {
        let mut url = loadable(self.source?)?;
        let [width, height] = fit(self.size, bounds)
            .map(|side| ((side * pixels_per_point).round() as u32).clamp(1, MAX_SIDE));
        let host = url.host_str().unwrap_or_default().to_owned();
        if host == MEDIA_PROXY || is_external_proxy(&host) {
            let mut query = url.query_pairs_mut();
            if host == MEDIA_PROXY {
                query.append_pair("format", "webp");
            }
            if self.size.is_some() {
                query
                    .append_pair("width", &width.to_string())
                    .append_pair("height", &height.to_string());
            }
        }
        Some(Request {
            url: url.into(),
            max: [width, height],
        })
    }

    pub fn to_owned(self) -> Viewing {
        Viewing {
            source: self.source.map(Into::into),
            link: self.link.into(),
            size: self.size,
        }
    }
}

/// The picture open in the full-size viewer.
#[derive(Clone, Debug, PartialEq)]
pub struct Viewing {
    source: Option<String>,
    link: String,
    size: Option<[u32; 2]>,
}

impl Viewing {
    pub fn picture(&self) -> Picture<'_> {
        Picture {
            source: self.source.as_deref(),
            link: &self.link,
            size: self.size,
        }
    }
}

/// Values that each cost some bytes; once the total passes the cap, the
/// least recently used go first.
pub struct Lru<K, V> {
    entries: HashMap<K, Slot<V>>,
    used: usize,
    cap: usize,
    clock: u64,
}

struct Slot<V> {
    value: V,
    cost: usize,
    used_at: u64,
}

impl<K: Clone + Eq + Hash, V> Lru<K, V> {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            used: 0,
            cap,
            clock: 0,
        }
    }

    /// The value, without counting as a use.
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|slot| &slot.value)
    }

    /// The value, which now counts as the most recently used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.clock += 1;
        let slot = self.entries.get_mut(key)?;
        slot.used_at = self.clock;
        Some(&slot.value)
    }

    /// Adds a value, then drops the least recently used others until the
    /// total fits. The new one stays even alone over the cap.
    pub fn insert(&mut self, key: K, value: V, cost: usize) {
        self.clock += 1;
        let slot = Slot {
            value,
            cost,
            used_at: self.clock,
        };
        if let Some(old) = self.entries.insert(key.clone(), slot) {
            self.used -= old.cost;
        }
        self.used += cost;
        while self.used > self.cap {
            let Some(oldest) = self
                .entries
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, slot)| slot.used_at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(slot) = self.entries.remove(&oldest) {
                self.used -= slot.cost;
            }
        }
    }

    /// The bytes held.
    #[cfg(test)]
    pub fn used(&self) -> usize {
        self.used
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.used = 0;
    }
}

/// Where pictures come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Discord,
    /// The demo's pictures, built into the binary: nothing goes online.
    Demo,
}

/// What a picture looks like this frame.
#[derive(Clone, Copy)]
pub enum Shown {
    Loading,
    Ready(egui::load::SizedTexture),
    Failed,
}

/// The requests the interface still draws, by the frame it last asked for
/// each: the fetcher skips the others when their turn comes, so scrolling
/// past a channel of pictures does not download them all.
#[derive(Default)]
struct Wanted {
    frame: AtomicU64,
    asked: Mutex<HashMap<Request, u64>>,
}

impl Wanted {
    fn asked(&self) -> std::sync::MutexGuard<'_, HashMap<Request, u64>> {
        self.asked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stamp(&self, request: &Request) {
        let frame = self.frame.load(Ordering::Relaxed);
        self.asked().insert(request.clone(), frame);
    }

    fn still(&self, request: &Request) -> bool {
        let frame = self.frame.load(Ordering::Relaxed);
        self.asked()
            .get(request)
            .is_some_and(|&at| frame.saturating_sub(at) <= STALE_FRAMES)
    }
}

/// What became of a request.
enum Outcome {
    Loaded(egui::ColorImage),
    Failed,
    /// No longer drawn when its turn came: asked again if it comes back.
    Skipped,
}

/// The textures of the pictures drawn lately, and the thread that fetches
/// and decodes them off the interface's thread.
pub struct Media {
    source: Source,
    textures: Lru<Request, egui::TextureHandle>,
    pending: HashSet<Request>,
    /// When each failed.
    failed: HashMap<Request, Instant>,
    wanted: Arc<Wanted>,
    fetcher: Option<Fetcher>,
    /// The picture open full size, if any.
    pub viewing: Option<Viewing>,
}

struct Fetcher {
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
    results: mpsc::Receiver<(Request, Outcome)>,
}

impl Media {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            textures: Lru::new(CACHE_BYTES),
            pending: HashSet::new(),
            failed: HashMap::new(),
            wanted: Arc::default(),
            fetcher: None,
            viewing: None,
        }
    }

    /// The picture if it is ready, without asking for it or counting as a
    /// use: for measuring.
    pub fn peek(&self, request: &Request) -> Option<egui::load::SizedTexture> {
        self.textures.peek(request).map(Into::into)
    }

    /// The picture, asked for if it is not loaded. Draw only what is on
    /// screen, so a long channel loads only what is seen.
    pub fn get(&mut self, ctx: &egui::Context, request: Request) -> Shown {
        if let Some(texture) = self.textures.get(&request).map(Into::into) {
            return Shown::Ready(texture);
        }
        if let Some(at) = self.failed.get(&request) {
            if at.elapsed() < RETRY_AFTER {
                return Shown::Failed;
            }
            self.failed.remove(&request);
        }
        self.wanted.stamp(&request);
        if self.pending.insert(request.clone()) {
            let (source, wanted) = (self.source, self.wanted.clone());
            let fetcher = self
                .fetcher
                .get_or_insert_with(|| Fetcher::start(ctx.clone(), source, wanted));
            // A fetcher whose thread could not start leaves it loading.
            let _ = fetcher.requests.send(request);
        }
        Shown::Loading
    }

    /// Turns what the fetcher decoded into textures. Called once a frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        self.wanted.frame.fetch_add(1, Ordering::Relaxed);
        let Some(fetcher) = &self.fetcher else {
            return;
        };
        for (request, outcome) in fetcher.results.try_iter() {
            self.pending.remove(&request);
            self.wanted.asked().remove(&request);
            match outcome {
                Outcome::Loaded(image) => {
                    let cost = image.pixels.len() * 4;
                    // Named generically: texture names show in debug tools.
                    let texture = ctx.load_texture("picture", image, egui::TextureOptions::LINEAR);
                    self.textures.insert(request, texture, cost);
                }
                Outcome::Failed => {
                    self.failed.insert(request, Instant::now());
                }
                Outcome::Skipped => {}
            }
        }
    }

    /// Forgets every picture, as when the account signs out. Downloads
    /// under way finish into nothing.
    pub fn clear(&mut self) {
        self.textures.clear();
        self.pending.clear();
        self.failed.clear();
        self.wanted = Arc::default();
        self.fetcher = None;
        self.viewing = None;
    }
}

impl Fetcher {
    fn start(ctx: egui::Context, source: Source, wanted: Arc<Wanted>) -> Self {
        let (requests, mut incoming) = tokio::sync::mpsc::unbounded_channel::<Request>();
        let (sender, results) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("media".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => return log::warn!("pictures will not load: {error}"),
                };
                let client = (source == Source::Discord).then(client);
                let slots = Arc::new(Slots {
                    downloads: tokio::sync::Semaphore::new(DOWNLOADS),
                    decodes: tokio::sync::Semaphore::new(DECODES),
                });
                runtime.block_on(async move {
                    while let Some(request) = incoming.recv().await {
                        let (client, sender, ctx) = (client.clone(), sender.clone(), ctx.clone());
                        let (slots, wanted) = (slots.clone(), wanted.clone());
                        tokio::spawn(async move {
                            let outcome = load(client.as_ref(), &request, &slots, &wanted).await;
                            if sender.send((request, outcome)).is_ok() {
                                ctx.request_repaint();
                            }
                        });
                    }
                });
            });
        if let Err(error) = spawned {
            log::warn!("pictures will not load: {error}");
        }
        Self { requests, results }
    }
}

struct Slots {
    downloads: tokio::sync::Semaphore,
    decodes: tokio::sync::Semaphore,
}

/// Requests as a browser would, without cookies or referer, following a
/// few redirects only within Discord's hosts.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::api::USER_AGENT)
        .referer(false)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let allowed = loadable(attempt.url().as_str()).is_some();
            if allowed && attempt.previous().len() < MAX_REDIRECTS {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("an HTTP client with the bundled TLS roots")
}

async fn load(
    client: Option<&reqwest::Client>,
    request: &Request,
    slots: &Slots,
    wanted: &Wanted,
) -> Outcome {
    let bytes = {
        let Ok(_download) = slots.downloads.acquire().await else {
            return Outcome::Failed;
        };
        if !wanted.still(request) {
            return Outcome::Skipped;
        }
        match client {
            Some(client) => download(client, &request.url).await,
            None => crate::demo::image(&request.url).map(<[u8]>::to_vec),
        }
    };
    let Some(bytes) = bytes else {
        return Outcome::Failed;
    };
    let Ok(_decode) = slots.decodes.acquire().await else {
        return Outcome::Failed;
    };
    let max = request.max;
    match tokio::task::spawn_blocking(move || decode(&bytes, max)).await {
        Ok(Some(image)) => Outcome::Loaded(image),
        _ => Outcome::Failed,
    }
}

async fn download(client: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    // Checked again where it counts: nothing else is ever contacted.
    loadable(url)?;
    // reqwest quotes the URL in its errors; it would show the channel.
    let failed =
        |error: reqwest::Error| log::debug!("a picture did not load: {}", error.without_url());
    let mut response = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(failed)
        .ok()?;
    if response.content_length().unwrap_or(0) > MAX_DOWNLOAD as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(failed).ok()? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > MAX_DOWNLOAD {
            return None;
        }
    }
    Some(bytes)
}

/// A reader that refuses pictures too large to decode safely.
fn reader(bytes: &[u8]) -> Option<image::ImageReader<std::io::Cursor<&[u8]>>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE);
    reader.limits(limits);
    Some(reader)
}

/// Decodes a picture (the first frame of an animation) and shrinks it to
/// `max` pixels, so its texture costs no more than what is drawn.
fn decode(bytes: &[u8], max: [u32; 2]) -> Option<egui::ColorImage> {
    let mut image = reader(bytes)?.decode().ok()?;
    let [width, height] = max;
    if image.width() > width || image.height() > height {
        image = image.thumbnail(width, height);
    }
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        rgba.as_raw(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attachment(filename: &str, content_type: Option<&str>, size: Option<u32>) -> Attachment {
        Attachment {
            id: 1,
            filename: filename.into(),
            url: format!("https://cdn.discordapp.com/attachments/1/2/{filename}"),
            proxy_url: format!("https://media.discordapp.net/attachments/1/2/{filename}"),
            content_type: content_type.map(Into::into),
            size: 1000,
            width: size,
            height: size,
            flags: 0,
        }
    }

    #[test]
    fn measured_images_show_as_pictures() {
        assert!(is_image(&attachment("a.bin", Some("image/png"), Some(10))));
        assert!(is_image(&attachment("Photo.JPG", None, Some(10))));
        // Not measured, an SVG, or not an image at all: a file card.
        assert!(!is_image(&attachment("a.png", Some("image/png"), None)));
        assert!(!is_image(&attachment(
            "a.svg",
            Some("image/svg+xml"),
            Some(10)
        )));
        assert!(!is_image(&attachment(
            "notes.pdf",
            Some("application/pdf"),
            Some(10)
        )));
        assert!(!is_image(&attachment("png", None, Some(10))));
    }

    #[test]
    fn pictures_fit_their_box_without_growing() {
        let size = |w, h| Some([w, h]);
        assert_eq!(fit(size(1100, 700), [550.0, 350.0]), [550.0, 350.0]);
        // Wide: the width limits; tall: the height does.
        assert_eq!(fit(size(2000, 500), [550.0, 350.0]), [550.0, 138.0]);
        assert_eq!(fit(size(300, 1200), [550.0, 350.0]), [88.0, 350.0]);
        assert_eq!(fit(size(40, 30), [550.0, 350.0]), [40.0, 30.0]);
        assert_eq!(fit(None, [80.0, 80.0]), [80.0, 80.0]);
        assert_eq!(fit(size(0, 10), [80.0, 80.0]), [80.0, 80.0]);
    }

    #[test]
    fn only_discord_hosts_over_https_load() {
        for url in [
            "https://cdn.discordapp.com/attachments/1/2/a.png",
            "https://media.discordapp.net/attachments/1/2/a.png?ex=1&is=2&hm=3&",
            "https://images-ext-1.discordapp.net/external/abc/https/example.com/a.png",
        ] {
            assert!(loadable(url).is_some(), "{url}");
        }
        for url in [
            "http://cdn.discordapp.com/attachments/1/2/a.png",
            "https://example.com/a.png",
            "https://cdn.discordapp.com.example.com/a.png",
            "https://cdn.discordapp.com@example.com/a.png",
            "https://images-ext-x.discordapp.net/a.png",
            "https://cdn.discordapp.com:8443/a.png",
            "https://cdn.discordapp.com./a.png",
            "https://images-ext-1.discordapp.net.evil.com/a.png",
            "https://images-ext-.discordapp.net/a.png",
            // A look-alike with a Cyrillic "а", read as punycode.
            "https://cdn.discord\u{430}pp.com/a.png",
            "https://cdn.discordapp.com%2eevil.com/a.png",
            "not a url",
        ] {
            assert!(loadable(url).is_none(), "{url}");
        }
        // The same hosts, written differently: URLs normalise them.
        for url in [
            "https://CDN.DiscordApp.com/a.png",
            "https://cdn.discordapp.com:443/a.png",
            "https://cdn%2Ediscordapp.com/a.png",
        ] {
            let host = loadable(url).map(|u| u.host_str().map(str::to_owned));
            assert_eq!(host, Some(Some("cdn.discordapp.com".into())), "{url}");
        }
    }

    #[test]
    fn embed_pictures_load_only_through_discord() {
        let image = |proxy_url: Option<&str>| EmbedImage {
            url: "https://example.com/a.png".into(),
            proxy_url: proxy_url.map(Into::into),
            width: Some(100),
            height: Some(50),
        };
        let without = image(None);
        assert_eq!(Picture::embed(&without).request([80.0; 2], 1.0), None);
        let elsewhere = image(Some("https://example.com/a.png"));
        assert_eq!(Picture::embed(&elsewhere).request([80.0; 2], 1.0), None);
        let proxied = image(Some(
            "https://images-ext-1.discordapp.net/external/x/https/example.com/a.png",
        ));
        assert_eq!(
            Picture::embed(&proxied).request([80.0; 2], 2.0),
            Some(Request {
                url: "https://images-ext-1.discordapp.net/external/x/https/example.com/a.png?width=160&height=80".into(),
                max: [160, 80],
            })
        );
    }

    #[test]
    fn attachments_ask_the_proxy_for_the_pixels_drawn() {
        let a = attachment("a.png", Some("image/png"), Some(1000));
        let request = Picture::attachment(&a)
            .request(ATTACHMENT_BOX, 2.0)
            .unwrap();
        assert_eq!(
            request.url,
            "https://media.discordapp.net/attachments/1/2/a.png?format=webp&width=700&height=700"
        );
        assert_eq!(request.max, [700, 700]);
        // Without Discord's copy, the CDN's file, resized here.
        let mut direct = a.clone();
        direct.proxy_url.clear();
        let request = Picture::attachment(&direct)
            .request(ATTACHMENT_BOX, 1.0)
            .unwrap();
        assert_eq!(
            request.url,
            "https://cdn.discordapp.com/attachments/1/2/a.png"
        );
        assert_eq!(request.max, [350, 350]);
    }

    #[test]
    fn sizes_read_as_the_official_client_writes_them() {
        assert_eq!(human_size(0), "0 bytes");
        assert_eq!(human_size(1023), "1023 bytes");
        assert_eq!(human_size(18_800), "18.36 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.00 MB");
        assert_eq!(human_size(3 << 40), "3072.00 GB");
    }

    #[test]
    fn inline_fields_share_rows() {
        let field = |inline| EmbedField {
            name: String::new(),
            value: String::new(),
            inline,
        };
        let fields: Vec<EmbedField> = [true, true, true, true, false, true, false, false]
            .into_iter()
            .map(field)
            .collect();
        assert_eq!(
            field_rows(&fields, 3),
            vec![0..3, 3..4, 4..5, 5..6, 6..7, 7..8]
        );
        assert_eq!(field_rows(&fields[..4], 2), vec![0..2, 2..4]);
        assert!(field_rows(&[], 3).is_empty());
        assert_eq!(fields_per_row(false, 432.0), 3);
        assert_eq!(fields_per_row(true, 336.0), 2);
        assert_eq!(fields_per_row(false, 250.0), 2);
        assert_eq!(fields_per_row(false, 60.0), 1);
    }

    #[test]
    fn lru_drops_the_least_recently_used_over_the_cap() {
        let mut lru = Lru::new(10);
        lru.insert("a", 'a', 4);
        lru.insert("b", 'b', 4);
        // Drawing "a" again makes "b" the oldest.
        assert_eq!(lru.get(&"a"), Some(&'a'));
        lru.insert("c", 'c', 4);
        let has = |lru: &Lru<&str, char>, key| lru.peek(&key).is_some();
        assert!(has(&lru, "a") && has(&lru, "c") && !has(&lru, "b"));
        assert_eq!(lru.used(), 8);
        // Replacing a value counts its new cost only.
        lru.insert("c", 'C', 6);
        assert_eq!(lru.used(), 10);
        // One over the cap on its own stays, alone.
        lru.insert("d", 'd', 20);
        assert_eq!(lru.used(), 20);
        assert!(has(&lru, "d") && !has(&lru, "a"));
        // Peeking does not count as a use: "c" goes before "e".
        lru.insert("c", 'c', 4);
        lru.insert("e", 'e', 4);
        assert!(lru.peek(&"c").is_some());
        lru.insert("f", 'f', 4);
        assert!(!has(&lru, "c") && has(&lru, "e") && has(&lru, "f"));
    }

    #[test]
    fn decoding_shrinks_to_the_pixels_asked() {
        let image = decode(include_bytes!("../assets/demo/paysage.png"), [480, 480]).unwrap();
        assert_eq!(image.size, [480, 270]);
        let small = decode(include_bytes!("../assets/demo/logo.png"), [480, 480]).unwrap();
        assert_eq!(small.size, [160, 160]);
        assert!(decode(b"not a picture", [10, 10]).is_none());
    }

    /// A PNG's opening: its signature and a header declaring `side`².
    fn png_header(side: u32) -> Vec<u8> {
        let crc = |bytes: &[u8]| {
            let mut crc = !0u32;
            for &byte in bytes {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = if crc & 1 == 1 {
                        (crc >> 1) ^ 0xedb8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        };
        let mut header = b"IHDR".to_vec();
        header.extend(side.to_be_bytes());
        header.extend(side.to_be_bytes());
        header.extend([8, 6, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(13u32.to_be_bytes());
        png.extend(&header);
        png.extend(crc(&header).to_be_bytes());
        png
    }

    #[test]
    fn huge_pictures_are_refused_before_decoding() {
        let huge = png_header(65_535);
        let refused = reader(&huge).unwrap().decode();
        assert!(
            matches!(refused, Err(image::ImageError::Limits(_))),
            "{refused:?}"
        );
        assert!(decode(&huge, [100, 100]).is_none());
    }

    #[test]
    fn image_and_gif_links_show_on_their_own() {
        let picture = EmbedImage {
            url: String::new(),
            proxy_url: None,
            width: None,
            height: None,
        };
        let mut embed = Embed {
            kind: Some("gifv".into()),
            title: None,
            description: None,
            url: None,
            color: None,
            author: None,
            footer: None,
            provider: None,
            fields: vec![],
            thumbnail: Some(picture),
            image: None,
        };
        assert!(standalone(&embed).is_some());
        // The image Discord kept a copy of wins over a thumbnail it did not.
        let copied = EmbedImage {
            proxy_url: Some("https://images-ext-1.discordapp.net/external/x".into()),
            ..embed.thumbnail.clone().unwrap()
        };
        embed.image = Some(copied.clone());
        assert_eq!(standalone(&embed), Some(&copied));
        embed.kind = Some("rich".into());
        assert!(standalone(&embed).is_none());
    }

    #[test]
    fn spoilers_by_flag_or_name() {
        let mut photo = attachment("photo.png", Some("image/png"), Some(10));
        assert!(!is_spoiler(&photo));
        photo.flags = Attachment::SPOILER;
        assert!(is_spoiler(&photo));
        assert!(is_spoiler(&attachment("SPOILER_photo.png", None, Some(10))));
        assert!(!is_spoiler(&attachment(
            "spoiler_photo.png",
            None,
            Some(10)
        )));
    }

    #[test]
    fn sizes_come_from_discord_or_else_the_copy_loaded() {
        // Discord's dimensions win; the copy is not looked at.
        assert_eq!(
            drawn_size(Some([1100, 700]), Some([10.0, 10.0]), ATTACHMENT_BOX, 2.0),
            [550.0, 350.0]
        );
        // A copy of 200×100 pixels at 2 pixels per point: 100×50 points.
        assert_eq!(
            drawn_size(None, Some([200.0, 100.0]), ATTACHMENT_BOX, 2.0),
            [100.0, 50.0]
        );
        assert_eq!(drawn_size(None, None, THUMBNAIL_BOX, 1.0), THUMBNAIL_BOX);
    }

    #[test]
    fn the_viewer_keeps_inside_the_window() {
        assert_eq!(viewer_bounds([1000.0, 800.0]), [900.0, 688.0]);
        assert_eq!(viewer_bounds([20.0, 20.0]), [18.0, 0.0]);
    }

    #[test]
    fn embeds_narrow_their_text_before_their_thumbnail() {
        let wide = embed_layout(1000.0, true);
        assert_eq!(
            wide,
            EmbedLayout {
                text: 432.0,
                image: EMBED_IMAGE_BOX
            }
        );
        // A half-screen window: the thumbnail keeps its 80 points, the
        // picture fits inside the padding.
        let narrow = embed_layout(300.0, true);
        assert_eq!(
            narrow,
            EmbedLayout {
                text: 172.0,
                image: [268.0, 300.0]
            }
        );
        assert_eq!(embed_layout(300.0, false).text, 268.0);
        assert_eq!(embed_layout(10.0, true).text, 0.0);
        assert_eq!(field_width(336.0, 2), 164.0);
        assert_eq!(field_width(100.0, 1), 100.0);
    }

    #[test]
    fn stale_requests_are_skipped() {
        let wanted = Wanted::default();
        let request = Request {
            url: "https://cdn.discordapp.com/a.png".into(),
            max: [1, 1],
        };
        assert!(!wanted.still(&request), "never asked");
        wanted.stamp(&request);
        wanted.frame.store(STALE_FRAMES, Ordering::Relaxed);
        assert!(wanted.still(&request));
        wanted.frame.store(STALE_FRAMES + 1, Ordering::Relaxed);
        assert!(!wanted.still(&request), "scrolled away");
    }

    fn demo_request(name: &str) -> Request {
        let url = format!("https://cdn.discordapp.com/attachments/1/2/{name}");
        let picture = Picture {
            source: Some(&url),
            link: &url,
            size: Some([160, 160]),
        };
        picture.request(THUMBNAIL_BOX, 1.0).unwrap()
    }

    /// Polls until `done`, for up to five seconds.
    fn wait(media: &mut Media, ctx: &egui::Context, done: impl Fn(&Media) -> bool) {
        for _ in 0..500 {
            media.poll(ctx);
            if done(media) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out");
    }

    #[test]
    fn pictures_load_off_the_thread_and_clear_drops_late_ones() {
        let ctx = egui::Context::default();
        let mut media = Media::new(Source::Demo);
        let logo = demo_request("logo.png");
        assert!(matches!(media.get(&ctx, logo.clone()), Shown::Loading));
        wait(&mut media, &ctx, |m| m.peek(&logo).is_some());
        assert!(matches!(media.get(&ctx, logo.clone()), Shown::Ready(_)));

        media.clear();
        assert!(media.peek(&logo).is_none());
        // Asked, then cleared before it arrives: it never lands.
        media.get(&ctx, logo.clone());
        media.clear();
        std::thread::sleep(Duration::from_millis(200));
        media.poll(&ctx);
        assert!(media.peek(&logo).is_none() && media.pending.is_empty());
    }

    #[test]
    fn failures_are_tried_again_after_a_while() {
        let ctx = egui::Context::default();
        let mut media = Media::new(Source::Demo);
        let missing = demo_request("missing.png");
        media.get(&ctx, missing.clone());
        wait(&mut media, &ctx, |m| m.failed.contains_key(&missing));
        assert!(matches!(media.get(&ctx, missing.clone()), Shown::Failed));
        let long_ago = Instant::now().checked_sub(RETRY_AFTER).unwrap();
        media.failed.insert(missing.clone(), long_ago);
        assert!(matches!(media.get(&ctx, missing.clone()), Shown::Loading));
        assert!(media.pending.contains(&missing));
    }
}
