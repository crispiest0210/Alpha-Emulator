//! The `egui` chrome: library browser, HUD, save-state list, keybind editor, settings.
//!
//! # Why the panels return actions instead of doing things
//!
//! Predecessor lesson §2 was one ~2,200-line React component that owned UI state, canvas drawing,
//! audio glue, keyboard routing, and IPC at once. The structural fix is not "several files" — it
//! is that these panels have **no way** to reach the session, the library, or the window. They are
//! given the state to draw and they return [`UiAction`]s describing what the user asked for;
//! [`crate::app`] is the single place that interprets them.
//!
//! That is what keeps a panel honest. A settings checkbox cannot quietly grow a save-RAM flush, a
//! library row cannot start an emulation thread, and every side effect in the application is
//! reachable from one `match`.
//!
//! It also makes the panels' behaviour readable without following a call graph: the entire
//! vocabulary of things the UI can do is the [`UiAction`] enum, on one screen.

mod debugger_view;
mod hud;
mod keybinds;
mod library_view;
mod settings;
mod states;

pub use debugger_view::request as debug_request;
pub use keybinds::resolve_capture;

use frontend_core::{
    Action, ChromeAction, KeybindMap, LoadedRom, PhysicalKey, RewindConfig, ScalingMode,
    SessionCommand, SessionStats, SessionStatus,
};
use library::{RomEntry, RomId, SaveId, SaveStateEntry};
use std::path::PathBuf;

/// Everything the user can ask the application to do through the chrome.
#[derive(Debug, Clone, PartialEq)]
pub enum UiAction {
    /// Launch a library entry.
    Play(RomId),
    /// Index these files and folders.
    Import(Vec<PathBuf>),
    /// Index the path typed into the import box.
    ImportTyped(String),
    /// Re-run the filesystem reconciliation pass.
    Rescan,
    Forget {
        rom: RomId,
        delete_states: bool,
    },
    Rename {
        rom: RomId,
        title: String,
    },

    /// Pass straight through to the emulation thread.
    Session(SessionCommand),
    /// Write a state under a name the user typed.
    SaveNamed(String),
    DeleteState(SaveId),

    SetScaling(ScalingMode),
    SetVolume(f32),
    SetMuted(bool),
    SetFastForwardSpeed(f32),
    SetPauseOnFocusLoss(bool),
    SetDualScreenGap(u32),
    SetRewind(RewindConfig),

    Rebind {
        key: PhysicalKey,
        action: Action,
    },
    Unbind(PhysicalKey),
    ResetKeybinds,

    Screenshot,
    ToggleFullscreen,
    Quit,
}

/// What the chrome needs to know about the application to draw it.
///
/// A borrowed snapshot rather than a reference to the `App`: the panels then cannot reach anything
/// they were not handed, which is the whole point of the split above.
pub struct ChromeState<'a> {
    pub status: SessionStatus,
    pub stats: SessionStats,
    pub loaded: Option<&'a LoadedRom>,
    pub roms: &'a [RomEntry],
    pub states: &'a [SaveStateEntry],
    pub keybinds: &'a KeybindMap,
    pub config: &'a frontend_core::Config,
    pub audio_description: &'a str,
    pub gpu_description: &'a str,
    pub library_error: Option<&'a str>,
    /// The most recent debugger snapshot, if the panel has one yet.
    pub debug: Option<&'a frontend_core::DebugSnapshot>,
    /// Why introspection is not available, when it is not.
    pub debug_unavailable: Option<&'a str>,
    pub fullscreen: bool,
    /// Recent notices and errors, newest last.
    pub messages: &'a [Message],
}

/// A transient line for the user.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub text: String,
    pub is_error: bool,
    pub at: std::time::Instant,
}

/// Which panels are open, and the small amount of state that belongs to the UI alone.
///
/// Every field here is genuinely presentational — a search box's contents, which panel is showing.
/// Nothing that survives a restart lives here; that is `Config`'s job, and nothing about the
/// running machine lives here either, because that is the session's.
pub struct Chrome {
    pub show_library: bool,
    pub show_settings: bool,
    pub show_keybinds: bool,
    pub show_states: bool,
    pub show_hud: bool,

