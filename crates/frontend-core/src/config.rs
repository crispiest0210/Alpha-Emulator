//! The user's settings file: keybinds, audio, presentation, rewind.
//!
//! TOML, at the OS-appropriate config path, hand-editable on purpose. Everything here is a
//! preference — nothing in this file affects emulation *accuracy*, and nothing that does may be
//! added to it. A settings file that can change how a machine behaves turns every bug report
//! into "what is in your config?".
//!
//! # Why a malformed file does not stop the program
//!
//! [`Config::load`] logs and returns defaults. An emulator that refuses to start
//! because one line of TOML is wrong is worse than one that starts with default keybinds and
//! says so, and the file is hand-editable precisely so people will edit it by hand and get it
//! wrong sometimes. It also reports [`ConfigOrigin::Unusable`], so the shutdown save
//! ([`Config::save_over`]) moves the broken file to `config.toml.bad` before writing — loading
//! never touched it, but quitting used to overwrite it with the defaults, and whatever the user
//! was trying to express was gone.

use crate::input::KeybindMap;
use library::AppPaths;
use serde::{Deserialize, Serialize};

/// How the emulated framebuffer is scaled into the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScalingMode {
    /// Nearest-neighbour to fill the window, preserving aspect ratio. The default: these are
    /// pixel-art machines and a blurred Game Boy is a worse picture, not a softer one.
    #[default]
    Nearest,
    /// Nearest-neighbour at the largest whole-number multiple that fits, letterboxed.
    ///
    /// The only mode where every emulated pixel is exactly the same size on screen. At a
    /// non-integer scale, nearest-neighbour necessarily makes some pixel rows one screen pixel
    /// taller than their neighbours, which reads as a shimmer when the picture scrolls.
    IntegerNearest,
    /// Bilinear filtering. Present because some people prefer it on a large display.
    Linear,
}

impl ScalingMode {
    pub const ALL: &'static [ScalingMode] = &[
        ScalingMode::Nearest,
        ScalingMode::IntegerNearest,
        ScalingMode::Linear,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            ScalingMode::Nearest => "Nearest (fit)",
            ScalingMode::IntegerNearest => "Integer scale",
            ScalingMode::Linear => "Bilinear",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Linear gain, 0.0 to 1.0. Applied on the emulation thread before samples reach the ring,
    /// because the audio callback must do as close to nothing as possible.
    pub volume: f32,
    pub muted: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            volume: 0.7,
            muted: false,
        }
    }
}

/// The hardware the interface is dressed as.
///
/// Named after the consoles rather than after "light" and "dark" because that is what a user
/// choosing one is actually picking, and because two of them are light and two dark.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    /// Off-white shell, cool silver trim, a soft blue accent.
    #[default]
    DsLite,
    /// The Game Boy Advance's indigo-violet.
    Gba,
    /// The original DS's titanium grey with an amber accent.
    DsPhat,
    /// The original Game Boy's olive-green LCD.
    GameBoy,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 4] = [
        ThemeChoice::DsLite,
        ThemeChoice::Gba,
        ThemeChoice::DsPhat,
        ThemeChoice::GameBoy,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::DsLite => "DS Lite",
            ThemeChoice::Gba => "Game Boy Advance",
            ThemeChoice::DsPhat => "Nintendo DS",
            ThemeChoice::GameBoy => "Game Boy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoConfig {
    pub scaling: ScalingMode,
    /// Show the HUD from startup.
    pub hud_visible: bool,
    /// Vertical gap, in emulated pixels, between the DS's two screens.
    ///
    /// Presentation only. The emulation core produces the two screens with no gap; inserting one
    /// is the frontend's business, and it is a setting because the right value depends on how
    /// large the window is.
    pub dual_screen_gap: u32,
    /// Which hardware the interface is dressed as.
    ///
    /// Presentation only, and stored here rather than in `frontend-native` because a setting the
    /// user can change belongs with the rest of them — the *colours* live in the frontend, since
    /// `frontend-core` may not depend on a UI framework.
    pub theme: ThemeChoice,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            scaling: ScalingMode::Nearest,
            hud_visible: false,
            dual_screen_gap: 8,
            theme: ThemeChoice::default(),
        }
    }
}

