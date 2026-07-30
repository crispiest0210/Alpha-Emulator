//! The in-app debugger panel: registers, disassembly, memory, breakpoints.
//!
//! Prompt 15 names this the priority over the GDB server, and the reason is the daily case: a
//! contributor chasing a PPU or CPU accuracy bug wants to pause, look at where the machine is, and
//! step. That is what this shows.
//!
//! Like every other panel here it reaches nothing — it renders a
//! [`DebugSnapshot`](frontend_core::DebugSnapshot) the emulation thread captured and returns
//! [`UiAction`]s. It cannot read memory, cannot step the machine, and cannot set a breakpoint; it can
//! only say that the user asked for those things. See [`super`] for why that is structural rather
//! than stylistic.
//!
//! # `--` is data
//!
//! Bytes the machine refused to reveal are drawn as `--`, and that goes all the way back to
//! `DebugTarget::peek8`. A Game Boy's joypad register latches when read, so a memory view that
//! showed a number there would have changed the machine to get it. `--` in a hex viewer is the
//! honest rendering of "reading this would have consequences".

use super::{Chrome, ChromeState, UiAction};
use frontend_core::{AccessKind, DebugRequest, SessionCommand, SessionStatus, Watchpoint};

/// Monospace, because a hex viewer whose columns do not line up is not a hex viewer.
fn mono(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).monospace()
}

pub fn window(
    chrome: &mut Chrome,
    ctx: &egui::Context,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    let mut open = chrome.show_debugger;
    egui::Window::new("Debugger")
        .open(&mut open)
        .default_width(760.0)
        .default_height(560.0)
        .show(ctx, |ui| {
            if state.loaded.is_none() {
                ui.label("No cartridge is loaded.");
                return;
            }
            if let Some(reason) = state.debug_unavailable {
                ui.label(
                    egui::RichText::new(reason).color(egui::Color32::from_rgb(0xFF, 0xB0, 0x74)),
                );
                ui.label(
                    egui::RichText::new(
                        "A system offers introspection once it implements `DebugTarget`. \
                         The Nintendo DS has not been assembled yet.",
                    )
                    .small()
                    .weak(),
                );
                return;
            }
            let Some(snapshot) = state.debug else {
                ui.spinner();
                ui.label(
                    egui::RichText::new("waiting for the first snapshot…")
                        .small()
                        .weak(),
                );
                return;
            };

            controls(chrome, ui, state, actions);
            ui.separator();
            registers(ui, snapshot);
            ui.separator();

            // Side by side: the disassembly is what you read and the memory is what you check
            // against it, and putting one above the other makes both too short to be useful.
            ui.columns(2, |columns| {
                disassembly(chrome, &mut columns[0], state, actions);
                memory(chrome, &mut columns[1], state, actions);
            });
            ui.separator();
            breakpoints(chrome, ui, state, actions);
        });
    chrome.show_debugger = open;

    // Attachment follows the panel. Opening it is the explicit action that switches the emulation
    // thread to instruction stepping, and closing it must give the speed back — a debugger that
    // stayed attached after being closed would be a permanent, invisible slowdown.
    if chrome.show_debugger != chrome.debugger_attached {
        chrome.debugger_attached = chrome.show_debugger;
        actions.push(UiAction::Session(SessionCommand::SetDebugAttached(
            chrome.debugger_attached,
        )));
    }
}

fn controls(
    chrome: &mut Chrome,
    ui: &mut egui::Ui,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    let paused = state.status == SessionStatus::Paused;
    ui.horizontal_wrapped(|ui| {
        if ui
            .button(if paused { "▶ Continue" } else { "⏸ Break" })
            .clicked()
        {
            actions.push(UiAction::Session(SessionCommand::SetPaused(!paused)));
        }
        // Stepping is only meaningful from a stop. Enabling it while running would produce a step
        // from wherever the machine happened to be a moment later, which is not what was asked for.
        ui.add_enabled_ui(paused, |ui| {
            if ui.button("Step").on_hover_text("One instruction").clicked() {
                actions.push(UiAction::Session(SessionCommand::StepInstructions(1)));
            }
            for count in [16u32, 256] {
                if ui.button(format!("+{count}")).clicked() {
                    actions.push(UiAction::Session(SessionCommand::StepInstructions(count)));
                }
            }
            if ui
                .button("Frame")
                .on_hover_text("One video frame")
                .clicked()
            {
                actions.push(UiAction::Session(SessionCommand::StepFrames(1)));
            }
        });

        ui.separator();
        let following = chrome.debugger_follow_pc;
        if ui
            .selectable_label(following, "Follow PC")
            .on_hover_text("Keep the disassembly centred on the program counter")
            .clicked()
        {
            chrome.debugger_follow_pc = !following;
            if chrome.debugger_follow_pc {
                chrome.debugger_disassembly_at = None;
            }
        }

        if let Some(snapshot) = state.debug {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if snapshot.halted {
                    ui.label(
                        egui::RichText::new("halted")
                            .color(egui::Color32::from_rgb(0xFF, 0xD5, 0x4F)),
                    )
                    .on_hover_text("Waiting for an interrupt, which is not the same as stopped.");
                }
                ui.label(mono(format!(
                    "PC {}  {}",
                    snapshot.format_address(snapshot.program_counter),
                    snapshot.flags
                )));
            });
        }
    });
}