    /// Library browser state.
    pub search: String,
    pub platform_filter: Option<library::Platform>,
    pub selected: Option<RomId>,
    pub import_path: String,
    pub rename_buffer: Option<(RomId, String)>,
    pub confirm_forget: Option<RomId>,

    /// Save-state panel state.
    pub state_label: String,

    /// Debugger panel state. All presentational: where the two views are scrolled to, and the
    /// contents of three address boxes.
    pub show_debugger: bool,
    pub debugger_attached: bool,
    pub debugger_follow_pc: bool,
    pub debugger_disassembly_at: Option<u32>,
    pub debugger_memory_at: u32,
    pub debugger_goto: String,
    pub debugger_memory_goto: String,
    pub debugger_add_breakpoint: String,
    pub debugger_add_watch: String,

    /// The keybind editor is waiting for a key press for this action.
    pub capturing: Option<Action>,
}

impl Default for Chrome {
    fn default() -> Self {
        Self {
            // The library is what an emulator with no cartridge should show. Anything else is a
            // blank window that gives the user nothing to click.
            show_library: true,
            show_settings: false,
            show_keybinds: false,
            show_states: false,
            show_hud: false,
            search: String::new(),
            platform_filter: None,
            selected: None,
            import_path: String::new(),
            rename_buffer: None,
            confirm_forget: None,
            state_label: String::new(),
            show_debugger: false,
            debugger_attached: false,
            // Following the program counter is what a debugger view should do until the user
            // deliberately scrolls away from it.
            debugger_follow_pc: true,
            debugger_disassembly_at: None,
            debugger_memory_at: 0,
            debugger_goto: String::new(),
            debugger_memory_goto: String::new(),
            debugger_add_breakpoint: String::new(),
            debugger_add_watch: String::new(),
            capturing: None,
        }
    }
}

impl Chrome {
    /// Whether a key press should go to the chrome rather than to the emulated machine.
    ///
    /// Prompt 10's precedence rule in one place. While the keybind editor is waiting for a key, or
    /// while a text field has focus, the emulated console must not also see the keystroke —
    /// otherwise typing a save-state label walks the player's character across the room.
    pub fn wants_keyboard(&self, egui_wants_keyboard: bool) -> bool {
        self.capturing.is_some() || egui_wants_keyboard
    }

    /// React to a chrome action bound to a key.
    ///
    /// The ones that only affect what is on screen are handled here; the rest become
    /// [`UiAction`]s for the application, because they touch the session, the window, or the disk.
    pub fn handle_chrome_action(&mut self, action: ChromeAction) -> Option<UiAction> {
        match action {
            ChromeAction::ToggleHud => {
                self.show_hud = !self.show_hud;
                None
            }
            ChromeAction::ToggleDebugger => {
                self.show_debugger = !self.show_debugger;
                // Opening it pauses, because the first thing anyone does on opening a debugger is
                // stop the machine to look at it. Closing does not resume: the user may have paused
                // deliberately, and un-pausing them would be presumptuous.
                self.show_debugger
                    .then_some(UiAction::Session(SessionCommand::SetPaused(true)))
            }
            ChromeAction::TogglePause => Some(UiAction::Session(SessionCommand::TogglePause)),
            ChromeAction::SaveState => Some(UiAction::Session(SessionCommand::SaveState {
                slot: None,
                label: None,
            })),
            ChromeAction::LoadState => Some(UiAction::Session(SessionCommand::LoadSlot(0))),
            ChromeAction::Reset => Some(UiAction::Session(SessionCommand::Reset)),
            ChromeAction::Screenshot => Some(UiAction::Screenshot),
            ChromeAction::ToggleFullscreen => Some(UiAction::ToggleFullscreen),
            // Held, not pressed: delivered per frame from the held-key set, never from here.
            ChromeAction::FastForward | ChromeAction::Rewind => None,
        }
    }

