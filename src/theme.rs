//! Palette, fonts, icons, and egui's own style.
//!
//! Colours use the sixteen base names every fastframe app reads, so Omarchy's
//! rendering of the base template and the shared palettes fit as they are.

use egui::{Color32, CornerRadius, FontId, Stroke};

/// The palette files, and the Omarchy palette where the desktop has one.
pub type Catalog = fastframe_theme::Catalog<Palette>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub window: Color32,
    pub panel: Color32,
    pub surface: Color32,
    pub surface_hover: Color32,
    pub surface_active: Color32,
    pub outline: Color32,
    pub text: Color32,
    pub secondary: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub on_accent: Color32,
    pub danger: Color32,
    pub warning: Color32,
    pub overlay: Color32,
    pub shadow: Color32,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            dark: true,
            window: Color32::from_rgb(0x1e, 0x1f, 0x24),
            panel: Color32::from_rgb(0x17, 0x18, 0x1c),
            surface: Color32::from_rgb(0x26, 0x28, 0x2e),
            surface_hover: Color32::from_rgb(0x2e, 0x30, 0x37),
            surface_active: Color32::from_rgb(0x38, 0x3a, 0x42),
            outline: Color32::from_rgb(0x30, 0x32, 0x39),
            text: Color32::from_rgb(0xf0, 0xf1, 0xf3),
            secondary: Color32::from_rgb(0xb0, 0xb4, 0xbc),
            dim: Color32::from_rgb(0x74, 0x78, 0x82),
            accent: Color32::from_rgb(0x58, 0x65, 0xf2),
            accent_hover: Color32::from_rgb(0x6d, 0x78, 0xf5),
            on_accent: Color32::WHITE,
            danger: Color32::from_rgb(0xf2, 0x3f, 0x43),
            warning: Color32::from_rgb(0xf0, 0xb2, 0x32),
            overlay: Color32::from_rgb(0x26, 0x28, 0x2e),
            shadow: Color32::from_black_alpha(140),
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            window: Color32::from_rgb(0xff, 0xff, 0xff),
            panel: Color32::from_rgb(0xf2, 0xf3, 0xf5),
            surface: Color32::from_rgb(0xeb, 0xed, 0xef),
            surface_hover: Color32::from_rgb(0xe0, 0xe2, 0xe6),
            surface_active: Color32::from_rgb(0xd4, 0xd7, 0xdc),
            outline: Color32::from_rgb(0xdd, 0xe0, 0xe4),
            text: Color32::from_rgb(0x1f, 0x21, 0x26),
            secondary: Color32::from_rgb(0x4e, 0x53, 0x5c),
            dim: Color32::from_rgb(0x80, 0x85, 0x8e),
            accent: Color32::from_rgb(0x58, 0x65, 0xf2),
            accent_hover: Color32::from_rgb(0x47, 0x52, 0xc4),
            on_accent: Color32::WHITE,
            danger: Color32::from_rgb(0xd8, 0x3c, 0x3e),
            warning: Color32::from_rgb(0xb8, 0x7a, 0x14),
            overlay: Color32::WHITE,
            shadow: Color32::from_black_alpha(50),
        }
    }
}

impl fastframe_theme::Palette for Palette {
    fn base(base: fastframe_theme::Base) -> Self {
        match base {
            fastframe_theme::Base::Dark => Self::dark(),
            fastframe_theme::Base::Light => Self::light(),
        }
    }

    fn set(&mut self, name: &str, color: Color32) -> bool {
        match name {
            "window" => self.window = color,
            "panel" => self.panel = color,
            "surface" => self.surface = color,
            "surface_hover" => self.surface_hover = color,
            "surface_active" => self.surface_active = color,
            "outline" => self.outline = color,
            "text" => self.text = color,
            "secondary" => self.secondary = color,
            "dim" => self.dim = color,
            "accent" => self.accent = color,
            "accent_hover" => self.accent_hover = color,
            "on_accent" => self.on_accent = color,
            "danger" => self.danger = color,
            "warning" => self.warning = color,
            "overlay" => self.overlay = color,
            "shadow" => self.shadow = color,
            _ => return false,
        }
        true
    }
}

