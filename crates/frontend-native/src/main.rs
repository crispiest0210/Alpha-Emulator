//! The shipped desktop application: `winit` window, `wgpu` rendering, `egui` chrome.
//!
//! # What lives here and what does not
//!
//! Presentation and input capture, and nothing else. The emulation thread, the audio pipeline, the
//! rewind buffer, the settings format, and the library index are all in `frontend-core` and
//! `library`, behind a channel API this crate consumes without being able to reach past it. That is
//! not tidiness: it is the reason a web or TUI frontend could be written against `frontend-core`
//! without touching a line of this crate, and the reason the accuracy harness can drive the same
//! session runtime with no display.
//!
//! The module split, smallest responsibility first:
//!
//! - [`layout`] — where the picture goes and where a click lands in it. Pure arithmetic, unit-tested.
//! - [`keymap`] — `winit` key codes to `frontend-core` keys. Pure, unit-tested.
//! - [`block_on`] — a twenty-line executor for `wgpu`'s three `async` setup calls.
//! - [`audio`] — the `cpal` output stream, the only place in the workspace that opens a device.
//! - [`render`] — the surface, and the framebuffer as a GPU texture.
//! - [`chrome`] — the panels, which return [`chrome::UiAction`]s rather than doing anything. That
//!   includes the debugger panel: registers, disassembly, memory, breakpoints.
//! - [`app`] — the composition: route an event, apply an action, draw a frame.
//!
//! # Status
//!
//! Complete for prompt 14. Library browser with import by drag-and-drop or pasted path, gameplay
//! with video, audio, and input, quicksave and quickload across ten slots plus named states,
//! rewind, an HUD of measured figures, a keybind configurator, settings persisted as TOML, and
//! screenshots.
//!
//! Prompt 15's debugger panel is here too, in `chrome/debugger_view.rs`: registers, disassembly with
//! the program counter highlighted and click-to-toggle breakpoints, a hex viewer with a region jump
//! list, and instruction stepping. Watchpoints appear in the registry but do not halt yet, and the
//! panel says so rather than offering a control that does nothing.
//!
//! Not done: no native file dialog, because that would be a dependency for one button when
//! drag-and-drop already covers the gesture.

#![deny(unsafe_code)]

mod app;
mod audio;
mod block_on;
mod chrome;
mod keymap;
mod layout;
mod render;

use anyhow::Result;
use winit::event_loop::EventLoop;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut arguments = Arguments::parse(std::env::args().skip(1));
    if arguments.help {
        println!("{USAGE}");
        return Ok(());
    }

    // `--data-dir` puts the whole layout — index, saves, states, config — somewhere else. It exists
    // so a second copy can be run without touching the real library, which is what makes this
    // application testable by hand at all: a change to the import path should not be tried out
    // against the games someone actually plays.
    let paths = match arguments.data_dir.take() {
        Some(root) => library::AppPaths::rooted_at(root),
        None => library::AppPaths::discover(),
    };
    tracing::info!("data: {}", paths.data_dir().display());
    tracing::info!("config: {}", paths.config_file().display());

    let event_loop = EventLoop::new()?;
    let mut app = app::App::new(paths, arguments.rom)?;
    event_loop.run_app(&mut app)?;
    Ok(())
}

const USAGE: &str = "\
alpha-emulator [OPTIONS] [ROM]

  ROM                 A .gb, .gbc, .gba, or .nds file to import and start playing.
  --data-dir <PATH>   Use this directory for the library, saves, states, and config
                      instead of the OS-appropriate one.
  --help              Print this and exit.
";

/// Command-line arguments.
///
/// Parsed by hand rather than with `clap`. There are three of them, they have no subcommands, and
/// this application is not a CLI — `frontend-headless` is, and that is where `clap` belongs.
#[derive(Debug, Default, PartialEq)]
struct Arguments {
    rom: Option<std::path::PathBuf>,
    data_dir: Option<std::path::PathBuf>,
    help: bool,
}

impl Arguments {
    fn parse(arguments: impl Iterator<Item = String>) -> Self {
        let mut parsed = Self::default();
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--help" | "-h" => parsed.help = true,
                "--data-dir" => parsed.data_dir = arguments.next().map(Into::into),
                // An unrecognised flag is reported through the usage text rather than ignored, so a
                // typo does not silently start the application with the wrong library.
                other if other.starts_with('-') => {
                    eprintln!("unrecognised option {other:?}");
                    parsed.help = true;
                }
                other => parsed.rom = Some(other.into()),
            }
        }
        parsed
    }
}

#[cfg(test)]
mod tests {
    use super::Arguments;

    fn parse(arguments: &[&str]) -> Arguments {
        Arguments::parse(arguments.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_means_the_real_library_and_no_rom() {
        assert_eq!(parse(&[]), Arguments::default());
    }

    #[test]
    fn a_bare_path_is_the_rom_to_play() {
        let parsed = parse(&["/games/zelda.gbc"]);
        assert_eq!(
            parsed.rom.as_deref(),
            Some(std::path::Path::new("/games/zelda.gbc"))
        );
        assert_eq!(parsed.data_dir, None);
    }

    #[test]
    fn a_data_dir_takes_the_next_argument_and_leaves_the_rom_alone() {
        let parsed = parse(&["--data-dir", "/tmp/scratch", "/games/x.gb"]);
        assert_eq!(
            parsed.data_dir.as_deref(),
            Some(std::path::Path::new("/tmp/scratch"))
        );
        assert_eq!(
            parsed.rom.as_deref(),
            Some(std::path::Path::new("/games/x.gb"))
        );
    }

    #[test]
    fn an_unrecognised_flag_asks_for_help_rather_than_being_dropped() {
        assert!(parse(&["--turbo"]).help);
        assert!(parse(&["--help"]).help);
    }

    #[test]
    fn a_dangling_data_dir_does_not_swallow_a_following_rom_that_is_not_there() {
        let parsed = parse(&["--data-dir"]);
        assert_eq!(parsed.data_dir, None);
        assert_eq!(parsed.rom, None);
    }
}