    /// Draw everything, collecting what the user asked for.
    pub fn ui(&mut self, ui: &mut egui::Ui, state: &ChromeState<'_>) -> Vec<UiAction> {
        let mut actions = Vec::new();
        let ctx = ui.ctx().clone();
        // Panels are laid out against the root `Ui` and take space from it, in declaration order;
        // free-floating windows and overlays are attached to the context instead.
        self.top_bar(ui, state, &mut actions);
        if self.show_library {
            library_view::panel(self, ui, state, &mut actions);
        }
        if self.show_states {
            states::window(self, &ctx, state, &mut actions);
        }
        if self.show_debugger {
            debugger_view::window(self, &ctx, state, &mut actions);
        } else if self.debugger_attached {
            // Closing the window by its own X leaves the emulation thread stepping one instruction
            // at a time, which would be a permanent invisible slowdown. Detaching here covers every
            // way the panel can close.
            self.debugger_attached = false;
            actions.push(UiAction::Session(SessionCommand::SetDebugAttached(false)));
        }
        if self.show_keybinds {
            keybinds::window(self, &ctx, state, &mut actions);
        }
        if self.show_settings {
            settings::window(self, &ctx, state, &mut actions);
        }
        if self.show_hud && state.loaded.is_some() {
            hud::overlay(&ctx, state);
        }
        messages(&ctx, state);
        actions
    }

    fn top_bar(
        &mut self,
        root: &mut egui::Ui,
        state: &ChromeState<'_>,
        actions: &mut Vec<UiAction>,
    ) {
        egui::Panel::top("top-bar").show(root, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.selectable_label(self.show_library, "Library").clicked() {
                    self.show_library = !self.show_library;
                }

                ui.separator();

                let running = state.loaded.is_some();
                ui.add_enabled_ui(running, |ui| {
                    let paused = state.status == SessionStatus::Paused;
                    if ui.button(if paused { "Resume" } else { "Pause" }).clicked() {
                        actions.push(UiAction::Session(SessionCommand::TogglePause));
                    }
                    if ui.button("Reset").clicked() {
                        actions.push(UiAction::Session(SessionCommand::Reset));
                    }
                    if ui.button("Save state").clicked() {
                        actions.push(UiAction::Session(SessionCommand::SaveState {
                            slot: None,
                            label: None,
                        }));
                    }
                    if ui.button("Load state").clicked() {
                        actions.push(UiAction::Session(SessionCommand::LoadSlot(0)));
                    }
                    if ui.selectable_label(self.show_states, "States").clicked() {
                        self.show_states = !self.show_states;
                    }
                    if ui.button("Eject").clicked() {
                        actions.push(UiAction::Session(SessionCommand::CloseRom));
                    }
                });

                ui.separator();
                if ui.selectable_label(self.show_hud, "HUD").clicked() {
                    self.show_hud = !self.show_hud;
                }
                if ui.selectable_label(self.show_keybinds, "Keys").clicked() {
                    self.show_keybinds = !self.show_keybinds;
                }
                if ui
                    .selectable_label(self.show_debugger, "Debugger")
                    .on_hover_text(
                        "Registers, disassembly, memory, and execution breakpoints. \
                         Attaching steps one instruction at a time, which costs speed.",
                    )
                    .clicked()
                {
                    if let Some(action) = self.handle_chrome_action(ChromeAction::ToggleDebugger) {
                        actions.push(action);
                    }
                }
                if ui
                    .selectable_label(self.show_settings, "Settings")
                    .clicked()
                {
                    self.show_settings = !self.show_settings;
                }
                if ui
                    .button("Quit")
                    .on_hover_text("Writes settings and flushes save RAM before exiting.")
                    .clicked()
                {
                    actions.push(UiAction::Quit);
                }

                // Right-aligned status, so the eye has one fixed place to check what is running.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = match state.loaded {
                        Some(rom) => format!(
                            "{} — {} — {}",
                            rom.title,
                            rom.platform.display_name(),
                            state.status.label()
                        ),
                        None => "no cartridge".to_string(),
                    };
                    ui.label(label);
                    if state.loaded.is_some() && state.stats.fps > 0.0 {
                        ui.label(format!("{:.0}%", state.stats.speed_percent));
                    }
                });
            });
        });
    }
}

