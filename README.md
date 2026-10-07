# fastcord

**Discord, native and fast.** A lightweight Discord client written in Rust with
[egui](https://github.com/emilk/egui), built on
[fastframe](https://github.com/crmne/fastframe). No browser engine: when
Discord asks for a captcha, a small separate helper shows it, and only then.

Early work in progress. The first version covers text only: servers, channels,
direct messages, history, sending, reactions and notifications.

## Privacy

- Nothing phones home: no analytics, no `/science` events.
- Messages live in memory only; nothing is written to disk.
- The session token is kept in the system keyring, never in a plain file.
- Logs never contain message content.
- Desktop notifications show a message's text unless "Notification previews"
  is off; your desktop's notification server may keep its own history.

## Developing

```sh
cargo run -- --demo   # offline sample servers, no Discord connection
```

## Disclaimer

fastcord is an unofficial client and is not affiliated with Discord. Using an
unofficial client may be against Discord's terms of service and could get an
account suspended. Use it at your own risk.

## License

MIT
