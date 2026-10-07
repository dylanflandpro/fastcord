//! The window: the model, what is selected, and the palette it is drawn in.

use crate::model::{Id, Model};
use crate::theme::{self, Catalog, Palette};
use std::path::PathBuf;

/// Which list the middle column shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    DirectMessages,
    Guild(Id),
}

pub struct App {
    /// `None` until the account is connected.
    pub model: Option<Model>,
    pub view: View,
    pub channel: Option<Id>,
    pub palette: Palette,
    themes: Catalog,
    themes_dir: Option<PathBuf>,
    waker: fastframe_theme::Waker,
    transition: fastframe_theme::Transition,
    /// The palette to draw in once the transition lets it in.
    wanted: Palette,
    /// Whether a desktop palette has arrived yet: the first one is applied at
    /// once, later ones are revealed as Omarchy does.
    first_palette: bool,
}

impl App {
    /// `themes_dir` is where palette files live; `None` keeps the built-in
    /// palette and never reads the desktop.
    pub fn new(ctx: &egui::Context, model: Option<Model>, themes_dir: Option<PathBuf>) -> Self {
        theme::install(ctx);
        let palette = Palette::dark();
        theme::apply(ctx, &palette);

        let mut themes = Catalog::preview(Vec::new(), false);
        let repaint = ctx.clone();
        let waker = fastframe_theme::Waker::new(move || repaint.request_repaint());
        if let Some(dir) = &themes_dir {
            theme::enable_desktop_themes(&mut themes);
            themes.start(dir.clone(), None, &waker);
        }

        let mut app = Self {
            model,
            view: View::DirectMessages,
            channel: None,
            palette,
            themes,
            themes_dir,
            waker,
            transition: fastframe_theme::Transition::default(),
            wanted: palette,
            first_palette: true,
        };
        if let Some(guild) = app
            .model
            .as_ref()
            .and_then(|m| m.guilds.first())
            .map(|g| g.id)
        {
            app.open_guild(guild);
        }
        app
    }

    pub fn open_guild(&mut self, id: Id) {
        self.view = View::Guild(id);
        self.channel = self
            .model
            .as_ref()
            .and_then(|m| m.guild(id))
            .and_then(|g| g.first_text_channel());
    }

    pub fn open_direct_messages(&mut self) {
        self.view = View::DirectMessages;
        self.channel = self
            .model
            .as_ref()
            .and_then(|m| m.dms_by_recency().first().map(|d| d.id));
    }

    /// Picks up a new desktop palette and reveals it.
    fn follow_theme(&mut self, ctx: &egui::Context) {
        if self.themes.needs_reload()
            && let Some(dir) = &self.themes_dir
        {
            self.themes.start(dir.clone(), None, &self.waker);
        }
        if self.themes.poll() {
            self.wanted = self
                .themes
                .system_theme()
                .map_or_else(Palette::dark, |theme| theme.palette);
            if std::mem::replace(&mut self.first_palette, false) {
                self.set_palette(ctx, self.wanted);
            }
        }
        if self.palette != self.wanted {
            self.transition.begin(ctx);
            if !self.transition.holding(ctx) {
                self.set_palette(ctx, self.wanted);
            }
        }
    }

    fn set_palette(&mut self, ctx: &egui::Context, palette: Palette) {
        self.palette = palette;
        theme::apply(ctx, &palette);
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.follow_theme(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        crate::ui::show(self, ui);
        self.transition.paint(ui.ctx());
    }
}
