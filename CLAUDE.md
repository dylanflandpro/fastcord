# fastcord

A native Discord client: Rust, egui 0.36 (glow), fastframe. Text only for now.

## Loop

- `cargo run -- --demo` shows the interface on offline sample data (`src/demo.rs`).
  Sign-in does not exist yet, so the demo is the only way to see the app.
- Done means the CI steps in `.github/workflows/ci.yml` pass locally, `--locked` included.

## Layout

- `model.rs`: Discord's objects and the rules that order them (sidebar, DM recency,
  message grouping). It has no egui types: put behaviour here and test it here.
- `app.rs`: `App` holds the `Model`, the `Selection` (what is open) and the palette.
  Navigation lives on `Selection`, which takes `&Model`, so `ui.rs` draws from
  `&app.model` while it changes `app.selection`. Keep the model borrowed in place.
- `ui.rs`: drawing only. Turn any decision it makes into a function in `model.rs`
  or a method on `Selection`, with a test.
- `theme.rs`: the palette, which follows Omarchy's through `fastframe-theme`.

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

## Known gaps

- The conversation lays out every message on each frame. Virtualize it once
  history loading arrives.