/// `Copy` because it is four scalars that the settings panel copies to compare a pending change
/// against the current one, and threading a clone through that would only obscure it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RewindConfig {
    pub enabled: bool,
    /// How far back rewind can reach, in seconds of play.
    pub seconds: u32,
    /// Frames between snapshots.
    ///
    /// The memory/granularity trade-off prompt 14 asks to be configurable. A snapshot every
    /// frame would be exact and enormous; every second would be cheap and useless. Every six
    /// frames is a tenth of a second, which is under the threshold at which rewind stops feeling
    /// continuous.
    pub interval_frames: u32,
}

impl Default for RewindConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            seconds: 30,
            interval_frames: 6,
        }
    }
}

impl RewindConfig {
    /// How many snapshots the ring must hold to cover [`seconds`](Self::seconds).
    pub fn snapshot_capacity(&self, frame_rate: f64) -> usize {
        if !self.enabled || self.interval_frames == 0 {
            return 0;
        }
        let frames = self.seconds as f64 * frame_rate;
        ((frames / self.interval_frames as f64).ceil() as usize).max(1)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmulationConfig {
    /// Speed multiplier while fast-forward is held. `0.0` means uncapped — run as fast as the
    /// host manages.
    ///
    /// Expressed as a multiplier rather than "run the UI loop faster", which is the distinction
    /// prompt 14 draws: the emulation thread changes its own pacing target and the UI keeps
    /// redrawing at the display's rate. Speeding up the UI loop instead is what makes
    /// fast-forward in a single-threaded emulator drop input and stutter.
    pub fast_forward_speed: f32,
    /// Pause when the window loses focus.
    pub pause_on_focus_loss: bool,
}

impl Default for EmulationConfig {
    fn default() -> Self {
        Self {
            fast_forward_speed: 4.0,
            pause_on_focus_loss: false,
        }
    }
}

/// Everything persisted between runs that is not library data.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub audio: AudioConfig,
    pub video: VideoConfig,
    pub rewind: RewindConfig,
    pub emulation: EmulationConfig,
    /// Physical-key to action map. Serialised by its own config names, so the file reads
    /// `W = "button_up"`.
    pub keybinds: KeybindMap,
}

/// What [`Config::load`] found on disk, which decides what [`Config::save_over`] may do to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOrigin {
    /// Read and parsed; the running settings came from it.
    File,
    /// There was no file; the running settings are defaults.
    Absent,
    /// A file exists but could not be used — unparsable, not UTF-8, or unreadable — so the running
    /// settings are defaults and the file holds something the user wrote that is not in them.
    Unusable,
}