/// Follows Omarchy's palette where the desktop has one.
pub fn enable_desktop_themes(catalog: &mut Catalog) {
    catalog.enable_desktop_themes(fastframe_theme::DesktopThemes {
        slug: "fastcord",
        omarchy_template: fastframe_theme::omarchy::BASE_TEMPLATE,
        omarchy_previous_templates: &[],
        // No theme picker yet: only the desktop's palette is used.
        presets: false,
    });
}

fastframe_icons::icons! {
    /// Every icon the interface draws.
    pub enum Icon {
        prefix: "fastcord-icon-",
        directory: "../assets/icons/",
        Alert => lucide "circle-alert",
        Download => "download",
        File => "file",
        Hash => "hash",
        Megaphone => "megaphone",
        MessageCircle => "message-circle",
        Volume => lucide "volume-2",
    }
}

/// How the desktop renders text, read once per process: `detect` blocks on a
/// D-Bus call. Tests use the platform default so they do not depend on the
/// machine.
fn text_rendering() -> fastframe_text::TextRendering {
    static RENDERING: std::sync::OnceLock<fastframe_text::TextRendering> =
        std::sync::OnceLock::new();
    *RENDERING.get_or_init(|| {
        if cfg!(test) {
            fastframe_text::TextRendering::platform_default()
        } else {
            fastframe_text::detect()
        }
    })
}

/// Fonts, image loaders, icons and colour emoji, once per context.
pub fn install(ctx: &egui::Context) {
    let mut fonts = fastframe_fonts::FontSetup::default().definitions();
    text_rendering().apply_to(&mut fonts);
    ctx.set_fonts(fonts);
    egui_extras::install_image_loaders(ctx);
    fastframe_icons::install::<Icon>(ctx);
    ctx.add_plugin(fastframe_emoji::EmojiPlugin::default());
}

/// Applies the palette to egui's own widgets.
pub fn apply(ctx: &egui::Context, palette: &Palette) {
    let mut style = (*ctx.global_style()).clone();
    let mut visuals = if palette.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.override_text_color = Some(palette.text);
    visuals.panel_fill = palette.window;
    visuals.window_fill = palette.overlay;
    visuals.window_stroke = Stroke::new(1.0, palette.outline);
    visuals.extreme_bg_color = palette.surface;
    visuals.faint_bg_color = palette.panel;
    visuals.hyperlink_color = palette.accent;
    visuals.selection.bg_fill = palette.accent.gamma_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0, palette.on_accent);
    visuals.error_fg_color = palette.danger;
    visuals.warn_fg_color = palette.warning;
    let radius = CornerRadius::same(6);
    for (widget, fill) in [
        (&mut visuals.widgets.noninteractive, palette.window),
        (&mut visuals.widgets.inactive, palette.surface),
        (&mut visuals.widgets.hovered, palette.surface_hover),
        (&mut visuals.widgets.active, palette.surface_active),
        (&mut visuals.widgets.open, palette.surface_active),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::NONE;
        widget.fg_stroke = Stroke::new(1.0, palette.text);
        widget.corner_radius = radius;
    }
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.outline);
    text_rendering().apply_to_visuals(&mut visuals);
    style.visuals = visuals;
    style.interaction.selectable_labels = false;
    ctx.set_global_style(style);
}

pub fn regular(size: f32) -> FontId {
    fastframe_fonts::Weight::Regular.font_id(size)
}

pub fn semibold(size: f32) -> FontId {
    fastframe_fonts::Weight::SemiBold.font_id(size)
}

pub fn bold(size: f32) -> FontId {
    fastframe_fonts::Weight::Bold.font_id(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastframe_theme::Palette as _;

    #[test]
    fn every_base_colour_is_settable() {
        let mut palette = Palette::dark();
        for name in fastframe_theme::BASE_COLORS {
            assert!(palette.set(name, Color32::RED), "{name}");
        }
        assert!(!palette.set("bubble_in", Color32::RED));
    }
}
