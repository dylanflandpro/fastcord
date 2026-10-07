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
  message grouping). It has no egui types: put behaviour here and test it here.
- `app.rs`: `App` holds the `Model`, the `Selection` (what is open) and the palette.
  Navigation lives on `Selection`, which takes `&Model`, so `ui.rs` draws from
  `&app.model` while it changes `app.selection`. Keep the model borrowed in place.
- `ui.rs`: drawing only. Turn any decision it makes into a function in `model.rs`
  or a method on `Selection`, with a test.
- `theme.rs`: the palette, which follows Omarchy's through `fastframe-theme`.
- `backend.rs`: the thread that talks to Discord and the keyring. The interface
  sends `Command`s and reads what it reports; it never waits on the network.
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

- The conversation lays out every message on each frame. Virtualize it once
  history loading arrives.
