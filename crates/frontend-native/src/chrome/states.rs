//! Save-state management: the list, load, and delete.
//!
//! The four behaviours prompt 14 names as proven-good in the predecessor and worth keeping: list
//! them, load one back to its exact frame, delete from the UI *and* the disk, and organise them per
//! ROM. The frame number is shown next to each entry because "load to exact frame" is only a
//! meaningful promise if the frame is visible — and it is the frontend's own counter, restored with
//! the state, so what the list says is what the HUD will read after loading.

use super::{bytes, timestamp, Chrome, ChromeState, UiAction};
use frontend_core::SessionCommand;

/// Numbered quick-save slots offered in the UI.
///
/// Ten is the convention and it is enough that nobody runs out mid-session. Slots are overwritten
/// in place; named states accumulate, which is the distinction between a scratchpad and a keepsake.
const SLOTS: u8 = 10;

pub fn window(
    chrome: &mut Chrome,
    ctx: &egui::Context,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    let mut open = chrome.show_states;
    egui::Window::new("Save states")
        .open(&mut open)
        .default_width(460.0)
        .show(ctx, |ui| {
            let Some(rom) = state.loaded else {
                ui.label("No cartridge is loaded.");
                return;
            };
            ui.label(egui::RichText::new(&rom.title).strong());
            if rom.rom_id.is_none() {
                ui.label(
                    egui::RichText::new(
                        "This ROM is not in the library, so its states are written to disk but \
                         will not be listed here. Import it to keep track of them.",
                    )
                    .small()
                    .weak(),
                );
            }
            ui.separator();

            slots(ui, state, actions);
            ui.separator();
            named(chrome, ui, actions);
            ui.separator();
            listing(ui, state, actions);
        });
    chrome.show_states = open;
}

fn slots(ui: &mut egui::Ui, state: &ChromeState<'_>, actions: &mut Vec<UiAction>) {
    ui.label(egui::RichText::new("Quick-save slots").small().weak());
    egui::Grid::new("slots").num_columns(5).show(ui, |ui| {
        for slot in 0..SLOTS {
            let occupied = state.states.iter().find(|entry| entry.slot == Some(slot));
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    if ui.button(format!("Save {slot}")).clicked() {
                        actions.push(UiAction::Session(SessionCommand::SaveState {
                            slot: Some(slot),
                            label: None,
                        }));
                    }
                    // Loading an empty slot is disabled rather than reported as an error, so the
                    // UI never invites a click it will refuse.
                    if ui
                        .add_enabled(occupied.is_some(), egui::Button::new("Load"))
                        .clicked()
                    {
                        actions.push(UiAction::Session(SessionCommand::LoadSlot(slot)));
                    }
                });
                ui.label(
                    egui::RichText::new(match occupied {
                        Some(entry) => format!("frame {}", entry.frame),
                        None => "empty".to_string(),
                    })
                    .small()
                    .weak(),
                );
            });
            if slot % 5 == 4 {
                ui.end_row();
            }
        }
    });
}

fn named(chrome: &mut Chrome, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut chrome.state_label)
                .hint_text("Name a state to keep")
                .desired_width(220.0),
        );
        let label = chrome.state_label.trim().to_string();
        if ui
            .add_enabled(!label.is_empty(), egui::Button::new("Save named"))
            .clicked()
        {
            actions.push(UiAction::SaveNamed(label));
            chrome.state_label.clear();
        }
    });
}

fn listing(ui: &mut egui::Ui, state: &ChromeState<'_>, actions: &mut Vec<UiAction>) {
    if state.states.is_empty() {
        ui.label(
            egui::RichText::new("No save states for this cartridge yet.")
                .small()
                .weak(),
        );
        return;
    }

    egui::ScrollArea::vertical()
        .max_height(240.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for entry in state.states {
                ui.push_id(entry.id, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&entry.label).strong());
                        ui.label(
                            egui::RichText::new(format!(
                                "frame {} · {} · {}",
                                entry.frame,
                                bytes(entry.size_bytes as usize),
                                timestamp(entry.created_at)
                            ))
                            .small()
                            .weak(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("Delete")
                                .on_hover_text(
                                    "Removes the state from this list and deletes the file.",
                                )
                                .clicked()
                            {
                                actions.push(UiAction::DeleteState(entry.id));
                            }
                            if ui.button("Load").clicked() {
                                actions.push(UiAction::Session(SessionCommand::LoadState {
                                    path: entry.path.clone(),
                                }));
                            }
                        });
                    });
                });
            }
        });
}