fn registers(ui: &mut egui::Ui, snapshot: &frontend_core::DebugSnapshot) {
    // Wrapped rather than a fixed grid: the SM83 has a dozen registers and the ARM7TDMI
    // twenty-eight, and a layout tuned for one is wrong for the other.
    ui.horizontal_wrapped(|ui| {
        for register in &snapshot.registers {
            ui.label(mono(register.to_string()))
                .on_hover_text(format!("{} bits", register.width_bits));
        }
    });
}

fn disassembly(
    chrome: &mut Chrome,
    ui: &mut egui::Ui,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    let Some(snapshot) = state.debug else { return };
    ui.label(egui::RichText::new("Disassembly").small().weak());
    ui.label(
        egui::RichText::new("Click an address to toggle a breakpoint.")
            .small()
            .weak(),
    );

    egui::ScrollArea::vertical()
        .id_salt("disassembly")
        .max_height(280.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for line in &snapshot.disassembly {
                ui.horizontal(|ui| {
                    // The breakpoint dot is the click target, not a separate checkbox column: the
                    // address is what a reader is already looking at.
                    let marker = if line.has_breakpoint { "●" } else { "  " };
                    let address = mono(format!("{marker} {}", snapshot.format_address(line.addr)));
                    let address = if line.has_breakpoint {
                        address.color(egui::Color32::from_rgb(0xFF, 0x6B, 0x6B))
                    } else {
                        address
                    };
                    if ui
                        .add(egui::Label::new(address).sense(egui::Sense::click()))
                        .clicked()
                    {
                        actions.push(UiAction::Session(if line.has_breakpoint {
                            SessionCommand::RemoveBreakpoint(line.addr)
                        } else {
                            SessionCommand::AddBreakpoint(line.addr)
                        }));
                    }

                    // The encoding, then the mnemonic. Reading the bytes is how you check the
                    // disassembler itself, which is exactly what a contributor debugging a CPU is
                    // doing.
                    let bytes: String = line
                        .bytes
                        .iter()
                        .map(|byte| format!("{byte:02X}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    ui.label(mono(format!("{bytes:<11}")).weak());

                    let text = mono(&line.text);
                    ui.label(if line.is_program_counter {
                        text.strong()
                            .background_color(ui.visuals().selection.bg_fill)
                    } else {
                        text
                    });
                });
            }
        });

    // The scroll position is a jump address, not a pixel offset: with variable-length instructions
    // there is no way to scroll "up" a line without re-deriving where the previous instruction
    // started, so the view is re-captured from an explicit address instead.
    ui.horizontal(|ui| {
        if ui.small_button("◀ back").clicked() {
            let base = chrome
                .debugger_disassembly_at
                .unwrap_or(snapshot.program_counter);
            chrome.debugger_follow_pc = false;
            // A fixed step backwards. On a variable-length instruction set this can land
            // mid-instruction and decode differently than the same region read forwards — which is
            // inherent, not a bug, and is why the encoding is shown next to every line.
            chrome.debugger_disassembly_at = Some(base.wrapping_sub(16));
        }
        if ui.small_button("forward ▶").clicked() {
            let base = chrome
                .debugger_disassembly_at
                .unwrap_or(snapshot.program_counter);
            chrome.debugger_follow_pc = false;
            chrome.debugger_disassembly_at = Some(base.wrapping_add(16));
        }
        if ui.small_button("to PC").clicked() {
            chrome.debugger_follow_pc = true;
            chrome.debugger_disassembly_at = None;
        }
        address_box(ui, "disasm-goto", &mut chrome.debugger_goto, |addr| {
            chrome.debugger_follow_pc = false;
            chrome.debugger_disassembly_at = Some(addr);
        });
        if ui.small_button("set PC here").clicked() {
            if let Some(addr) = chrome.debugger_disassembly_at {
                actions.push(UiAction::Session(SessionCommand::SetProgramCounter(addr)));
            }
        }
    });
}

fn memory(
    chrome: &mut Chrome,
    ui: &mut egui::Ui,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    let _ = actions;
    let Some(snapshot) = state.debug else { return };
    ui.label(egui::RichText::new("Memory").small().weak());

    ui.horizontal_wrapped(|ui| {
        address_box(
            ui,
            "memory-goto",
            &mut chrome.debugger_memory_goto,
            |addr| {
                chrome.debugger_memory_at = addr;
            },
        );
    });
    ui.horizontal_wrapped(|ui| {
        for region in snapshot.regions {
            if ui
                .small_button(region.name)
                .on_hover_text(format!(
                    "{}–{}",
                    snapshot.format_address(region.start),
                    snapshot.format_address(region.end)
                ))
                .clicked()
            {
                chrome.debugger_memory_at = region.start;
            }
        }
    });

    egui::ScrollArea::vertical()
        .id_salt("memory")
        .max_height(280.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for row in &snapshot.memory {
                let hex: String = row
                    .bytes
                    .iter()
                    .map(|byte| match byte {
                        Some(byte) => format!("{byte:02X}"),
                        // Not `00`. A byte the machine refused to reveal and a byte that happens to
                        // be zero are different facts, and only one of them is a fact.
                        None => "--".to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                ui.label(mono(format!(
                    "{}  {hex}  {}",
                    snapshot.format_address(row.addr),
                    row.ascii()
                )));
            }
        });
}

fn breakpoints(
    chrome: &mut Chrome,
    ui: &mut egui::Ui,
    state: &ChromeState<'_>,
    actions: &mut Vec<UiAction>,
) {
    let Some(snapshot) = state.debug else { return };
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("Breakpoints").small().weak());
        if snapshot.execution_breakpoints.is_empty() {
            ui.label(
                egui::RichText::new("none — click an address above")
                    .small()
                    .weak(),
            );
        }
        for addr in &snapshot.execution_breakpoints {
            if ui
                .small_button(mono(snapshot.format_address(*addr)))
                .on_hover_text("Click to remove")
                .clicked()
            {
                actions.push(UiAction::Session(SessionCommand::RemoveBreakpoint(*addr)));
            }
        }
        if !snapshot.execution_breakpoints.is_empty() && ui.small_button("Clear all").clicked() {
            actions.push(UiAction::Session(SessionCommand::ClearBreakpoints));
        }
        address_box(
            ui,
            "breakpoint-add",
            &mut chrome.debugger_add_breakpoint,
            |addr| {
                actions.push(UiAction::Session(SessionCommand::AddBreakpoint(addr)));
            },
        );
    });
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("Watchpoints").small().weak());
        // Read and write are separate buttons rather than a mode toggle beside one box: which of the
        // two you want is the first thing you know, and a toggle you forget to set gives you a
        // watchpoint that silently never fires.
        address_box(ui, "watch-write", &mut chrome.debugger_add_watch, |addr| {
            actions.push(UiAction::Session(SessionCommand::AddWatchpoint(
                Watchpoint::at(addr, AccessKind::Write),
            )));
        });
        if ui
            .small_button("on write")
            .on_hover_text("Break when the address above is written")
            .clicked()
        {
            if let Some(addr) = parse_address(&chrome.debugger_add_watch) {
                actions.push(UiAction::Session(SessionCommand::AddWatchpoint(
                    Watchpoint::at(addr, AccessKind::Write),
                )));
                chrome.debugger_add_watch.clear();
            }
        }
        if ui
            .small_button("on read")
            .on_hover_text("Break when the address above is read")
            .clicked()
        {
            if let Some(addr) = parse_address(&chrome.debugger_add_watch) {
                actions.push(UiAction::Session(SessionCommand::AddWatchpoint(
                    Watchpoint::at(addr, AccessKind::Read),
                )));
                chrome.debugger_add_watch.clear();
            }
        }
        for watch in &snapshot.watchpoints {
            let kind = if watch.kind == AccessKind::Write {
                "w"
            } else {
                "r"
            };
            let label = if watch.end.saturating_sub(watch.start) <= 1 {
                format!("{kind} {}", snapshot.format_address(watch.start))
            } else {
                format!(
                    "{kind} {}..{}",
                    snapshot.format_address(watch.start),
                    snapshot.format_address(watch.end)
                )
            };
            if ui
                .small_button(mono(label))
                .on_hover_text("Click to remove")
                .clicked()
            {
                actions.push(UiAction::Session(SessionCommand::RemoveWatchpointsAt(
                    watch.start,
                )));
            }
        }
    });
    ui.label(
        egui::RichText::new(
            "A watchpoint sees the CPU's accesses. The PPU reads VRAM directly and DMA does not go \
             through the bus, so a watchpoint on VRAM catches the program writing it, not the \
             hardware reading it.",
        )
        .small()
        .weak(),
    );
}

