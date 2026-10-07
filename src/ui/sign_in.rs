//! The screen before the account is connected: the QR code, then who is
//! signing in, then the account.

use crate::app::App;
use crate::backend::{Command, Session};
use crate::theme::{self, Palette};
use egui::{Color32, CornerRadius, Frame, Rect, RichText, Vec2};

const QR_SIZE: f32 = 220.0;
/// The quiet zone scanners need around the code, in modules.
const QUIET_ZONE: usize = 2;

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let mut command = None;
    egui::CentralPanel::default()
        .frame(Frame::new().fill(palette.window))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() / 2.0 - QR_SIZE).max(24.0));
                match &app.session {
                    Session::Checking => {
                        ui.spinner();
                        ui.add_space(12.0);
                        secondary(ui, &palette, "Connecting…");
                    }
                    Session::Qr(_) => {
                        title(ui, &palette, "Log in with QR Code");
                        secondary(
                            ui,
                            &palette,
                            "Scan this with the Discord mobile app to log in instantly.",
                        );
                        ui.add_space(20.0);
                        match &app.qr {
                            Some(code) => qr(ui, code),
                            None => secondary(ui, &palette, "This code could not be drawn."),
                        }
                    }
                    Session::Scanned { username } => {
                        title(ui, &palette, "Check your phone!");
                        secondary(
                            ui,
                            &palette,
                            &format!("Log in on your phone to continue as {username}."),
                        );
                    }
                    Session::Captcha => {
                        title(ui, &palette, "Are you human?");
                        secondary(
                            ui,
                            &palette,
                            "Discord asks for a captcha. Complete it in the window that just opened.",
                        );
                    }
                    Session::SignedIn(user) => {
                        ui.spinner();
                        ui.add_space(12.0);
                        secondary(
                            ui,
                            &palette,
                            &format!("Loading {}'s servers…", user.display_name()),
                        );
                        ui.add_space(20.0);
                        if ui.button("Log out").clicked() {
                            command = Some(Command::LogOut);
                        }
                    }
                    Session::Stopped => {
                        title(ui, &palette, "Something went wrong");
                        secondary(
                            ui,
                            &palette,
                            "fastcord stopped talking to Discord after an internal error. Restart it to try again.",
                        );
                    }
                    Session::Failed(message) => {
                        title(ui, &palette, "Something went wrong");
                        secondary(ui, &palette, message);
                        ui.add_space(20.0);
                        if ui.button("Try again").clicked() {
                            command = Some(Command::Retry);
                        }
                    }
                }
            });
        });
    if let Some(command) = command {
        app.send(command);
    }
}

fn title(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::semibold(22.0))
            .color(palette.text),
    );
    ui.add_space(8.0);
}

fn secondary(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::regular(15.0))
            .color(palette.secondary),
    );
}

/// Dark modules on white whatever the theme: phone scanners expect that.
fn qr(ui: &mut egui::Ui, code: &qrcode::QrCode) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(QR_SIZE), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), Color32::WHITE);
    let width = code.width();
    let module = QR_SIZE / (width + 2 * QUIET_ZONE) as f32;
    let origin = rect.min + Vec2::splat(module * QUIET_ZONE as f32);
    for (index, color) in code.to_colors().into_iter().enumerate() {
        if color == qrcode::Color::Dark {
            let (x, y) = ((index % width) as f32, (index / width) as f32);
            // A hair of overlap so neighbouring modules leave no seams.
            let min = origin + Vec2::new(x, y) * module;
            painter.rect_filled(
                Rect::from_min_size(min, Vec2::splat(module + 0.5)),
                CornerRadius::ZERO,
                Color32::BLACK,
            );
        }
    }
}
