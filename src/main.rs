//! fastcord: Discord, native and fast.

mod acks;
mod api;
mod app;
mod backend;
mod captcha;
mod compose;
mod credentials;
mod demo;
mod events;
mod gateway;
mod markdown;
mod media;
mod model;
mod notify;
mod outbox;
mod remote_auth;
mod theme;
mod ui;
mod websocket;

use clap::Parser;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Show offline sample servers and messages instead of connecting.
    #[arg(long)]
    demo: bool,
    /// Log debug details (never message contents).
    #[arg(long, short)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let dirs = directories::ProjectDirs::from("", "", "fastcord");

    let filter = if cli.verbose {
        "warn,fastcord=debug"
    } else {
        "warn,fastcord=info"
    };
    let mut logging =
        fastframe_log::Logging::new("fastcord", env!("CARGO_PKG_VERSION")).filter(filter);
    // Demo runs log to stderr only.
    if let Some(state) = dirs
        .as_ref()
        .and_then(|d| d.state_dir())
        .filter(|_| !cli.demo)
    {
        logging = logging
            .file(state.join("fastcord.log"))
            .panic_log(state.join("panics.log"))
            .panic_message(fastframe_log::PanicMessage::Redacted(
                fastframe_log::redact::links,
            ));
    }
    if let Err(error) = logging.init() {
        eprintln!("not logging: {error}");
    }

    fastframe_emoji::EmojiSetup::default()
        .system(true)
        .install();
    std::thread::spawn(fastframe_emoji::warm_up);

    let model = cli.demo.then(demo::model);
    // Demo runs follow the desktop's palette too.
    let themes_dir = dirs.map(|d| d.config_dir().join("themes"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("fastcord")
            .with_title("fastcord")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "fastcord",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(&cc.egui_ctx, model, themes_dir)))),
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
}
