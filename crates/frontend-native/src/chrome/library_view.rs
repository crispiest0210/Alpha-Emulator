//! The ROM library browser.
//!
//! Shows what the index knows, which is more than a directory listing can: a title the user
//! corrected, when each game was last played, and whether the file is still where it was. A ROM on
//! an unmounted drive appears greyed out and keeps its history rather than disappearing — that
//! visible distinction between "gone" and "not indexed" is the user-facing half of predecessor
//! lesson §5.

use super::{timestamp, Chrome, ChromeState, UiAction};
use library::Platform;

pub fn panel(
    chrome: &mut Chrome,
    root: &mut egui::Ui,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    egui::Panel::left("library")
        .resizable(true)
        .default_size(380.0)
        .size_range(280.0..=720.0)
        .show(root, |ui| {
            ui.add_space(4.0);
            ui.heading("Library");

            if let Some(error) = state.library_error {
                ui.colored_label(egui::Color32::from_rgb(0xFF, 0x8A, 0x80), error);
            }

            import_box(chrome, ui, actions);
            ui.separator();
            filters(chrome, ui, state);
            ui.separator();
            rom_list(chrome, ui, state, actions);
        });
}

fn import_box(chrome: &mut Chrome, ui: &mut egui::Ui, actions: &mut Vec<UiAction>) {
    ui.add_space(4.0);
    // Drag-and-drop is the primary route and needs no widget; the text box is the fallback for a
    // path pasted from a terminal, and it is here rather than behind a native file dialog because
    // a file-dialog crate would be a dependency for one button.
    ui.label(
        egui::RichText::new("Drag ROMs or folders onto the window to import.")
            .small()
            .weak(),
    );
    ui.horizontal(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut chrome.import_path)
                .hint_text("…or paste a file or folder path")
                .desired_width(f32::INFINITY),
        );
        let submitted =
            response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if submitted && !chrome.import_path.trim().is_empty() {
            actions.push(UiAction::ImportTyped(chrome.import_path.trim().to_string()));
            chrome.import_path.clear();
        }
    });
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !chrome.import_path.trim().is_empty(),
                egui::Button::new("Import path"),
            )
            .clicked()
        {
            actions.push(UiAction::ImportTyped(chrome.import_path.trim().to_string()));
            chrome.import_path.clear();
        }
        if ui
            .button("Rescan")
            .on_hover_text(
                "Reconcile the index against the filesystem: pick up new files, notice moves \
                 and deletions. Does not rebuild the library.",
            )
            .clicked()
        {
            actions.push(UiAction::Rescan);
        }
    });
}

fn filters(chrome: &mut Chrome, ui: &mut egui::Ui, state: &ChromeState<'_>) {
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut chrome.search)
                .hint_text("Search")
                .desired_width(160.0),
        );
        if ui.button("×").on_hover_text("Clear the search").clicked() {
            chrome.search.clear();
        }
    });
    ui.horizontal_wrapped(|ui| {
        if ui
            .selectable_label(chrome.platform_filter.is_none(), "All")
            .clicked()
        {
            chrome.platform_filter = None;
        }
        for platform in Platform::ALL {
            // Counting from the already-filtered list would hide a platform the moment the search
            // excluded it, which makes the filter row jump around as you type.
            let count = state
                .roms
                .iter()
                .filter(|rom| rom.platform == *platform)
                .count();
            let selected = chrome.platform_filter == Some(*platform);
            if ui
                .selectable_label(selected, format!("{} ({count})", short_name(*platform)))
                .clicked()
            {
                chrome.platform_filter = if selected { None } else { Some(*platform) };
            }
        }
    });
}

fn rom_list(
    chrome: &mut Chrome,
    ui: &mut egui::Ui,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    let needle = chrome.search.trim().to_lowercase();
    let visible: Vec<_> = state
        .roms
        .iter()
        .filter(|rom| {
            chrome
                .platform_filter
                .is_none_or(|platform| rom.platform == platform)
                && (needle.is_empty()
                    || rom.title.to_lowercase().contains(&needle)
                    || rom.path.to_string_lossy().to_lowercase().contains(&needle))
        })
        .collect();

    if state.roms.is_empty() {
        ui.add_space(12.0);
        ui.label("Nothing imported yet.");
        ui.label(
            egui::RichText::new(
                "Drop a .gb, .gbc, or .gba file onto the window, or paste a folder path above.",
            )
            .small()
            .weak(),
        );
        return;
    }
    if visible.is_empty() {
        ui.add_space(12.0);
        ui.label("No ROM matches that filter.");
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for rom in visible {
                let selected = chrome.selected == Some(rom.id);
                let runnable = rom.platform.is_runnable();

                let response = ui
                    .push_id(rom.id, |ui| {
                        egui::Frame::group(ui.style())
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                row(chrome, ui, rom, actions);
                            })
                            .response
                    })
                    .inner
                    .interact(egui::Sense::click());

                if response.clicked() {
                    chrome.selected = Some(rom.id);
                }
                if response.double_clicked() && runnable && rom.present {
                    actions.push(UiAction::Play(rom.id));
                }
                if selected {
                    ui.painter().rect_stroke(
                        response.rect,
                        4.0,
                        egui::Stroke::new(1.0, ui.visuals().selection.bg_fill),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        });
}

