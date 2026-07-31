//! The application: a thin composition of the window, the renderer, the chrome, and the session.
//!
//! This file is deliberately the only place that knows about all of them, and it is deliberately
//! kept to plumbing. Nothing here draws a widget, computes a rectangle, decodes a key, or steps a
//! machine — those live in [`crate::chrome`], [`crate::layout`], [`crate::keymap`], and
//! `frontend-core` respectively. What is left is: route an event, apply an action, draw a frame.
//!
//! Predecessor lesson §2 was a single ~2,200-line component doing all of that at once. The guard
//! against repeating it is not this file's length but the shape of its dependencies: the chrome
//! cannot reach the session, the layout cannot reach the GPU, and `frontend-core` cannot reach any
//! of it. Every one of those is enforced by the type system or by `cargo deny`, not by discipline.

use anyhow::Result;
use frontend_core::{
    catalog, screen_size, Action, ChromeAction, Config, DebugSnapshot, Frame, InputTracker,
    KeybindMap, LoadedRom, Session, SessionCommand, SessionEvent, SessionOptions, SessionStats,
    SessionStatus,
};
use library::{AppPaths, Library, RomEntry, RomId, SaveStateEntry};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::{Fullscreen, Window, WindowId};

use crate::audio::Audio;
use crate::chrome::{Chrome, ChromeState, Message, UiAction};
use crate::keymap;
use crate::layout::{self, Layout};
use crate::render::{self, Renderer};

/// How long a notice stays on screen.
const MESSAGE_LIFETIME: Duration = Duration::from_secs(6);

/// How many notices are kept. Older ones fall off rather than pushing the game off the screen.
const MAX_MESSAGES: usize = 5;

/// The floor on redraw interval. Combined with a redraw on every arriving frame, this gives ~60 Hz
/// while playing and a responsive UI when nothing is running.
const MIN_REDRAW_INTERVAL: Duration = Duration::from_millis(15);

/// How often the event loop wakes to check for a new frame.
///
/// The emulation thread cannot wake `winit`'s event loop — there is no waker to hand it — so the
/// loop polls. Two milliseconds is fine-grained enough that a frame is never held back by more than
/// an eighth of its duration, and cheap because the check is one `try_recv`.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

pub struct App {
    paths: AppPaths,
    config: Config,
    library: Library,
    session: Session,
    audio: Audio,

    window: Option<Arc<Window>>,
    renderer: Renderer,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,

    chrome: Chrome,
    tracker: InputTracker,

    /// Cached library reads, so the browser does not query SQLite once per frame.
    roms: Vec<RomEntry>,
    states: Vec<SaveStateEntry>,
    library_error: Option<String>,

    status: SessionStatus,
    stats: SessionStats,
    loaded: Option<LoadedRom>,
    layout: Layout,
    messages: Vec<Message>,

    /// The most recent debugger snapshot. Boxed as it arrives, and kept until replaced so the panel
    /// has something to draw between requests.
    debug: Option<Box<DebugSnapshot>>,
    debug_unavailable: Option<String>,
    /// When the next snapshot request goes out. The panel is refreshed on a timer while running and
    /// on every stop by the session itself.
    next_debug_request: Instant,

    cursor: Option<(f32, f32)>,
    pointer_down: bool,
    fullscreen: bool,
    last_paint: Instant,
    /// Set when a shutdown is under way, so the config is not written twice.
    closing: bool,
    /// A ROM named on the command line, imported and started once the window exists.
    pending_rom: Option<PathBuf>,
}

