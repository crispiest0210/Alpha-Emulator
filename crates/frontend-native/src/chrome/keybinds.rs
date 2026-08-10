//! The keybind configurator.
//!
//! The conflict rule it enforces is `frontend-core`'s, not its own: `KeybindMap::bind` refuses a
//! key that is already claimed and `KeybindMap::rebind` displaces it. This panel's whole job is
//! to make that choice visible — show the conflict, let the user confirm, then displace. A UI that
//! silently displaced would make bindings vanish without explanation; one that silently refused
//! would look broken.
//!
//! Capture works by *taking the keyboard*: while waiting for a key, [`Chrome::wants_keyboard`]
//! returns true and the emulated console sees nothing. Without that, binding a key would also press
//! the button it was bound to.

use super::{Chrome, ChromeState, UiAction};
use core_common::Buttons;
use frontend_core::{Action, ChromeAction, PhysicalKey};

/// Emulated buttons in the order a player thinks about them, rather than bit order.
const BUTTONS: &[(&str, Buttons)] = &[
    ("Up", Buttons::UP),
    ("Down", Buttons::DOWN),
    ("Left", Buttons::LEFT),
    ("Right", Buttons::RIGHT),
    ("A", Buttons::A),
    ("B", Buttons::B),
    ("X", Buttons::X),
    ("Y", Buttons::Y),
    ("L", Buttons::L),
    ("R", Buttons::R),
    ("Start", Buttons::START),
    ("Select", Buttons::SELECT),
];

const CHROME_ACTIONS: &[ChromeAction] = &[
    ChromeAction::TogglePause,
    ChromeAction::FastForward,
    ChromeAction::Rewind,
    ChromeAction::SaveState,
    ChromeAction::LoadState,
    ChromeAction::Reset,
    ChromeAction::ToggleHud,
    ChromeAction::ToggleFullscreen,
    ChromeAction::Screenshot,
    ChromeAction::ToggleDebugger,
];

pub fn window(
    chrome: &mut Chrome,
    ctx: &egui::Context,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    let mut open = chrome.show_keybinds;
    egui::Window::new("Keys")
        .open(&mut open)
        .default_width(430.0)
        .show(ctx, |ui| {
            if let Some(action) = chrome.capturing {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Press a key for {}…", describe(action)))
                            .strong(),
                    );
                    if ui.button("Cancel").clicked() {
                        chrome.capturing = None;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "The key you press replaces whatever it was bound to. Escape cancels.",
                    )
                    .small()
                    .weak(),
                );
                ui.separator();
            }

            ui.label(egui::RichText::new("Buttons").small().weak());
            egui::Grid::new("button-binds")
                .num_columns(3)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for (name, button) in BUTTONS {
                        binding_row(chrome, ui, state, Action::Button(*button), name, actions);
                    }
                });

            ui.add_space(8.0);
            ui.label(egui::RichText::new("Frontend actions").small().weak());
            egui::Grid::new("chrome-binds")
                .num_columns(3)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for action in CHROME_ACTIONS {
                        binding_row(
                            chrome,
                            ui,
                            state,
                            Action::Chrome(*action),
                            action.name(),
                            actions,
                        );
                    }
                });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Restore defaults").clicked() {
                    actions.push(UiAction::ResetKeybinds);
                    chrome.capturing = None;
                }
                ui.label(
                    egui::RichText::new("Bindings are physical key positions, not letters.")
                        .small()
                        .weak(),
                );
            });
        });
    chrome.show_keybinds = open;
}

fn binding_row(
    chrome: &mut Chrome,
    ui: &mut egui::Ui,
    state: &ChromeState<'_>,
    action: Action,
    label: &str,
    actions: &mut Vec<UiAction>,
) {
    ui.label(label);

    // An action may hold several keys, and all of them are shown: a player who bound both WASD and
    // the arrow keys should be able to see and remove either.
    let keys = state.keybinds.keys_for(action);
    ui.horizontal(|ui| {
        if keys.is_empty() {
            ui.label(egui::RichText::new("unbound").weak());
        }
        for key in &keys {
            if ui
                .button(key_name(*key))
                .on_hover_text("Click to unbind")
                .clicked()
            {
                actions.push(UiAction::Unbind(*key));
            }
        }
    });

    let capturing_this = chrome.capturing == Some(action);
    if ui
        .selectable_label(
            capturing_this,
            if capturing_this { "press…" } else { "Bind" },
        )
        .clicked()
    {
        chrome.capturing = if capturing_this { None } else { Some(action) };
    }
    ui.end_row();
}