fn row(
    chrome: &mut Chrome,
    ui: &mut egui::Ui,
    rom: &library::RomEntry,
    actions: &mut Vec<UiAction>,
) {
    let runnable = rom.platform.is_runnable();
    let playable = runnable && rom.present;

    ui.horizontal(|ui| {
        let title = egui::RichText::new(&rom.title).strong();
        ui.label(if rom.present { title } else { title.weak() });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(playable, egui::Button::new("▶ Play"))
                .on_disabled_hover_text(if !runnable {
                    "The Nintendo DS is not assembled yet (prompt 13)."
                } else {
                    "The file is not where the library remembers it."
                })
                .clicked()
            {
                actions.push(UiAction::Play(rom.id));
            }
        });
    });

    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(rom.platform.display_name())
                .small()
                .weak(),
        );
        ui.label(egui::RichText::new("·").small().weak());
        ui.label(
            egui::RichText::new(super::bytes(rom.size_bytes as usize))
                .small()
                .weak(),
        );
        ui.label(egui::RichText::new("·").small().weak());
        ui.label(
            egui::RichText::new(match rom.last_played_at {
                Some(at) => format!("played {}", timestamp(at)),
                None => "never played".to_string(),
            })
            .small()
            .weak(),
        );
        if !rom.present {
            ui.label(
                egui::RichText::new("missing")
                    .small()
                    .color(egui::Color32::from_rgb(0xFF, 0xB0, 0x74)),
            )
            .on_hover_text(format!(
                "{} was not found. Its play count and save states are kept; \
                 put the file back or rescan after moving it.",
                rom.path.display()
            ));
        }
    });

    ui.label(
        egui::RichText::new(rom.path.to_string_lossy())
            .small()
            .weak()
            .monospace(),
    );

    // Rename and forget are per-row and infrequent, so they live behind a collapsing header
    // rather than adding two buttons to every row.
    ui.collapsing("Manage", |ui| {
        let editing = matches!(&chrome.rename_buffer, Some((id, _)) if *id == rom.id);
        if editing {
            let mut buffer = chrome
                .rename_buffer
                .as_ref()
                .map(|(_, text)| text.clone())
                .unwrap_or_default();
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut buffer).desired_width(200.0));
                if ui.button("Save").clicked() && !buffer.trim().is_empty() {
                    actions.push(UiAction::Rename {
                        rom: rom.id,
                        title: buffer.trim().to_string(),
                    });
                    chrome.rename_buffer = None;
                    return;
                }
                if ui.button("Cancel").clicked() {
                    chrome.rename_buffer = None;
                    return;
                }
                chrome.rename_buffer = Some((rom.id, buffer));
            });
        } else if ui.button("Rename…").clicked() {
            chrome.rename_buffer = Some((rom.id, rom.title.clone()));
        }

        if chrome.confirm_forget == Some(rom.id) {
            ui.label(
                egui::RichText::new(
                    "Remove from the library? The ROM file itself is never deleted.",
                )
                .small(),
            );
            ui.horizontal(|ui| {
                if ui.button("Remove, keep states").clicked() {
                    actions.push(UiAction::Forget {
                        rom: rom.id,
                        delete_states: false,
                    });
                    chrome.confirm_forget = None;
                }
                if ui.button("Remove and delete states").clicked() {
                    actions.push(UiAction::Forget {
                        rom: rom.id,
                        delete_states: true,
                    });
                    chrome.confirm_forget = None;
                }
                if ui.button("Cancel").clicked() {
                    chrome.confirm_forget = None;
                }
            });
        } else if ui.button("Remove from library…").clicked() {
            chrome.confirm_forget = Some(rom.id);
        }
    });
}

/// A short name for the filter chips, where the full one does not fit.
fn short_name(platform: Platform) -> &'static str {
    match platform {
        Platform::Gb => "GB",
        Platform::Gbc => "GBC",
        Platform::Gba => "GBA",
        Platform::Nds => "DS",
    }
}