impl App {
    pub fn new(paths: AppPaths, rom: Option<PathBuf>) -> Result<Self> {
        paths.create_all()?;
        let config = Config::load_or_default(&paths);

        let mut library = Library::open(paths.clone())?;
        // Reconciliation before the browser is first shown, so what the user sees is what is on
        // disk. This is the reconciliation-not-rescan pass; see `library::index`.
        let mut library_error = None;
        let mut messages = Vec::new();
        match catalog::reconcile(&mut library) {
            Ok(report) if report.changed_anything() => messages.push(Message {
                text: format!(
                    "library: {} added, {} moved, {} missing",
                    report.added, report.moved, report.missing
                ),
                is_error: false,
                at: Instant::now(),
            }),
            Ok(_) => {}
            Err(e) => library_error = Some(format!("could not reconcile the library: {e}")),
        }

        let (audio, producer) = Audio::open(frontend_core::DEFAULT_CAPACITY);
        let session = Session::spawn(
            SessionOptions::new(paths.clone(), config.clone())
                .with_audio(producer, audio.output_rate()),
        );

        let egui_ctx = egui::Context::default();
        // The interface is dressed as hardware rather than left on egui's defaults. One palette
        // decides every colour; see `chrome::theme`. Applied again whenever the setting changes.
        crate::chrome::theme::apply(&egui_ctx, config.video.theme.into());
        let renderer = Renderer::new(egui_ctx.clone());

        let mut app = Self {
            paths,
            chrome: Chrome {
                show_hud: config.video.hud_visible,
                ..Chrome::default()
            },
            config,
            library,
            session,
            audio,
            window: None,
            renderer,
            egui_ctx,
            egui_state: None,
            tracker: InputTracker::new(),
            roms: Vec::new(),
            states: Vec::new(),
            library_error,
            status: SessionStatus::Idle,
            stats: SessionStats::default(),
            loaded: None,
            layout: Layout::none(),
            messages,
            debug: None,
            debug_unavailable: None,
            next_debug_request: Instant::now(),
            cursor: None,
            pointer_down: false,
            fullscreen: false,
            last_paint: Instant::now(),
            closing: false,
            pending_rom: rom,
        };
        app.refresh_roms();
        Ok(app)
    }

    // --- library reads --------------------------------------------------------------------

    fn refresh_roms(&mut self) {
        match self.library.roms() {
            Ok(roms) => {
                self.roms = roms;
                self.library_error = None;
            }
            Err(e) => self.library_error = Some(format!("could not read the library: {e}")),
        }
    }

    fn refresh_states(&mut self) {
        let Some(rom_id) = self.loaded.as_ref().and_then(|rom| rom.rom_id) else {
            self.states.clear();
            return;
        };
        match self.library.states_for(rom_id) {
            Ok(states) => self.states = states,
            Err(e) => self.library_error = Some(format!("could not read save states: {e}")),
        }
    }

    fn note(&mut self, text: impl Into<String>, is_error: bool) {
        let text = text.into();
        if is_error {
            tracing::warn!("{text}");
        } else {
            tracing::info!("{text}");
        }
        self.messages.push(Message {
            text,
            is_error,
            at: Instant::now(),
        });
        if self.messages.len() > MAX_MESSAGES {
            self.messages.remove(0);
        }
    }

    // --- session events -------------------------------------------------------------------