impl Config {
    /// Read the settings file, falling back to defaults on anything unusable, and say which.
    ///
    /// The origin is returned rather than only logged because the shutdown save needs it: an
    /// unusable file was the user's, a typo away from working, and writing the defaults over it
    /// on quit used to destroy every keybind in it.
    pub fn load(paths: &AppPaths) -> (Self, ConfigOrigin) {
        let path = paths.config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(config) => (config.clamped(), ConfigOrigin::File),
                Err(e) => {
                    tracing::warn!(
                        "{} could not be parsed ({e}); using defaults. It will be kept as \
                         config.toml.bad when settings are next saved.",
                        path.display()
                    );
                    (Self::default(), ConfigOrigin::Unusable)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                (Self::default(), ConfigOrigin::Absent)
            }
            Err(e) => {
                tracing::warn!("{} could not be read ({e}); using defaults", path.display());
                (Self::default(), ConfigOrigin::Unusable)
            }
        }
    }

    /// Write the settings file, creating its directory.
    ///
    /// Overwrites unconditionally. The application's own save goes through
    /// [`save_over`](Self::save_over), which knows whether there is anything there to keep.
    pub fn save(&self, paths: &AppPaths) -> std::io::Result<()> {
        let path = paths.config_file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // A settings file is rewritten on every quit. Truncating the real file and then being
        // killed mid-write leaves a zero-byte config and loses every keybind the user set — cheap
        // to avoid, and the same reason the library index runs in WAL mode.
        crate::durable::write_atomically(&path, text.as_bytes())
    }

    /// Save, first moving an [unusable](ConfigOrigin::Unusable) file aside to `config.toml.bad`
    /// (or `.bad.1`, … if one is already there).
    ///
    /// Back up and then save, rather than skipping the save. Skipping keeps the broken file in
    /// place but silently discards every setting changed this session, and the next launch is on
    /// defaults again with the same parse warning; backing up keeps both the user's edit and a
    /// working file. If the backup itself fails the save is skipped instead — overwriting a file
    /// that could not be preserved is the one outcome this exists to prevent. Either way the log
    /// says which happened. On success `origin` becomes [`File`](ConfigOrigin::File), so a second
    /// save does not back up the file this one wrote.
    pub fn save_over(&self, paths: &AppPaths, origin: &mut ConfigOrigin) -> std::io::Result<()> {
        let path = paths.config_file();
        if *origin == ConfigOrigin::Unusable && path.exists() {
            match crate::durable::move_aside(&path, "bad") {
                Ok(backup) => tracing::warn!(
                    "the settings file that could not be used is kept as {}; writing current \
                     settings to {}",
                    backup.display(),
                    path.display()
                ),
                Err(e) => {
                    tracing::error!(
                        "{} could not be backed up ({e}); not saving settings, so it is left as \
                         it is",
                        path.display()
                    );
                    return Err(e);
                }
            }
        }
        self.save(paths)?;
        *origin = ConfigOrigin::File;
        Ok(())
    }

    /// Bring hand-edited values into range.
    ///
    /// A volume of 40 or a rewind depth of 0 is a plausible typo, and clamping is friendlier than
    /// either refusing the file or blowing out the user's speakers.
    pub fn clamped(mut self) -> Self {
        self.audio.volume = self.audio.volume.clamp(0.0, 1.0);
        if !self.audio.volume.is_finite() {
            self.audio.volume = AudioConfig::default().volume;
        }
        self.rewind.seconds = self.rewind.seconds.clamp(1, 600);
        self.rewind.interval_frames = self.rewind.interval_frames.clamp(1, 120);
        self.video.dual_screen_gap = self.video.dual_screen_gap.min(64);
        if !self.emulation.fast_forward_speed.is_finite() || self.emulation.fast_forward_speed < 0.0
        {
            self.emulation.fast_forward_speed = EmulationConfig::default().fast_forward_speed;
        }
        self.emulation.fast_forward_speed = self.emulation.fast_forward_speed.min(64.0);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let config = Config::default();
        let text = toml::to_string_pretty(&config).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn keybinds_serialise_by_name_so_the_file_is_readable() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(text.contains("button_up"), "unreadable keybinds:\n{text}");
        assert!(text.contains("fast_forward"));
    }

    #[test]
    fn a_partial_file_keeps_defaults_for_everything_it_omits() {
        let config: Config = toml::from_str("[audio]\nvolume = 0.25\n").unwrap();
        assert_eq!(config.audio.volume, 0.25);
        assert!(!config.audio.muted);
        assert_eq!(config.video.scaling, ScalingMode::Nearest);
        assert_eq!(config.keybinds, KeybindMap::defaults());
    }

    #[test]
    fn an_empty_file_is_the_default_config() {
        assert_eq!(toml::from_str::<Config>("").unwrap(), Config::default());
    }

    #[test]
    fn out_of_range_values_are_clamped_rather_than_refused() {
        let config: Config =
            toml::from_str("[audio]\nvolume = 40.0\n[rewind]\nseconds = 0\ninterval_frames = 0\n")
                .unwrap();
        let config = config.clamped();
        assert_eq!(config.audio.volume, 1.0);
        assert_eq!(config.rewind.seconds, 1);
        assert_eq!(config.rewind.interval_frames, 1);
    }

    #[test]
    fn a_broken_file_yields_defaults_and_is_not_overwritten_by_loading() {
        let dir = std::env::temp_dir().join(format!("alpha-config-{}", std::process::id()));
        let paths = AppPaths::rooted_at(&dir);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let broken = "this is not toml = = =";
        std::fs::write(paths.config_file(), broken).unwrap();

        let (config, origin) = Config::load(&paths);
        assert_eq!(config, Config::default());
        assert_eq!(origin, ConfigOrigin::Unusable);
        assert_eq!(
            std::fs::read_to_string(paths.config_file()).unwrap(),
            broken,
            "the user's broken file must survive so they can fix it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_survives_a_shutdown_save_as_a_backup() {
        let dir = std::env::temp_dir().join(format!("alpha-config-bad-{}", std::process::id()));
        let paths = AppPaths::rooted_at(&dir);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let broken = "[audio]\nvolume = 0.4\n[[[ a typo";
        std::fs::write(paths.config_file(), broken).unwrap();

        let (mut config, mut origin) = Config::load(&paths);
        config.audio.volume = 0.6; // changed during the session
        config.save_over(&paths, &mut origin).unwrap();

        let backup = paths.config_file().with_file_name("config.toml.bad");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), broken);
        assert_eq!(Config::load(&paths), (config.clone(), ConfigOrigin::File));
        assert_eq!(origin, ConfigOrigin::File);

        // A second save writes over the good file it just made, and backs up nothing more.
        config.save_over(&paths, &mut origin).unwrap();
        assert!(!backup.with_file_name("config.toml.bad.1").exists());
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), broken);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_good_or_absent_file_is_saved_without_a_backup() {
        let dir = std::env::temp_dir().join(format!("alpha-config-ok-{}", std::process::id()));
        let paths = AppPaths::rooted_at(&dir);
        let (config, mut origin) = Config::load(&paths);
        assert_eq!(origin, ConfigOrigin::Absent);
        config.save_over(&paths, &mut origin).unwrap();
        config.save_over(&paths, &mut origin).unwrap();
        assert!(!paths
            .config_file()
            .with_file_name("config.toml.bad")
            .exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_saved_config_loads_back_identically() {
        let dir = std::env::temp_dir().join(format!("alpha-config-rt-{}", std::process::id()));
        let paths = AppPaths::rooted_at(&dir);
        let mut config = Config::default();
        config.audio.volume = 0.31;
        config.video.scaling = ScalingMode::IntegerNearest;
        config.keybinds.rebind(
            crate::input::PhysicalKey::Z,
            crate::input::Action::Button(core_common::Buttons::A),
        );

        config.save(&paths).unwrap();
        assert_eq!(Config::load(&paths).0, config);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewind_capacity_covers_the_requested_seconds() {
        let rewind = RewindConfig {
            enabled: true,
            seconds: 30,
            interval_frames: 6,
        };
        // 30 s at 59.7275 Hz is 1791.8 frames; every 6th is 299 snapshots.
        assert_eq!(rewind.snapshot_capacity(59.7275), 299);
        assert!(
            rewind.snapshot_capacity(59.7275) as f64 * 6.0 / 59.7275 >= 30.0,
            "capacity must reach the requested depth, not fall just short of it"
        );
    }

    #[test]
    fn rewind_disabled_needs_no_snapshots() {
        let rewind = RewindConfig {
            enabled: false,
            ..Default::default()
        };
        assert_eq!(rewind.snapshot_capacity(59.7275), 0);
    }
}
