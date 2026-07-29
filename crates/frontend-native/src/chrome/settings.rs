//! The settings panel.
//!
//! Every control here maps to one field of [`Config`](frontend_core::Config), and nothing here can
//! change how a machine behaves — see that module for why that boundary is drawn hard. Changes take
//! effect immediately and are written to the TOML file when the application exits, so a session
//! spent adjusting the volume does not also spend it writing the config file.
//!
//! The rewind figures are shown as a *measured* memory cost rather than a promise, because the
//! trade prompt 14 asks to be configurable is depth against memory, and a user cannot make that
//! trade without seeing both numbers.

use super::{bytes, Chrome, ChromeState, UiAction};
use frontend_core::ScalingMode;

pub fn window(
    chrome: &mut Chrome,
    ctx: &egui::Context,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    let mut open = chrome.show_settings;
    egui::Window::new("Settings")
        .open(&mut open)
        .default_width(430.0)
        .show(ctx, |ui| {
            video(ui, state, actions);
            ui.separator();
            audio(ui, state, actions);
            ui.separator();
            emulation(ui, state, actions);
            ui.separator();
            rewind(ui, state, actions);
            ui.separator();
            about(ui, state);
        });
    chrome.show_settings = open;
}

fn video(ui: &mut egui::Ui, state: &ChromeState<'_>, actions: &mut Vec<UiAction>) {
    ui.label(egui::RichText::new("Video").strong());
    ui.horizontal(|ui| {
        ui.label("Scaling");
        for mode in ScalingMode::ALL {
            if ui
                .selectable_label(state.config.video.scaling == *mode, mode.label())
                .on_hover_text(hint(*mode))
                .clicked()
            {
                actions.push(UiAction::SetScaling(*mode));
            }
        }
    });

    let mut fullscreen = state.fullscreen;
    if ui.checkbox(&mut fullscreen, "Fullscreen").changed() {
        actions.push(UiAction::ToggleFullscreen);
    }

    let mut gap = state.config.video.dual_screen_gap;
    if ui
        .add(
            egui::Slider::new(&mut gap, 0..=64)
                .text("Dual-screen gap")
                .suffix(" px"),
        )
        .changed()
    {
        actions.push(UiAction::SetDualScreenGap(gap));
    }
    ui.label(
        egui::RichText::new(
            "The gap between the Nintendo DS's two screens, in emulated pixels, so it scales \
             with the picture.",
        )
        .small()
        .weak(),
    );
}

fn audio(ui: &mut egui::Ui, state: &ChromeState<'_>, actions: &mut Vec<UiAction>) {
    ui.label(egui::RichText::new("Audio").strong());

    let mut volume = state.config.audio.volume;
    if ui
        .add(egui::Slider::new(&mut volume, 0.0..=1.0).text("Volume"))
        .changed()
    {
        actions.push(UiAction::SetVolume(volume));
    }
    let mut muted = state.config.audio.muted;
    if ui.checkbox(&mut muted, "Mute").changed() {
        actions.push(UiAction::SetMuted(muted));
    }
    ui.label(
        egui::RichText::new(format!("Device: {}", state.audio_description))
            .small()
            .weak(),
    );
    if state.audio_description == "no output device" {
        ui.label(
            egui::RichText::new(
                "No audio device was opened, so the emulator is running silently. \
                 Everything else works.",
            )
            .small()
            .weak(),
        );
    }
}

fn emulation(ui: &mut egui::Ui, state: &ChromeState<'_>, actions: &mut Vec<UiAction>) {
    ui.label(egui::RichText::new("Emulation").strong());

    let mut speed = state.config.emulation.fast_forward_speed;
    if ui
        .add(
            egui::Slider::new(&mut speed, 0.0..=16.0)
                .text("Fast-forward speed")
                .custom_formatter(|value, _| {
                    if value <= 0.0 {
                        "uncapped".to_string()
                    } else {
                        format!("{value:.1}×")
                    }
                }),
        )
        .changed()
    {
        actions.push(UiAction::SetFastForwardSpeed(speed));
    }
    ui.label(
        egui::RichText::new(
            "A finite multiplier keeps the sound, pitched up like a tape running fast. \
             Uncapped runs as fast as the machine manages and is silent, because there is no \
             fixed ratio to resample by.",
        )
        .small()
        .weak(),
    );

    let mut pause_on_focus_loss = state.config.emulation.pause_on_focus_loss;
    if ui
        .checkbox(
            &mut pause_on_focus_loss,
            "Pause when the window loses focus",
        )
        .changed()
    {
        actions.push(UiAction::SetPauseOnFocusLoss(pause_on_focus_loss));
    }
}