/// Recent notices, bottom-left, oldest first.
///
/// Deliberately not modal. A save state written or a rewind exhausted is worth saying and never
/// worth interrupting play for, and an emulator that opens a dialog box mid-boss-fight is worse
/// than one that says nothing.
fn messages(ctx: &egui::Context, state: &ChromeState<'_>) {
    if state.messages.is_empty() {
        return;
    }
    egui::Area::new("messages".into())
        .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(12.0, -12.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(560.0);
                for message in state.messages {
                    let text = egui::RichText::new(&message.text).monospace();
                    ui.label(if message.is_error {
                        text.color(egui::Color32::from_rgb(0xFF, 0x8A, 0x80))
                    } else {
                        text
                    });
                }
            });
        });
}

/// Human-readable byte count, for the rewind and save-state figures.
pub(crate) fn bytes(count: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A Unix timestamp as something a person can read.
///
/// Hand-rolled civil-date arithmetic rather than a `chrono`/`time` dependency for one label. The
/// algorithm is the standard days-from-epoch inverse; it is exact for every date this will ever
/// see, and the alternative was a dependency whose only use is this function.
pub(crate) fn timestamp(unix_seconds: i64) -> String {
    if unix_seconds <= 0 {
        return "never".to_string();
    }
    let days = unix_seconds.div_euclid(86_400);
    let seconds_of_day = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60
    )
}

/// Days since 1970-01-01 to a calendar date. Howard Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_actions_are_not_handled_as_presses() {
        let mut chrome = Chrome::default();
        assert_eq!(chrome.handle_chrome_action(ChromeAction::FastForward), None);
        assert_eq!(chrome.handle_chrome_action(ChromeAction::Rewind), None);
    }

    #[test]
    fn toggling_the_hud_is_handled_in_the_chrome_and_needs_no_action() {
        let mut chrome = Chrome::default();
        let before = chrome.show_hud;
        assert_eq!(chrome.handle_chrome_action(ChromeAction::ToggleHud), None);
        assert_eq!(chrome.show_hud, !before);
    }

    #[test]
    fn discrete_actions_that_touch_the_session_become_ui_actions() {
        let mut chrome = Chrome::default();
        assert_eq!(
            chrome.handle_chrome_action(ChromeAction::TogglePause),
            Some(UiAction::Session(SessionCommand::TogglePause))
        );
        assert_eq!(
            chrome.handle_chrome_action(ChromeAction::SaveState),
            Some(UiAction::Session(SessionCommand::SaveState {
                slot: None,
                label: None
            }))
        );
        assert_eq!(
            chrome.handle_chrome_action(ChromeAction::Screenshot),
            Some(UiAction::Screenshot)
        );
    }

    #[test]
    fn a_capturing_keybind_editor_takes_the_keyboard_from_the_game() {
        let chrome = Chrome::default();
        assert!(!chrome.wants_keyboard(false));
        let chrome = Chrome {
            capturing: Some(Action::Chrome(ChromeAction::Reset)),
            ..Chrome::default()
        };
        assert!(
            chrome.wants_keyboard(false),
            "a key pressed while rebinding must not also reach the emulated console"
        );
    }

    #[test]
    fn a_focused_text_field_takes_the_keyboard_from_the_game() {
        let chrome = Chrome::default();
        assert!(
            chrome.wants_keyboard(true),
            "typing a save-state label must not walk the player's character across the room"
        );
    }

    #[test]
    fn byte_counts_read_the_way_a_person_expects() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1_572_864), "1.5 MiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn timestamps_are_formatted_from_known_epoch_values() {
        assert_eq!(timestamp(0), "never", "an unset timestamp is not 1970");
        assert_eq!(timestamp(86_400), "1970-01-02 00:00");
        // A leap day, which is where naive date arithmetic goes wrong.
        assert_eq!(timestamp(1_709_209_240), "2024-02-29 12:20");
        // 2000-03-01, the day after the leap day of a century that *is* a leap year.
        assert_eq!(timestamp(951_868_800), "2000-03-01 00:00");
    }

    #[test]
    fn the_library_is_open_by_default_so_a_fresh_launch_is_not_a_blank_window() {
        let chrome = Chrome::default();
        assert!(chrome.show_library);
        assert!(!chrome.show_settings);
        assert!(!chrome.show_keybinds);
    }
}
