//! The in-game HUD overlay.
//!
//! Everything here is a *measured* number from [`SessionStats`](frontend_core::SessionStats).
//! There is deliberately no nominal "60 fps" anywhere: the point of a HUD is to tell you when the
//! emulator is not keeping up, and a figure that reads 100% regardless answers the one question it
//! exists to answer with a lie.
//!
//! Toggled by its own key, never by an emulated button — prompt 10's precedence rule. Overloading
//! Select onto "show HUD" is how an emulator ends up with a control the player cannot use in-game.

use super::{bytes, ChromeState};
use frontend_core::SessionStatus;

pub fn overlay(ctx: &egui::Context, state: &ChromeState<'_>) {
    egui::Area::new("hud".into())
        .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 12.0))
        // Not interactable: the HUD must never swallow a click meant for the game beneath it.
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(190.0);
                let stats = state.stats;

                if let Some(rom) = state.loaded {
                    ui.label(egui::RichText::new(&rom.title).strong());
                    ui.label(
                        egui::RichText::new(rom.platform.display_name())
                            .small()
                            .weak(),
                    );
                    ui.separator();
                }

                let speed_colour = if stats.speed_percent >= 95.0 {
                    ui.visuals().text_color()
                } else if stats.speed_percent >= 80.0 {
                    egui::Color32::from_rgb(0xFF, 0xD5, 0x4F)
                } else {
                    egui::Color32::from_rgb(0xFF, 0x8A, 0x80)
                };
                row(ui, "speed", |ui| {
                    ui.colored_label(
                        speed_colour,
                        format!("{:.0}%  ({:.1} fps)", stats.speed_percent, stats.fps),
                    );
                });
                row(ui, "frame", |ui| {
                    ui.label(format!("{}", stats.frame));
                });

                let mode = match state.status {
                    SessionStatus::Paused => Some("paused"),
                    SessionStatus::Stopped => Some("stopped by the machine"),
                    _ if stats.rewinding => Some("rewinding"),
                    _ if stats.fast_forward => Some("fast-forward"),
                    _ => None,
                };
                if let Some(mode) = mode {
                    row(ui, "state", |ui| {
                        ui.label(egui::RichText::new(mode).strong());
                    });
                }

                ui.separator();
                if state.config.rewind.enabled {
                    row(ui, "rewind", |ui| {
                        // Seconds, not snapshots: "12 s" is the thing a player wants to know, and
                        // it is derived from the frames the buffer actually spans rather than from
                        // the configured depth.
                        let seconds = stats.rewind_span_frames as f64
                            / frontend_core::frame_rate(
                                state.loaded.map(|rom| rom.platform).unwrap_or(
                                    // Only reached with no ROM loaded, where the span is zero
                                    // anyway and the rate cannot affect the result.
                                    library::Platform::Gb,
                                ),
                            );
                        ui.label(format!("{seconds:.1} s · {}", bytes(stats.rewind_bytes)));
                    });
                }

                // Dropped counts are only shown once they are non-zero. A HUD line reading "0"
                // forever trains the eye to ignore it, which is the opposite of what a warning
                // indicator is for.
                if stats.frames_dropped > 0 {
                    row(ui, "frames dropped", |ui| {
                        ui.label(format!("{}", stats.frames_dropped));
                    });
                }
                if stats.audio_dropped > 0 {
                    row(ui, "audio dropped", |ui| {
                        ui.label(format!("{}", stats.audio_dropped));
                    })
                    .on_hover_text(
                        "Samples the ring could not accept. Expected during fast-forward; \
                         at normal speed it means the output rate is wrong.",
                    );
                }
            });
        });
}

fn row<R>(
    ui: &mut egui::Ui,
    label: &str,
    value: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::Response {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).small().weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), value);
    })
    .response
}
