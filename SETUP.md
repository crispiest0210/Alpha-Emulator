# Setup

Getting from a fresh clone to playing a cartridge. Three commands on most machines.

## What you need

- **A Rust toolchain**, installed with [rustup](https://rustup.rs). Nothing else — the exact
  compiler version is pinned in `rust-toolchain.toml` and rustup picks it up on its own. Do not
  install a specific version by hand and do not override it.
- **On Linux only**, two sets of development headers. See [Linux](#linux) below; `cargo xtask setup`
  will tell you exactly which and print the command.
- **A ROM file you own.** None are included here and none ever will be — see
  [Where ROMs come from](#where-roms-come-from).

## Get playing

```sh
git clone <this repository>
cd Alpha-Emulator

cargo xtask setup                          # checks your machine, prints anything missing
cargo xtask dev -- ~/roms/mygame.gba       # build, then open that cartridge
```

`cargo xtask setup` prints one `ok` line per requirement and finishes with
`All required dependencies present.` If something is missing it prints the exact `apt`, `dnf`, or
`pacman` command for your system and stops. It never downloads anything into the repository.

It also lists a few optional developer tools as `absent`. Those are for working on the emulator,
not for playing — you can ignore them.

**The first build takes a few minutes** — it is compiling a GPU stack and an audio stack from
source. Later builds take seconds.

Leave off the ROM path to open the application with no cartridge:

```sh
cargo xtask dev
```

Then drag a ROM onto the window, or paste its path into the library panel's import box. Either way
it is added to your library and starts playing.

## Which files work

| Extension | System | State |
|---|---|---|
| `.gb` | Game Boy | Fully playable |
| `.gbc` | Game Boy Color | Fully playable |
| `.gba` | Game Boy Advance | Playable — with two gaps worth knowing about |
| `.nds` | Nintendo DS | **Partial** — see below |

Game Boy Advance games play at full speed with picture, sound, input, and saves. Two things are
missing. The GBA can make sound two ways and only one is wired, so **part of the mix is missing**
in a game that uses the older four-channel hardware alongside the main one. And there is **no
cartridge clock**, so a game with a real-time clock will say its internal battery has run dry and
carry on — which is exactly what a real cartridge with a flat battery does, so nothing is broken,
but time-of-day events never happen.

Nintendo DS support is real but newer: games boot, both screens draw in 2D and 3D, sound plays, and
saves work. It is held to a lower accuracy bar than the other three and some games will misbehave.
`README.md` lists exactly what is and is not implemented.

## Controls

| Key | Action |
|---|---|
| `W` `A` `S` `D` | D-pad |
| `Space` / `R` | A / B |
| `T` / `G` / `Q` / `E` | X / Y / L / R (GBA and DS) |
| `Enter` / `Left Shift` | Start / Select |
| `P` | Pause |
| `Tab` (held) | Fast-forward |
| `Backspace` (held) | Rewind |
| `F1` | Show the HUD |
| `F2` / `F3` | Quicksave / quickload |
| `F9` | Debugger panel |
| `F11` / `F12` | Fullscreen / screenshot |
| `Escape` | Reset |

All rebindable in the **Keys** panel. Bindings follow physical key *positions*, so a non-QWERTY
layout puts them under the same fingers rather than under the same letters.

On the DS, the bottom screen is the touchscreen — click and drag on it with the mouse.

## Where your files go

Saves, save states, settings, and the library index are written to the standard per-user
application-data directory for your OS. The exact paths are printed in the first two lines of
output when the application starts.

To keep everything somewhere else — useful for trying a build without touching your real library:

```sh
cargo run -p frontend-native -- --data-dir /tmp/scratch ~/roms/mygame.gba
```

Settings and keybindings are plain TOML and safe to edit by hand.

## Per-OS notes

### macOS

Nothing to install. The frameworks the window, GPU, and audio need ship with the OS. Xcode Command
Line Tools are needed for the linker; rustup prompts for them if they are missing.

### Windows

Nothing to install beyond the MSVC toolchain, which rustup sets up as part of the default
`x86_64-pc-windows-msvc` target. No MSYS2 and no vcpkg — the graphics and audio backends bind to
system DLLs directly.

### Linux

Two groups of development headers, because two crates link against system libraries:

| What | Why |
|---|---|
| ALSA development headers | audio output |
| X11 **or** Wayland client headers | creating the window |

`pkg-config` is required too — that is how the build finds both.

```sh
# Debian / Ubuntu
sudo apt install libasound2-dev pkg-config
sudo apt install libx11-dev libxkbcommon-dev libwayland-dev

# Fedora / RHEL
sudo dnf install alsa-lib-devel pkgconf-pkg-config
sudo dnf install libX11-devel libxkbcommon-devel wayland-devel

# Arch
sudo pacman -S alsa-lib pkgconf
sudo pacman -S libx11 libxkbcommon wayland
```

X11 **or** Wayland is enough. `winit` uses whichever your session is running.

These are the same commands `cargo xtask setup` prints and the same packages CI installs. If you
change one, change all three: `xtask/src/main.rs`, this file, and `.github/workflows/ci.yml`.

## Where ROMs come from

**Yours.** Dump the cartridges you own, or use freely-licensed homebrew. This repository contains no
commercial ROM, references none, and fetches none, and that is a hard constraint rather than a
current state of affairs.

### Boot ROMs and BIOS images

You do not need one. Every system here starts from the documented post-boot register and memory
state, worked out from community hardware research rather than from any copyrighted image. Supply a
real boot ROM and it will be used instead; leave it out and everything still runs.

**None is included here and none will be** — their licensing status is not something this project is
willing to assume is clear.

## When something goes wrong

**`cargo xtask setup` says a package is missing that I have installed.**
It asks `pkg-config`, so the *development* package — headers and a `.pc` file — has to be installed,
not just the runtime library. Debian-family distributions suffix those `-dev`; Fedora uses `-devel`.

**The build fails linking ALSA on a machine with no sound card.**
The headers are needed regardless, because the audio backend is compiled in either way. Install
`libasound2-dev` or your distribution's equivalent; you do not need working audio hardware.

**macOS or Windows warns that the application is unsigned.**
Expected. Nothing here is code-signed or notarised. It is not a sign the build is damaged.

**A game runs but looks or sounds wrong.**
Check `README.md`'s status tables first — several behaviours are knowingly unimplemented and each is
listed there rather than left for you to discover. Nintendo DS games in particular are held to a
lower bar than the rest.

**Something else.**
`cargo xtask lint` runs exactly what CI runs. A green local lint with a red CI job means the
difference is environmental rather than in your change.

## For contributors

Everything above is enough to play. To work on the emulator:

```sh
cargo xtask test              # unit tests
cargo xtask lint              # exactly what CI runs — do this before opening a PR
cargo xtask fetch-test-roms   # the accuracy corpus; never committed, gitignored
cargo xtask test --accuracy   # the accuracy suite, once the corpus is fetched
cargo xtask bench             # performance measurements
```

Without the fetch, the accuracy tests skip cleanly rather than failing, so a plain
`cargo xtask test` works on a fresh clone with no network. The fetch itself needs `curl`, and
`unzip` for the one ROM its upstream publishes only inside a release archive; both ship with macOS
and every Linux distribution worth supporting.

Read `CONTRIBUTING.md` for the conventions and `AGENTS.md` for the architecture and the reasoning
behind it.