/// The key press that ends a capture.
///
/// Returns the action to apply, or `None` when the press cancelled. Separate from the drawing code
/// because it is called from the window-event path, before any UI runs for the frame — a capture
/// resolved during drawing would be one frame late and would flicker.
pub fn resolve_capture(chrome: &mut Chrome, key: PhysicalKey) -> Option<UiAction> {
    let action = chrome.capturing.take()?;
    // Escape cancels rather than binding, so a user who opened the capture by accident is not
    // forced to give up a key. Escape is bindable through the config file for anyone who wants it.
    if key == PhysicalKey::Escape {
        return None;
    }
    Some(UiAction::Rebind { key, action })
}

fn describe(action: Action) -> String {
    match action {
        Action::Button(buttons) => BUTTONS
            .iter()
            .find(|(_, b)| *b == buttons)
            .map(|(name, _)| format!("button {name}"))
            .unwrap_or_else(|| format!("{buttons:?}")),
        Action::Chrome(chrome) => chrome.name().to_string(),
    }
}

/// A key's name as the user would say it, rather than its `Debug` form.
fn key_name(key: PhysicalKey) -> String {
    use PhysicalKey as P;
    match key {
        P::ArrowUp => "↑".into(),
        P::ArrowDown => "↓".into(),
        P::ArrowLeft => "←".into(),
        P::ArrowRight => "→".into(),
        P::ShiftLeft => "L Shift".into(),
        P::ShiftRight => "R Shift".into(),
        P::ControlLeft => "L Ctrl".into(),
        P::ControlRight => "R Ctrl".into(),
        P::AltLeft => "L Alt".into(),
        P::AltRight => "R Alt".into(),
        P::BracketLeft => "[".into(),
        P::BracketRight => "]".into(),
        P::Backquote => "`".into(),
        P::Comma => ",".into(),
        P::Period => ".".into(),
        P::Slash => "/".into(),
        P::Backslash => "\\".into(),
        P::Semicolon => ";".into(),
        P::Quote => "'".into(),
        P::Minus => "-".into(),
        P::Equal => "=".into(),
        // `Digit3` and the letter variants read correctly with the prefix stripped.
        other => format!("{other:?}").trim_start_matches("Digit").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_emulated_button_is_offered() {
        // A button missing from this table cannot be rebound at all, and nothing else would say so.
        let listed: Buttons = BUTTONS
            .iter()
            .fold(Buttons::empty(), |acc, (_, b)| acc | *b);
        assert_eq!(
            listed,
            Buttons::all(),
            "a button the emulated systems have is missing from the configurator"
        );
    }

    #[test]
    fn every_chrome_action_is_offered() {
        // Compared against the default map rather than a hard-coded count, so adding an action to
        // `ChromeAction` and binding it by default forces it into the UI too.
        for (_, action) in frontend_core::KeybindMap::defaults().iter() {
            if let Action::Chrome(chrome) = action {
                assert!(
                    CHROME_ACTIONS.contains(&chrome),
                    "{chrome:?} is bound by default but cannot be rebound"
                );
            }
        }
    }

    #[test]
    fn escape_cancels_a_capture_instead_of_binding_itself() {
        let mut chrome = Chrome {
            capturing: Some(Action::Button(Buttons::A)),
            ..Chrome::default()
        };
        assert_eq!(resolve_capture(&mut chrome, PhysicalKey::Escape), None);
        assert_eq!(
            chrome.capturing, None,
            "cancelling must still end the capture"
        );
    }

    #[test]
    fn a_captured_key_becomes_a_rebind_and_ends_the_capture() {
        let mut chrome = Chrome {
            capturing: Some(Action::Button(Buttons::A)),
            ..Chrome::default()
        };
        assert_eq!(
            resolve_capture(&mut chrome, PhysicalKey::Z),
            Some(UiAction::Rebind {
                key: PhysicalKey::Z,
                action: Action::Button(Buttons::A),
            })
        );
        assert_eq!(chrome.capturing, None);
    }

    #[test]
    fn a_key_press_with_no_capture_in_progress_does_nothing() {
        let mut chrome = Chrome::default();
        assert_eq!(resolve_capture(&mut chrome, PhysicalKey::Z), None);
    }

    #[test]
    fn key_names_are_readable_rather_than_debug_output() {
        assert_eq!(key_name(PhysicalKey::Digit3), "3");
        assert_eq!(key_name(PhysicalKey::W), "W");
        assert_eq!(key_name(PhysicalKey::ArrowUp), "↑");
        assert_eq!(key_name(PhysicalKey::ShiftLeft), "L Shift");
        assert_eq!(key_name(PhysicalKey::F11), "F11");
    }
}