    fn pump_session_events(&mut self) {
        for event in self.session.drain_events() {
            match event {
                SessionEvent::RomLoaded(rom) => {
                    // The single most important event in the application, and the first thing a bug
                    // report needs: which file, on which machine, with or without its save.
                    tracing::info!(
                        "playing {} ({}) from {}{}",
                        rom.title,
                        rom.platform.display_name(),
                        rom.path.display(),
                        if rom.save_ram_restored {
                            ", save RAM restored"
                        } else {
                            ""
                        }
                    );
                    if let Some(id) = rom.rom_id {
                        if let Err(e) = self.library.mark_played(id) {
                            self.note(format!("could not record the play time: {e}"), true);
                        }
                        self.refresh_roms();
                    }
                    if let Some(window) = &self.window {
                        window.set_title(&format!("{} — Alpha Emulator", rom.title));
                    }
                    // Playing is the point of the application, so the library panel gets out of the
                    // way once something is running. It is one click to bring back.
                    self.chrome.show_library = false;
                    self.loaded = Some(rom);
                    self.refresh_states();
                }
                SessionEvent::RomClosed => {
                    self.debug = None;
                    self.debug_unavailable = None;
                    self.loaded = None;
                    self.states.clear();
                    self.chrome.show_library = true;
                    self.session.frames().clear();
                    self.renderer.clear_screen_texture();
                    self.layout = Layout::none();
                    if let Some(window) = &self.window {
                        window.set_title("Alpha Emulator");
                    }
                }
                SessionEvent::StatusChanged(status) => self.status = status,
                SessionEvent::Stats(stats) => {
                    // At debug level so it is available when diagnosing "it runs slowly" without
                    // filling the log during ordinary play.
                    tracing::debug!(
                        "{:.0}% ({:.1} fps), frame {}, {} frames and {} samples dropped",
                        stats.speed_percent,
                        stats.fps,
                        stats.frame,
                        stats.frames_dropped,
                        stats.audio_dropped
                    );
                    self.stats = stats;
                }
                SessionEvent::StateSaved(saved) => {
                    let label = saved.label.clone();
                    let frame = saved.frame;
                    match catalog::record_saved_state(&mut self.library, &saved) {
                        Ok(_) => self.refresh_states(),
                        Err(e) => self.note(format!("could not index the save state: {e}"), true),
                    }
                    self.note(format!("saved “{label}” at frame {frame}"), false);
                }
                SessionEvent::StateLoaded { frame, .. } => {
                    self.note(format!("loaded frame {frame}"), false);
                }
                SessionEvent::SaveRamWritten { path } => {
                    tracing::debug!("save RAM written to {}", path.display());
                }
                SessionEvent::Notice(text) => self.note(text, false),
                SessionEvent::Error(text) => self.note(text, true),

                SessionEvent::DebugSnapshot(snapshot) => {
                    self.debug_unavailable = None;
                    self.debug = Some(snapshot);
                }
                SessionEvent::BreakpointHit { addr } => {
                    // Worth telling the user about even with the panel open: a breakpoint hit while
                    // they were looking at the game is otherwise just the picture stopping.
                    let digits =
                        self.debug.as_ref().map(|s| s.address_digits).unwrap_or(4) as usize;
                    self.note(format!("breakpoint at {addr:0>digits$X}"), false);
                    self.chrome.show_debugger = true;
                }
                SessionEvent::WatchpointHit { addr, write, value } => {
                    let digits =
                        self.debug.as_ref().map(|s| s.address_digits).unwrap_or(4) as usize;
                    self.note(
                        format!(
                            "watchpoint: {} {addr:0>digits$X} = {value:02X}",
                            if write { "wrote" } else { "read" }
                        ),
                        false,
                    );
                    self.chrome.show_debugger = true;
                }
                SessionEvent::DebugUnavailable(reason) => {
                    self.debug = None;
                    self.debug_unavailable = Some(reason);
                }
            }
        }
        let now = Instant::now();
        self.messages
            .retain(|message| now.duration_since(message.at) < MESSAGE_LIFETIME);
    }

    // --- actions --------------------------------------------------------------------------

    fn apply(&mut self, action: UiAction, event_loop: &ActiveEventLoop) {
        match action {
            UiAction::Play(id) => self.play(id),
            UiAction::Import(paths) => self.import(&paths),
            UiAction::ImportTyped(text) => {
                let path = PathBuf::from(shellexpand_home(&text));
                if path.exists() {
                    self.import(&[path]);
                } else {
                    self.note(format!("{} does not exist", path.display()), true);
                }
            }
            UiAction::Rescan => match catalog::reconcile(&mut self.library) {
                Ok(report) => {
                    self.refresh_roms();
                    self.note(
                        format!(
                            "{} added, {} moved, {} missing, {} states dropped, {} adopted",
                            report.added,
                            report.moved,
                            report.missing,
                            report.states_dropped,
                            report.states_adopted
                        ),
                        false,
                    );
                }
                Err(e) => self.note(format!("rescan failed: {e}"), true),
            },
            UiAction::Forget { rom, delete_states } => {
                match self.library.remove_rom(rom, delete_states) {
                    Ok(()) => {
                        if self.chrome.selected == Some(rom) {
                            self.chrome.selected = None;
                        }
                        self.refresh_roms();
                        self.note("removed from the library", false);
                    }
                    Err(e) => self.note(format!("could not remove it: {e}"), true),
                }
            }
            UiAction::Rename { rom, title } => match self.library.set_title(rom, &title) {
                Ok(()) => self.refresh_roms(),
                Err(e) => self.note(format!("could not rename it: {e}"), true),
            },

            UiAction::Session(command) => self.session.send(command),
            UiAction::SaveNamed(label) => self.session.send(SessionCommand::SaveState {
                slot: None,
                label: Some(label),
            }),
            UiAction::DeleteState(id) => match self.library.delete_state(id) {
                Ok(()) => {
                    self.refresh_states();
                    self.note("save state deleted", false);
                }
                Err(e) => self.note(format!("could not delete it: {e}"), true),
            },

            UiAction::SetScaling(mode) => self.config.video.scaling = mode,
            UiAction::SetVolume(volume) => {
                self.config.audio.volume = volume;
                self.session.send(SessionCommand::SetVolume(volume));
            }
            UiAction::SetMuted(muted) => {
                self.config.audio.muted = muted;
                self.session.send(SessionCommand::SetMuted(muted));
            }
            UiAction::SetFastForwardSpeed(speed) => {
                self.config.emulation.fast_forward_speed = speed;
                self.session
                    .send(SessionCommand::SetFastForwardSpeed(speed));
            }
            UiAction::SetPauseOnFocusLoss(on) => self.config.emulation.pause_on_focus_loss = on,
            UiAction::SetDualScreenGap(gap) => self.config.video.dual_screen_gap = gap,
            UiAction::SetTheme(theme) => {
                self.config.video.theme = theme;
                crate::chrome::theme::apply(&self.egui_ctx, theme.into());
            }
            UiAction::SetRewind(rewind) => {
                self.config.rewind = rewind;
                self.session.send(SessionCommand::SetRewindConfig(rewind));
            }

            UiAction::Rebind { key, action } => self.rebind(key, action),
            UiAction::Unbind(key) => {
                self.config.keybinds.unbind(key);
            }
            UiAction::ResetKeybinds => {
                self.config.keybinds = KeybindMap::defaults();
                self.note("keybinds restored to defaults", false);
            }

            UiAction::Screenshot => self.screenshot(),
            UiAction::ToggleFullscreen => self.toggle_fullscreen(),
            UiAction::Quit => self.shutdown(event_loop),
        }
    }

