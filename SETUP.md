# Setup

## The short version

```sh
cargo xtask setup
```

That checks your host for everything the build needs and, if something is missing, prints the
exact install command for your package manager and exits non-zero. It never downloads or
vendors a binary into the repository — a deliberate constraint, so nothing in `target/` or the
working tree can ever depend on a library that arrived outside your package manager.

If it prints nothing but a success line, you are ready:

```sh
cargo xtask dev
```

## Toolchain

A Rust toolchain via [rustup](https://rustup.rs). The version is pinned in
`rust-toolchain.toml` and rustup selects it automatically — you do not need to install a
specific version by hand, and you should not override it.

Nothing else is required on any platform beyond the per-OS notes below.

## Per-OS

### macOS

Nothing to install. The system frameworks that `cpal` (audio) and `wgpu`/`winit` (window and
GPU) need ship with the OS. Xcode Command Line Tools are needed for the linker, and rustup
prompts for them if they are missing.

### Windows

Nothing to install beyond the MSVC toolchain, which rustup installs as part of the default
`x86_64-pc-windows-msvc` target. No MSYS2 or vcpkg step — the graphics and audio backends bind
to system DLLs directly.

### Linux

Two groups of development headers, because two crates link against system libraries:

| What | Why |
|---|---|
| ALSA development headers | `cpal` opens the audio device through ALSA |
| X11 or Wayland client headers | `winit` creates the window |

`pkg-config` itself is required — that is how the build locates both.

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

X11 **or** Wayland is enough — `cargo xtask setup` accepts either, and `winit` picks whichever
your session is running.

These are the same commands `cargo xtask setup` prints, and the same packages CI installs. If
you change one, change all three: `xtask/src/main.rs`, this file, and
`.github/workflows/ci.yml`.

## Test ROMs

The accuracy suite runs against test ROMs that are **fetched, never committed**:

```sh
cargo xtask fetch-test-roms
cargo xtask test --accuracy
```

`testing/test-roms/` is gitignored and nothing in it is ever committed. Without the fetch the
accuracy tests skip cleanly rather than failing, so a normal `cargo xtask test` works on a
fresh clone with no network.

No commercial ROM is vendored, referenced, or fetched by anything in this repository, under any
circumstance.

## Boot ROMs

Every system runs without one. Supply a real boot ROM and it will be used; omit it and the
emulator jumps straight to the documented post-boot register and memory state, which is derived
from community hardware research rather than from any copyrighted image.

**No boot ROM is vendored in this repository**, and none will be — their licensing status is
not something this project assumes is clear.

## Troubleshooting

**`cargo xtask setup` says a package is missing that I have installed.**
It asks `pkg-config`, so the *development* package (the one with headers and a `.pc` file) has
to be installed, not just the runtime library. On Debian-family distributions that is the
`-dev` suffix; on Fedora, `-devel`.

**The build fails linking ALSA on a headless Linux box.**
The headers are needed even if you never play audio, because `cpal` is compiled in. Install
`libasound2-dev` (or your distribution's equivalent) — you do not need a sound card.

**Something else.**
`cargo xtask lint` runs exactly what CI runs, so a green local lint and a red CI job means the
difference is environmental rather than in your change.
