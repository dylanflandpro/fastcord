//! The window: the model, what is open, and the palette it is drawn in.

use crate::model::{Id, Model};
use crate::theme::{self, Catalog, Palette};
use std::path::PathBuf;

/// Which list the middle column shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum View {
    DirectMessages,
    Guild(Id),
}

/// What is open: the list in the middle column and the conversation. Kept
/// apart from the model so the interface can change it while drawing from
/// the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub view: View,
    pub channel: Option<Id>,
}

impl Selection {
    /// Where the window opens: the first guild, or the DMs without one.
    pub fn initial(model: Option<&Model>) -> Self {
        let mut selection = Self {
            view: View::DirectMessages,
            channel: None,
        };
        if let Some(model) = model {
            match model.guilds.first() {
                Some(guild) => selection.open_guild(model, guild.id),
                None => selection.open_direct_messages(model),
            }
        }
        selection
    }

    pub fn open_guild(&mut self, model: &Model, id: Id) {
        self.view = View::Guild(id);
        self.channel = model.guild(id).and_then(|g| g.first_text_channel());
    }

    pub fn open_direct_messages(&mut self, model: &Model) {
        self.view = View::DirectMessages;
        self.channel = model.dms_by_recency().first().map(|d| d.id);
    }
}

pub struct App {
    /// `None` until the account is connected.
    pub model: Option<Model>,
    pub selection: Selection,
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

        let mut themes = Catalog::default();
        let repaint = ctx.clone();
        let waker = fastframe_theme::Waker::new(move || repaint.request_repaint());
        if let Some(dir) = &themes_dir {
            theme::enable_desktop_themes(&mut themes);
            themes.start(dir.clone(), None, &waker);
        }

        Self {
            selection: Selection::initial(model.as_ref()),
            model,
            palette,
            themes,
            themes_dir,
            waker,
            transition: fastframe_theme::Transition::default(),
            wanted: palette,
            first_palette: true,
        }
    }

    /// Picks up a new desktop palette and reveals it.
    fn follow_theme(&mut self, ctx: &egui::Context) {
        if self.themes.needs_reload()
            && let Some(dir) = &self.themes_dir
        {
            self.themes.start(dir.clone(), None, &self.waker);
        }
        if self.themes.poll() {
            // A scan without a desktop palette (none at all, or one caught
            // half-written mid-switch) keeps the current one rather than
            // flashing the built-in palette.
            if let Some(theme) = self.themes.system_theme() {
                self.wanted = theme.palette;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_on_the_first_guild_and_its_first_text_channel() {
        let model = crate::demo::model();
        let selection = Selection::initial(Some(&model));
        assert_eq!(selection.view, View::Guild(100));
        assert_eq!(selection.channel, Some(101));
    }

    #[test]
    fn switching_lists_opens_a_conversation() {
        let model = crate::demo::model();
        let mut selection = Selection::initial(Some(&model));
        selection.open_guild(&model, 200);
        assert_eq!(selection.channel, Some(201));
        // The DM with the most recent message.
        selection.open_direct_messages(&model);
        assert_eq!(selection.view, View::DirectMessages);
        assert_eq!(selection.channel, Some(900));
    }

    #[test]
    fn without_an_account_nothing_is_open() {
        let selection = Selection::initial(None);
        assert_eq!(selection.view, View::DirectMessages);
        assert_eq!(selection.channel, None);
    }
}