    fn play(&mut self, id: RomId) {
        let rom = match self.library.rom(id) {
            Ok(Some(rom)) => rom,
            Ok(None) => return self.note("that ROM is no longer in the library", true),
            Err(e) => return self.note(format!("could not read the library: {e}"), true),
        };
        if !rom.present {
            return self.note(
                format!(
                    "{} is not where the library remembers it — put it back, or rescan",
                    rom.path.display()
                ),
                true,
            );
        }
        self.session.send(SessionCommand::LoadRom {
            path: rom.path,
            rom_id: Some(id),
        });
    }

    fn import(&mut self, paths: &[PathBuf]) {
        // Runs on this thread and reads every file to hash it, which for a large folder blocks the
        // UI. Acceptable for an explicit, one-off action the user initiated and is waiting on;
        // moving it to a worker is the right change once a progress indicator exists to justify it.
        let (ids, problems) = catalog::import_dropped(&mut self.library, paths);
        self.refresh_roms();
        for problem in &problems {
            self.note(problem.clone(), true);
        }
        match ids.len() {
            0 if problems.is_empty() => self.note("nothing to import there", false),
            0 => {}
            1 => self.note("imported 1 ROM", false),
            n => self.note(format!("imported {n} ROMs"), false),
        }
        // A single dropped file is almost certainly meant to be played straight away.
        if ids.len() == 1 && problems.is_empty() {
            let id = ids[0];
            if self
                .library
                .rom(id)
                .ok()
                .flatten()
                .is_some_and(|rom| rom.platform.is_runnable())
            {
                self.play(id);
            }
        }
    }

    /// Bind a key, showing the conflict before displacing anything.
    ///
    /// `frontend-core` owns the rule; this is the UI half of it. `bind` is tried first so the
    /// refusal can be reported, and only then is the key taken — which is what makes the
    /// displacement something the user was told about rather than something that just happened.
    fn rebind(&mut self, key: frontend_core::PhysicalKey, action: Action) {
        match self.config.keybinds.bind(key, action) {
            Ok(()) => {}
            Err(_) => {
                let displaced = self.config.keybinds.rebind(key, action);
                if let Some(displaced) = displaced {
                    self.note(
                        format!(
                            "{key:?} was bound to {}; it now does something else",
                            describe_action(displaced)
                        ),
                        false,
                    );
                }
            }
        }
    }