/// A hex-address entry box that fires on Enter.
///
/// Parsing is deliberately lenient about `0x` and `$` prefixes and about case: a contributor pastes
/// addresses from a disassembly, a hardware document, and a source comment in one session, and each
/// writes them differently.
fn address_box(ui: &mut egui::Ui, id: &str, buffer: &mut String, mut on_submit: impl FnMut(u32)) {
    let response = ui.add(
        egui::TextEdit::singleline(buffer)
            .id_salt(id)
            .hint_text("hex address")
            .desired_width(96.0)
            .font(egui::TextStyle::Monospace),
    );
    let submitted = response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
    if submitted {
        // A value that does not parse is left in the box rather than cleared, so a typo can be
        // corrected instead of retyped.
        if let Some(addr) = parse_address(buffer) {
            on_submit(addr);
            buffer.clear();
        }
    }
}

/// Parse a hex address written any of the ways people write them.
fn parse_address(text: &str) -> Option<u32> {
    let cleaned = text
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .trim_start_matches('$')
        .replace('_', "");
    if cleaned.is_empty() {
        return None;
    }
    u32::from_str_radix(&cleaned, 16).ok()
}

/// The request the panel wants served, derived from its own scroll state.
///
/// Built here rather than in `app.rs` so the panel's view state and the request that fills it cannot
/// drift apart — the panel decides what it is showing, and this says what that needs.
pub fn request(chrome: &Chrome) -> DebugRequest {
    DebugRequest {
        disassembly_at: if chrome.debugger_follow_pc {
            None
        } else {
            chrome.debugger_disassembly_at
        },
        disassembly_lines: 32,
        memory_at: chrome.debugger_memory_at,
        memory_rows: 24,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_however_they_are_written() {
        assert_eq!(parse_address("C000"), Some(0xC000));
        assert_eq!(parse_address("c000"), Some(0xC000));
        assert_eq!(parse_address("0xC000"), Some(0xC000));
        assert_eq!(parse_address("$C000"), Some(0xC000));
        assert_eq!(parse_address("  0x0800_0000 "), Some(0x0800_0000));
    }

    #[test]
    fn a_non_address_is_refused_rather_than_guessed() {
        assert_eq!(parse_address(""), None);
        assert_eq!(parse_address("   "), None);
        assert_eq!(parse_address("nonsense"), None);
        assert_eq!(parse_address("0x"), None);
        // Wider than 32 bits: no machine here has such an address, and truncating would jump
        // somewhere the user did not name.
        assert_eq!(parse_address("1_0000_0000"), None);
    }

    #[test]
    fn following_the_pc_asks_for_no_explicit_start() {
        let chrome = Chrome {
            debugger_follow_pc: true,
            debugger_disassembly_at: Some(0x1234),
            ..Chrome::default()
        };
        assert_eq!(
            request(&chrome).disassembly_at,
            None,
            "following the PC must override a stale scroll position"
        );
    }

    #[test]
    fn a_scrolled_view_asks_for_its_own_start() {
        let chrome = Chrome {
            debugger_follow_pc: false,
            debugger_disassembly_at: Some(0x1234),
            debugger_memory_at: 0xC000,
            ..Chrome::default()
        };
        let request = request(&chrome);
        assert_eq!(request.disassembly_at, Some(0x1234));
        assert_eq!(request.memory_at, 0xC000);
    }

    #[test]
    fn the_request_stays_within_what_the_session_will_serve() {
        // `debugger::Request::clamped` caps these; asking for more would silently get less, which
        // would make the panel's own scroll arithmetic wrong.
        let request = request(&Chrome::default()).clamped();
        assert_eq!(request.disassembly_lines, 32);
        assert_eq!(request.memory_rows, 24);
    }
}
