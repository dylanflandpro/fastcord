# fastcord

A native Discord client: Rust, egui 0.36 (glow), fastframe. Text only for now.

## Loop

- `cargo run -- --demo` shows the interface on offline sample data (`src/demo.rs`).
- `cargo build --workspace && cargo run -- -v` signs in for real (QR code). Use a
  secondary Discord account. `cargo run` alone does not build `fastcord-captcha`.
- Done means the CI steps in `.github/workflows/ci.yml` pass locally, `--locked` and
  `--workspace` included.

## Layout

- `model.rs`: Discord's objects and the rules that order them (sidebar, DM recency,
  message grouping, unread badges and mutes). It has no egui types: put behaviour
  here and test it here.
- `app.rs`: `App` holds the `Model`, the `Selection` (what is open) and the palette.
  Navigation lives on `Selection`, which takes `&Model`, so `ui.rs` draws from
  `&app.model` while it changes `app.selection`. Keep the model borrowed in place.
- `ui.rs`: drawing only. Turn any decision it makes into a function in `model.rs`
  or a method on `Selection`, with a test.
- `media.rs`: pictures in messages. The rules (which attachments are images, sizes,
  which hosts may be loaded: Discord's only) are tested functions; `Media` keeps the
  textures in memory under a cap and fetches off the interface's thread. Draw a
  picture only when it is on screen, so only what is seen loads.
- `compose.rs`: what a draft sends (trimming, emoji shortcodes, the 2,000-character
  limit and its counter, the web client's text commands). `app::Composer` keeps the
  drafts and turns one into a pending message (`model::Delivery`) and a
  `Command::Send`; the API's answer or the gateway's MESSAGE_CREATE with the same
  `nonce` replaces it. It keeps each message until confirmed, so a failure that finds
  no copy (a new READY replaced the conversation) shows it again or puts its text
  back in the draft. Demo runs confirm sends locally after a moment, and refuse them
  in #egui (slowmode) to show a failure.
- The keyboard never writes by accident: the composer keeps Tab (none of a message's
  buttons or reaction pills takes keyboard focus: `Sense::CLICK`), sends only on an
  Enter typed while it already held the cursor (not the one that opened the channel,
  nor a held Enter repeating), and keeps every `/command` but `/shrug`, `/tableflip`,
  `/unflip` and `/me` (`\/` sends a literal slash). Tests drive `ui::show` headless on
  the demo app with synthetic keys (`ui::tests::show`).
- Replies: `model::Reply` keeps what a reply answers (`Original`: shown, deleted when
  Discord sends null, unknown when it sends nothing) and whether it pinged, which an
  edit (quiet `allowed_mentions`, also when the original is not loaded: never a ping by
  guess), a retry and a draft put back keep. `markdown::reply_snippet` is the
  line above it.
- `typing.rs`: typing indicators both ways, timed as the web client's `TypingStore`
  (shown 10 s, sent at most every 8 s after 1.5 s, none past five typists). Typing goes
  outside the write queue (`Command::Typing`) and is never retried.
- `outbox.rs`: messages, edits and deletions (`backend::Write`) leave one at a time, in
  order; a failed message fails the channel's later messages unsent. Edits and
  deletions are not optimistic, as in the web client: the editor shows "Saving…" until
  Discord answers, a deletion waits for it, and a refusal is told under the message. `api::verdict` reads each attempt, status first: a lost
  answer or a 5xx is `Unsure` (no Retry until the gateway showed life after it: a
  connection, or a heartbeat acknowledged, `Event::Alive`), a 429 is waited out in full
  while the message says so. After a new READY an unsure message waits for its
  channel's history: there, it arrived; else it returns to the draft with a notice.
- Buttons acting on one message take their id from it (`ui::keyed_button`): egui
  credits a click to the id pressed, so auto ids (drawing order) could hand it to
  another message when the layout shifts. It allocates in the row and `interact`s
  with that id: a child Ui (`push_id`, `UiBuilder::id`) would not wrap in a wrapped
  row.
- `ui.rs` tests can draw a widget headless with `Context::run_ui` and synthetic
  events (see the composer's test): prefer that to launching the app.
- `theme.rs`: the palette, which follows Omarchy's through `fastframe-theme`.
- `backend.rs`: the thread that talks to Discord and the keyring. The interface
  sends `Command`s and reads what it reports; it never waits on the network.
  Acks (`Command::Ack`) write to the account. Only a conversation the person opened,
  shown down to its last message, in a focused window with input in the last ten
  minutes, is acked (`app::acknowledge`). `acks.rs` holds them the web client's 3 s
  (none with mentions), retries failures without ever sending an older one after a
  newer, and drops what the gateway says must not be read; closing sends what waits.
  Reactions (`Command::React`) write to the account too: the model shows them at
  once (`Model::toggle_reaction`) and `Event::ReactionFailed` undoes one Discord
  refused. Demo runs only change what is shown.
- `notify.rs`: desktop notifications. The backend decides as each message
  arrives (`wanted`, the web client's `shouldNotify`), against its own copy of
  the model without history, so a window that is not drawing does not hold
  them up; the window shares its focus and open channel through `Shared`. Tests
  never reach the desktop: `desktop()` (one D-Bus connection, zbus) is the only
  real notifier.
- `gateway.rs`: the live connection. `Gateway` is the protocol without I/O (hello,
  identify with the web client's capabilities, heartbeats, resume, close codes);
  `connect` drives one connection. The backend reconnects with doubling delays.
- `events.rs`: `Decoder` reads gateway events into `Update`s, which
  `Model::apply` applies. Shapes follow what the web client receives (READY's
  `users`, guild `properties`, `merged_members`); the fixture in
  `src/fixtures/ready.json` is made up, never real account data.
- `remote_auth.rs`: QR sign-in. `Handshake` is the protocol without I/O (test it
  there); `run` drives it over the socket.
- `api.rs`: Discord's HTTP API. `credentials.rs`: the token in the keyring.
- `captcha.rs`: runs `crates/captcha` (`fastcord-captcha`), a separate WebKitGTK
  program that shows Discord's hCaptcha. It is its own package so WebKit never
  links into `fastcord`: check with `ldd target/debug/fastcord | grep -i webkit`.
- `websocket.rs`: WebSocket upgrade written by hand, because Discord's remote auth
  gateway refuses a lowercase `origin` header (403) and the `http` crate
  lowercases every header name.

## Conventions

- Match Discord's official client: ordering, grouping and wording follow what it
  does, and doc comments say so when the rule is not obvious.
- Each `ScrollArea` gets an `id_salt` for what it shows (channel, view), so each
  one keeps its own scroll position.
- Demo messages get their ids from `Timeline`, which keeps snowflakes unique
  across channels.
- egui 0.36: `egui::Panel` replaces `SidePanel` and `TopBottomPanel`, and
  `eframe::App` splits into `logic` and `ui`.
- The `fastframe-*` crates share one git tag: bump them together.

## Privacy (hard rules)

The guarantees in README.md § Privacy are product promises: message content
stays in memory and stays out of logs, and the token goes only to the keyring.
`Token` has no `Debug`/`Display` and wipes itself on drop: keep it that way, and
never log a ticket, a QR URL (it holds the session fingerprint) or a payload.
serde errors quote the text they failed on: log them through `backend::describe`.

## Known gaps

- The conversation lays out every loaded message on each frame (messages are
  parsed once and cached, but laid out each frame). Virtualize it before long
  scroll-back sessions become common.
- Sending: no attachments, no `@silent`, no upload of over-long messages (Nitro's
  4,000-character limit is not known either), no timeout (`communication_disabled_until`)
  check, and Discord-only shortcodes (those the GitHub table lacks) stay as typed.
  A message whose answer was lost when a new READY (not a resume) arrives goes back to
  the draft, though it may have been sent: the reloaded history shows whether.
  Retry reuses the nonce, but the web client sends no `enforce_nonce`, so Discord does
  not deduplicate by it: a copy that did arrive confirms the retried one only locally.
- Editing and deleting cover my own messages only: deleting others' with Manage
  Messages (moderators) is not offered. "(edited)" sits on its own line, not after
  the text.
- Guilds over 75,000 members only send messages after a guild subscription
  (gateway op 37); smaller guilds are subscribed automatically on connect.
- Notifications know nothing of threads and forum posts (fastcord does not
  show them yet), and the "Notification previews" switch lives in memory only:
  there is no settings file.