    fn screenshot(&mut self) {
        let Some(frame) = self.session.frames().current() else {
            return self.note("there is no frame to capture", true);
        };
        let title = self
            .loaded
            .as_ref()
            .map(|rom| rom.title.clone())
            .unwrap_or_else(|| "alpha".to_string());
        match frontend_core::png::save_screenshot(
            &self.paths.screenshots_dir(),
            &title,
            &frame.buffer,
        ) {
            Ok(path) => self.note(format!("screenshot: {}", path.display()), false),
            Err(e) => self.note(format!("could not write the screenshot: {e}"), true),
        }
    }

    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        if let Some(window) = &self.window {
            window.set_fullscreen(if self.fullscreen {
                Some(Fullscreen::Borderless(None))
            } else {
                None
            });
        }
    }

    fn shutdown(&mut self, event_loop: &ActiveEventLoop) {
        if self.closing {
            return;
        }
        self.closing = true;
        // Persist the HUD's visibility along with everything else, so it is where the user left it.
        self.config.video.hud_visible = self.chrome.show_hud;
        if let Err(e) = self.config.save(&self.paths) {
            tracing::error!("could not write the settings file: {e}");
        }
        // Flush save RAM before the process can exit. `Session::drop` also does this, but doing it
        // here means it happens before the window tears down rather than during it.
        self.session.send(SessionCommand::FlushSaveRam);
        self.session.send(SessionCommand::CloseRom);
        event_loop.exit();
    }

    // --- input ----------------------------------------------------------------------------

    /// Publish the current button state. One atomic store; safe to call as often as it is useful.
    fn publish_input(&mut self) {
        let mut state = self.tracker.input_state(&self.config.keybinds);
        // Touch is derived from where the pointer is, not tracked as a key, so it is set here
        // rather than in the tracker's keyboard path.
        state.touch = self.touch_point();
        self.session.set_input(state);
    }

    fn touch_point(&self) -> Option<core_common::TouchPoint> {
        if !self.pointer_down {
            return None;
        }
        let (x, y) = self.cursor?;
        self.layout.touch_at(x, y)
    }

    /// Apply the held fast-forward and rewind keys, which are level- not edge-triggered.
    fn apply_held_actions(&mut self) {
        let fast_forward = self
            .tracker
            .is_held(&self.config.keybinds, ChromeAction::FastForward);
        let rewinding = self
            .tracker
            .is_held(&self.config.keybinds, ChromeAction::Rewind);
        // Sent unconditionally: the session ignores a value it already holds, and tracking the
        // previous value here would be a second copy of state that can go stale.
        self.session
            .send(SessionCommand::SetFastForward(fast_forward));
        self.session.send(SessionCommand::SetRewinding(rewinding));
    }

    /// Ask the session for a fresh snapshot, a few times a second while the panel is open.
    ///
    /// A timer rather than one request per redraw: a snapshot is a few hundred peeks on the emulation
    /// thread, and sixty of them a second would be work taken from the machine to show a number that
    /// nobody can read that fast. The session also re-serves the last request on every stop, so a
    /// paused or single-stepped machine updates immediately regardless of this timer.
    fn request_debug_snapshot_if_due(&mut self) {
        const INTERVAL: Duration = Duration::from_millis(100);
        if !self.chrome.show_debugger || self.loaded.is_none() {
            return;
        }
        if Instant::now() < self.next_debug_request {
            return;
        }
        self.next_debug_request = Instant::now() + INTERVAL;
        self.session.send(SessionCommand::RequestDebugSnapshot(
            crate::chrome::debug_request(&self.chrome),
        ));
    }

    // --- drawing --------------------------------------------------------------------------

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let Some(egui_state) = self.egui_state.as_mut() else {
            return;
        };
        self.last_paint = Instant::now();

        let raw_input = egui_state.take_egui_input(&window);
        // The framebuffer size comes from the frame in hand when there is one, and from the
        // platform's declared size when there is not — so the picture does not jump on the first
        // frame after a load.
        let framebuffer = self
            .session
            .frames()
            .current()
            .map(|frame: &Frame| (frame.buffer.width(), frame.buffer.height()))
            .or_else(|| self.loaded.as_ref().map(|rom| screen_size(rom.platform)))
            .unwrap_or((0, 0));

        let texture = {
            let scaling = self.config.video.scaling;
            match self.session.frames().current() {
                // `upload` needs `&mut self.renderer` and the frame borrows `self.session`, so the
                // frame is cloned out of the way. It is a `&Frame` reborrow, not a pixel copy —
                // `current()` returns a reference and `upload` reads through it.
                Some(_) => {
                    let frame = self.session.frames().current().expect("checked").clone();
                    self.renderer.upload(&frame, scaling)
                }
                None => None,
            }
        };

        let mut actions = Vec::new();
        let output = self.egui_ctx.clone().run_ui(raw_input, |root| {
            let chrome_state = ChromeState {
                status: self.status,
                stats: self.stats,
                loaded: self.loaded.as_ref(),
                roms: &self.roms,
                states: &self.states,
                keybinds: &self.config.keybinds,
                config: &self.config,
                audio_description: &self.audio.describe(),
                gpu_description: &self.renderer.adapter_summary(),
                library_error: self.library_error.as_deref(),
                debug: self.debug.as_deref(),
                debug_unavailable: self.debug_unavailable.as_deref(),
                fullscreen: self.fullscreen,
                messages: &self.messages,
            };
            actions = self.chrome.ui(root, &chrome_state);

            // The game fills whatever the panels left over. Drawing it in a frameless central
            // panel is what makes the layout's rectangle and the drawn rectangle the same number.
            egui::CentralPanel::no_frame().show(root, |ui| {
                let available = ui.available_rect_before_wrap();
                // The bezel. Painted before the screens rather than set as a panel fill, so the
                // layout's rectangle and the drawn rectangle stay the same number — which is the
                // reason the panel is frameless in the first place.
                let bezel = crate::chrome::theme::Palette::from(self.config.video.theme).bezel;
                ui.painter().rect_filled(available, 0.0, bezel);
                self.layout = layout::compute(
                    framebuffer,
                    self.loaded
                        .as_ref()
                        .is_some_and(|rom| frontend_core::is_dual_screen(rom.platform)),
                    self.config.video.dual_screen_gap,
                    layout::Rect::new(
                        available.min.x,
                        available.min.y,
                        available.width(),
                        available.height(),
                    ),
                    self.config.video.scaling,
                );
                match texture {
                    Some(texture) => {
                        render::draw_screens(ui.painter(), &self.layout, texture, framebuffer);
                    }
                    None => idle_message(ui, self.loaded.is_some()),
                }
            });
        });

        egui_state.handle_platform_output(&window, output.platform_output);
        let primitives = self
            .egui_ctx
            .tessellate(output.shapes, output.pixels_per_point);
        self.renderer.paint(
            &window,
            output.pixels_per_point,
            &primitives,
            &output.textures_delta,
        );

        for action in actions {
            self.apply(action, event_loop);
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Sized for a 3× Game Boy Advance with room for the chrome — large enough to be usable
        // immediately, small enough not to fill a laptop display.
        let (width, height) = screen_size(library::Platform::Gba);
        let attributes = Window::default_attributes()
            .with_title("Alpha Emulator")
            .with_inner_size(winit::dpi::LogicalSize::new(
                (width * 3 + 380) as f64,
                (height * 3 + 60) as f64,
            ))
            .with_min_inner_size(winit::dpi::LogicalSize::new(480.0, 320.0));

        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(e) => {
                tracing::error!("could not create a window: {e}");
                event_loop.exit();
                return;
            }
        };
        if let Err(e) = self.renderer.set_window(window.clone()) {
            tracing::error!("{e:#}");
            event_loop.exit();
            return;
        }

        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            window.theme(),
            self.renderer.max_texture_side(),
        ));
        self.window = Some(window);
        tracing::info!("GPU: {}", self.renderer.adapter_summary());

        // Deferred to here rather than done in `new`: importing may report a problem, and a message
        // posted before there is a window to show it in would be gone by the time one existed.
        if let Some(rom) = self.pending_rom.take() {
            self.import(&[rom]);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone() else {
            return;
        };

        // egui sees every event first and reports whether it wants it. That report is what keeps a
        // keystroke meant for a text field out of the emulated console.
        let consumed = match self.egui_state.as_mut() {
            Some(state) => state.on_window_event(&window, &event).consumed,
            None => false,
        };

        match event {
            WindowEvent::CloseRequested => self.shutdown(event_loop),

            WindowEvent::Resized(size) => {
                self.renderer.on_window_resized(size.width, size.height);
            }

            WindowEvent::ScaleFactorChanged { .. } => {
                let size = window.inner_size();
                self.renderer.on_window_resized(size.width, size.height);
            }

            WindowEvent::RedrawRequested => self.redraw(event_loop),

            WindowEvent::DroppedFile(path) => {
                // winit delivers one event per dropped file, so a multi-file drop arrives as
                // several. Importing each as it lands is correct and keeps no partial state.
                // Routed as an action rather than called directly, so every import in the
                // application goes through the same one place.
                self.apply(UiAction::Import(vec![path]), event_loop);
            }

            WindowEvent::Focused(focused) => {
                if !focused {
                    // Without this, a key held at the moment of alt-tabbing stays down forever:
                    // its release goes to whichever window took the focus.
                    self.tracker.release_all();
                    self.pointer_down = false;
                    self.publish_input();
                    if self.config.emulation.pause_on_focus_loss && self.loaded.is_some() {
                        self.session.send(SessionCommand::SetPaused(true));
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                let logical = position.to_logical::<f32>(window.scale_factor());
                self.cursor = Some((logical.x, logical.y));
                if self.pointer_down {
                    self.publish_input();
                }
            }

            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                self.pointer_down = false;
                self.publish_input();
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    // A click that egui took belongs to a panel, not to the touch screen beneath
                    // it. A release is always honoured, or a stylus could get stuck down by
                    // releasing the mouse over a panel.
                    self.pointer_down = state == ElementState::Pressed && !consumed;
                    self.publish_input();
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                let winit::keyboard::PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let Some(key) = keymap::translate(code) else {
                    return;
                };
                let pressed = event.state == ElementState::Pressed;

                // A capture in progress takes the key before anything else can, including egui's
                // own text handling — binding a key must not also type it.
                if self.chrome.capturing.is_some() {
                    if pressed && !event.repeat {
                        if let Some(action) = crate::chrome::resolve_capture(&mut self.chrome, key)
                        {
                            self.apply(action, event_loop);
                        }
                    }
                    return;
                }
                if self.chrome.wants_keyboard(consumed) {
                    return;
                }

                self.tracker.apply(
                    &self.config.keybinds,
                    frontend_core::PhysicalInputEvent { key, pressed },
                );
                for action in self.tracker.take_chrome_actions() {
                    if let Some(ui_action) = self.chrome.handle_chrome_action(action) {
                        self.apply(ui_action, event_loop);
                    }
                }
                self.apply_held_actions();
                self.publish_input();
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.pump_session_events();
        self.request_debug_snapshot_if_due();

        // If the emulation thread has died — a panic in a system — say so once and stop, rather
        // than presenting a frozen picture that looks like a hang.
        if !self.session.is_alive() && !self.closing && self.loaded.is_some() {
            self.note("the emulation thread stopped; see the log", true);
            self.loaded = None;
        }

        let new_frame = self.session.frames().poll();
        let due = self.last_paint.elapsed() >= MIN_REDRAW_INTERVAL;
        if let Some(window) = &self.window {
            if new_frame || due {
                window.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL_INTERVAL));
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.shutdown(event_loop);
    }
}

/// What the central area shows when there is nothing to draw.
fn idle_message(ui: &mut egui::Ui, loaded: bool) {
    ui.centered_and_justified(|ui| {
        ui.label(
            egui::RichText::new(if loaded {
                "waiting for the first frame…"
            } else {
                "Drop a .gb, .gbc, .gba, or .nds file here, or pick one from the library."
            })
            .weak(),
        );
    });
}

fn describe_action(action: Action) -> String {
    match action {
        Action::Button(buttons) => format!("button {buttons:?}"),
        Action::Chrome(chrome) => chrome.name().to_string(),
    }
}

/// Expand a leading `~` in a typed path.
///
/// The import box is where people paste paths from a terminal, and a `~` that is taken literally
/// produces a baffling "does not exist" for a path the user can see is right. Only the leading
/// `~/` form is handled; `~user` is a different lookup and nobody pastes it.
fn shellexpand_home(text: &str) -> String {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(rest)
                .to_string_lossy()
                .into_owned();
        }
    }
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leading_tilde_is_expanded_but_nothing_else_is_touched() {
        // Safety of the assumption below: `HOME` is set in every environment the tests run in, and
        // the fallback path is exercised by the unchanged cases.
        if let Some(home) = std::env::var_os("HOME") {
            let home = PathBuf::from(home);
            assert_eq!(
                shellexpand_home("~/roms/game.gb"),
                home.join("roms/game.gb").to_string_lossy()
            );
        }
        assert_eq!(shellexpand_home("/abs/game.gb"), "/abs/game.gb");
        assert_eq!(shellexpand_home("  spaced.gb  "), "spaced.gb");
        assert_eq!(
            shellexpand_home("~weird/game.gb"),
            "~weird/game.gb",
            "only the ~/ form is a home reference"
        );
    }

    #[test]
    fn actions_are_described_for_a_person_not_for_a_debugger() {
        assert_eq!(
            describe_action(Action::Chrome(ChromeAction::FastForward)),
            "fast-forward"
        );
    }
}