fn rewind(ui: &mut egui::Ui, state: &ChromeState<'_>, actions: &mut Vec<UiAction>) {
    ui.label(egui::RichText::new("Rewind").strong());
    let current = state.config.rewind;
    let mut next = current;

    ui.checkbox(&mut next.enabled, "Enable rewind");
    ui.add_enabled_ui(next.enabled, |ui| {
        ui.add(
            egui::Slider::new(&mut next.seconds, 1..=300)
                .text("Depth")
                .suffix(" s"),
        );
        ui.add(
            egui::Slider::new(&mut next.interval_frames, 1..=60)
                .text("Snapshot every")
                .suffix(" frames"),
        );
    });

    // The cost the user is actually choosing. `rewind_bytes` is what the buffer holds right now, so
    // it is a measurement of this ROM rather than an estimate for an average one — and dividing it
    // by the snapshots held gives a per-snapshot size that projects honestly to a full buffer.
    let stats = state.stats;
    if let Some(per_snapshot) = stats.rewind_bytes.checked_div(stats.rewind_snapshots) {
        let projected = per_snapshot
            * next.snapshot_capacity(
                state
                    .loaded
                    .map(|rom| frontend_core::frame_rate(rom.platform))
                    .unwrap_or(59.7275),
            );
        ui.label(
            egui::RichText::new(format!(
                "Holding {} in {} snapshots. At these settings a full buffer would be about {}.",
                bytes(stats.rewind_bytes),
                stats.rewind_snapshots,
                bytes(projected)
            ))
            .small()
            .weak(),
        );
    } else {
        ui.label(
            egui::RichText::new("Load a cartridge to see what rewind costs for it.")
                .small()
                .weak(),
        );
    }

    if next != current {
        // Changing the depth restarts the ring, which discards the history the player has. Saying
        // so is better than a silent loss they discover the next time they hold the rewind key.
        if next.enabled != current.enabled
            || next.seconds != current.seconds
            || next.interval_frames != current.interval_frames
        {
            actions.push(UiAction::SetRewind(next));
        }
    }
    if next.enabled {
        ui.label(
            egui::RichText::new("Changing these clears the history recorded so far.")
                .small()
                .weak(),
        );
    }
}

fn about(ui: &mut egui::Ui, state: &ChromeState<'_>) {
    ui.collapsing("About", |ui| {
        ui.label(
            egui::RichText::new(format!("Alpha Emulator {}", env!("CARGO_PKG_VERSION"))).strong(),
        );
        ui.label(
            egui::RichText::new(state.gpu_description)
                .small()
                .monospace(),
        );
        ui.label(
            egui::RichText::new(
                "Settings and keybinds are written to a TOML file you can edit by hand; \
                 the library index is SQLite.",
            )
            .small()
            .weak(),
        );
    });
}

fn hint(mode: ScalingMode) -> &'static str {
    match mode {
        ScalingMode::Nearest => {
            "Fills the window, keeping the aspect ratio. Some pixel rows end up one screen pixel \
             taller than their neighbours."
        }
        ScalingMode::IntegerNearest => {
            "Every emulated pixel is exactly the same size on screen, with a border. The sharpest \
             option, and the only one with no shimmer when the picture scrolls."
        }
        ScalingMode::Linear => "Bilinear filtering. Softer, and not how the hardware looked.",
    }
}

/// Rewind is the only setting here that changes what the emulation thread allocates, so it is worth
/// asserting the projection is arithmetic and not a guess.
#[cfg(test)]
mod tests {
    use frontend_core::RewindConfig;

    #[test]
    fn a_deeper_buffer_projects_a_larger_cost() {
        let shallow = RewindConfig {
            enabled: true,
            seconds: 10,
            interval_frames: 6,
        };
        let deep = RewindConfig {
            seconds: 60,
            ..shallow
        };
        assert!(deep.snapshot_capacity(59.7275) > shallow.snapshot_capacity(59.7275));
    }

    #[test]
    fn a_longer_interval_projects_a_smaller_cost() {
        let dense = RewindConfig {
            enabled: true,
            seconds: 30,
            interval_frames: 2,
        };
        let sparse = RewindConfig {
            interval_frames: 30,
            ..dense
        };
        assert!(sparse.snapshot_capacity(59.7275) < dense.snapshot_capacity(59.7275));
    }
}
