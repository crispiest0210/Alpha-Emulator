# Alpha Emulator

A from-scratch, multi-generation Nintendo handheld emulator written in Rust, targeting the
Game Boy, Game Boy Color, Game Boy Advance, and Nintendo DS.

It is a clean-room implementation: the emulation core is original Rust, with no vendored
third-party emulator embedded as a black box, and no browser engine in the rendering path.
Rendering is native GPU (`wgpu` + `winit`), audio is `cpal` fed from a lock-free ring buffer,
and the emulation core is a pure library with zero UI-framework dependency.

## Quick start

```sh
cargo xtask setup                       # checks your machine; on Linux it names two packages to install
cargo xtask dev -- path/to/rom.gba      # build, then play
```

That is the whole thing on macOS and Windows. The first build takes a few minutes. Bring your own
ROM — none are included here and none ever will be. **[SETUP.md](SETUP.md)** has the full guide:
controls, per-OS packages, where saves go, and what to do when something goes wrong.

## Status

There are no released builds yet, but all four systems run. The table below reflects what is
**actually implemented and tested**, not what is planned, and is updated as work lands.

| System | Boots | Playable | Accuracy suite | Notes |
|---|---|---|---|---|
| Game Boy (DMG) | ✅ | ✅ | ⚠️ | Plays in the window with sound and input, measured at 100% speed. Passes all 11 Blargg `cpu_instrs` sub-tests, `instr_timing`, `mem_timing`, `dmg-acid2` pixel-exact, and 9 of 12 `dmg_sound` sub-tests — see below |
| Game Boy Color | ✅ | ✅ | ⚠️ | Plays. 11 of 12 Blargg `cgb_sound` sub-tests pass; `cgb-acid2` is pixel-exact against its reference |
| Game Boy Advance | ✅ | ✅ | ⚠️ | Passes `gba-suite`'s ARM, Thumb, memory, and `save/none` ROMs. **A commercial game plays in the window at a measured 100% speed** — zero dropped frames, zero dropped audio samples — with backgrounds, sprites, affine effects, and colour blending. BIOS calls work in both instruction sets, decompressors included. `bios` and three of the four `save` ROMs fail, each a specific diagnosed gap — see below. **No PSG mixing, so music is often silent**; no cartridge clock, mosaic, or EEPROM |
| Nintendo DS | ✅ | ⚠️ | ❌ | **Partial, and deliberately so.** Boots a `.nds` ROM, runs both CPUs, draws both screens in 2D and 3D, plays its sixteen sound channels, and keeps saves. Held to a lower accuracy bar than the other three; expect some games to misbehave. See below for exactly what is missing |

**All four systems boot with picture and sound, and three of them play commercial games at full
speed.** The Game Boy and Game Boy Color are the two held to a full accuracy bar; see the notes
below each of the other two. The application has a
ROM library, video, audio, keyboard and touchscreen input, quicksave and quickload, rewind, an HUD,
a rebindable keymap, screenshots, and an in-app debugger with registers, disassembly, a memory
viewer, breakpoints, and watchpoints. For the GBA, the debugger also has a PPU panel — layer
isolation, palette and tile viewers, an OAM table, and decoded video registers.

### Where the Game Boy Advance actually is

`gba-suite`'s ARM, Thumb, and memory ROMs pass, and a commercial game **plays in the window at a
measured 100% speed** — 59.7 fps sustained, zero frames dropped to the drawing thread, zero audio
samples dropped. Backgrounds in every mode, sprites at both colour depths, affine scaling and
rotation, windows, colour blending, save states and rewind all work on real software rather than
only on unit tests.

`gba-suite`'s `bios` and `save` ROMs are also in the corpus now, and they found two real, specific
gaps rather than passing cleanly — see "Open gaps" below.

Two gaps are worth stating plainly because you will notice both:

- **PSG mixing is not wired.** The GBA makes sound two ways — two DMA-fed direct-sound channels and
  the Game Boy's four older PSG channels — and only direct sound is connected. A game that drives
  sound through the PSG channels loses that part of its mix. Direct sound itself now works: until
  2026-08-10 it produced **exact digital silence**, because `DirectSound::owns` claimed both FIFO
  addresses and `write16` answered `None` for them, so every byte a DMA channel delivered was
  accepted by the bus and dropped. Nothing caught it because nothing checked that a sample put
  into a FIFO comes back out.
- **There is no cartridge GPIO**, so a game with a real-time clock finds none and reports a flat
  battery. That is the accurate outcome rather than a failure — a real cartridge with a dead
  battery behaves identically and games handle it — but time-of-day events never fire.

Mosaic and EEPROM saves are absent. In-game saving, quicksave, and save states are confirmed
working by play on a commercial title, and a real cartridge's chip and size are detected
correctly. `gba-suite`'s `save` ROMs (added to the corpus alongside `bios`) turned "unverified"
into two specific findings instead: fresh SRAM reads `0x00` rather than the `0xFF` a real chip
reads before anything is written, and a 16- or 32-bit CPU access to the save window reads two
independent bytes rather than mirroring the single addressed byte across every lane, as an 8-bit
device must. Both are diagnosed in `testing/corpus/src/lib.rs` against the exact code that needs
to change. `save/none` and the ARM/Thumb/memory suites still pass.

#### The bug worth reading about before touching timing anywhere

Getting here took four fixes. Three were ordinary: the BIOS `SWI` interception ignored Thumb, which
is what almost every commercial game is compiled to; the interrupt HLE skipped the BIOS's return
wrapper, so the machine took exactly one interrupt and then none; and eleven BIOS calls were
missing, including all five decompressors.

The fourth is the one with a lesson in it. **Every memory access was charged between three and six
times over** — an ARM instruction in internal WRAM cost 13 cycles where hardware charges 1, and 49
from the cartridge where it charges 6.

Nothing failed. Every test passed, and the emulator reported a steady 100% speed the whole time,
because *a frame is a fixed number of cycles however few instructions fit inside it*. The emulator
was not slow; the emulated machine was. What a game lost was about nine tenths of its processor,
and what that looked like was a frozen picture with the CPU visibly running — which reads as a
hang, and was diagnosed as one twice before a profiler was pointed at it.

Three causes compounded, all now fixed and pinned by
`an_instruction_costs_what_the_hardware_charges_for_it`:

- The `SWI` check fetched every opcode through the bus before the CPU fetched the same word itself.
  It peeks now, through a path with no side effects.
- `read32` charged the wait-state table, then charged it again in each `read16` and each `read8` it
  decomposed into. Charging happens once, in the `Bus` method the CPU called, at the width the CPU
  asked for; routing is split from timing so the decomposition cannot re-enter the accounting.
- The wait-state table reports an access's whole cost including its first cycle, and the CPU core's
  S/N/I total already counted that cycle. Only the waiting is charged now.

Three rendering defects surfaced afterwards, all of which had produced a complete and plausible
*wrong* picture rather than a missing one — the failure mode this project treats as the dangerous
one:

- **Colour index 0 in a text background was drawn as a colour instead of being transparent.** On
  this machine a background is one of four *layers*, and index 0 lets whatever is behind show
  through. Writing it made the frontmost enabled text layer opaque across the whole screen, hiding
  every layer behind it and the backdrop under flat bands of one palette colour — worst on menus,
  text boxes, and anything mid-transition, where the front layer is mostly empty. The Game Boy's
  rule is the opposite and equally correct, because its background is the bottom of the picture and
  sprite priority is decided by comparing against index 0, so this is a parameter rather than a
  fix: `BackgroundParams::transparent_index_zero`.

- **Alpha blending used the backdrop as its lower layer**, because the scanline buffer keeps only
  the winning pixel. Any game blending a layer over artwork had that artwork mixed with black. The
  lower layer is now composed as a second pass with the first-target layers left out, and a blend
  happens only where the pixel underneath belongs to a declared second target, as hardware requires.
- **Sprite-versus-background priority was the Game Boy's rule, not this machine's.** A GBA sprite
  carries a priority in OAM and each background carries one in `BGxCNT`, and the sprite is in front
  where its own is less than or equal to the background's. Instead the Game Boy's single
  "behind background" bit was consulted — which the GBA decoder always leaves false — so every
  sprite won against every background. Characters walked *over* the text boxes in front of them.
  The GBA text tilemap also recorded a hard-coded priority of zero on every pixel, so there was
  nothing to compare against even once the sprite's was passed through.
- **A background larger than 32x32 tiles wrapped at half its size.** `BackgroundParams::full_line`
  describes a 32x32 map and the text renderer wraps on the size it is handed, so a layer left at
  that default never reached its second screen block. Emerald's battle menu lives in exactly that
  block, on a 32x64 background scrolled to 320 — so the bottom fifty scanlines read an empty block
  and came out as backdrop.
- **The object window was reported as never covering.** A sprite whose graphics mode is
  `ObjectWindow` draws nothing; its *shape* is a window region, and `WINOUT`'s high byte says what
  is visible inside it. Answering "never" is not a neutral default — a game that reveals content
  *through* one gets a blank region instead. Pokémon Emerald's battle screen puts the action menu
  and message box there, so the bottom fifty scanlines came out as pure backdrop.
- **Bit 5 of the window registers was treated as a layer.** It is not: it says whether colour
  special effects apply inside that region at all, and it shares a bit position with the backdrop
  target in `BLDCNT` — the same bit meaning two things in two register sets. A game that darkens
  the world behind a menu switches the effect off inside the menu's window; ignoring that darkened
  the menu too, so its panels came out grey instead of white.
- **Every sprite was decoded as 16-colour.** Depth is per sprite on this hardware and one scanline
  can hold both, so it now rides on the sprite rather than on the call. A 256-colour sprite read as
  16-colour comes out as a stretched checkerboard.

#### The tools

Reach for these before reasoning about pixels; each of them beat that approach by an order of
magnitude on this system.

```sh
TRACE_ROM=<rom> cargo test -p system-gba --release -- --ignored --nocapture dump_state
TRACE_ROM=<rom> TRACE_FRAMES=600 cargo test -p system-gba --release -- --ignored --nocapture trace_stall
```

`dump_state` prints the program counter, `DISPCNT`, the interrupt registers, the handler pointer,
and a per-layer graphics decode at seven points in a run. `trace_stall` answers the question after
it — for a machine executing happily and getting nowhere — by profiling a window of instructions:
hottest addresses disassembled in the instruction set they were *fetched* in, a breakdown by 4 KiB
page, how many steps went to a halted CPU, and the average cycle cost of each instruction. The page
breakdown said a game's main loop was running seven times where it should run a hundred and sixty;
the cycle column said why.

### What the Nintendo DS does and does not do

DS support is real but newer, and is the start of DS emulation here rather than the end of it. The
bar it was built to is "runs good homebrew and simple commercial titles correctly", not parity with
hardware.

**Implemented:**

- **Both CPUs**, interleaved deterministically on one thread, with the runtime-configurable
  shared-WRAM split and two genuinely different views of memory.
- **All nine VRAM banks** and every mapping their control registers can select, including two banks
  claiming one address at once.
- **Both 2D engines** — background modes 0-5 (text, affine, and all three extended types), sprites
  at 4 and 8 bits per pixel in both mapping arrangements with flips, rotation, scaling, semi-
  transparency, bitmap and object-window modes, extended palettes, windows, alpha blending,
  brightness effects, and master brightness.
- **The 3D core** — the full geometry command set, four matrix stacks, four-light vertex lighting,
  clipping against all six frustum planes, a perspective-correct software rasteriser with a 24-bit
  depth buffer, all seven texture formats including 4x4 compression, and compositing into the 2D
  layers through the ordinary blend unit.
- **Sixteen-channel sound** — PCM8, PCM16, IMA-ADPCM, six square-wave channels and two noise
  channels, with per-channel rate, volume, panning, and loop modes.
- **The cartridge save chip.** Which chip a cartridge has is not in its header, so it is worked out
  from how the game talks to it — and nothing is written to disk until that is certain, because a
  save file of the wrong shape is worse than none.
- **Input**: the keypad, the two extra buttons only the ARM7 can see, and the touchscreen.
- IPC (both FIFOs and `IPCSYNC`), both interrupt controllers, eight timers, eight DMA channels, the
  cartridge transfer interface, direct boot, and save states covering all of it.

**Not implemented, and visibly so rather than approximated:**

- **3D refinements**, ranked below getting the geometry and textures right: fog, edge marking,
  anti-aliasing, shadow polygons (which render as ordinary geometry), and the toon and highlight
  tables (which fall back to plain texturing). `BOX_TEST` always answers "visible" — answering
  "hidden" wrongly makes geometry disappear with no way to recover it, while answering "visible"
  wrongly only costs a little work. The geometry command queue is reported as never full, because
  real hardware stalls the CPU when it fills and nothing in this codebase's bus interface can
  express a bus that blocks a CPU part-way through an instruction.
- **Wifi**, and it is not planned. Its register block reads as open bus, which is what a DS with no
  card present looks like.
- Mosaic, mode 6's large bitmap, display mode 3 (main-memory display), KEY1 cartridge encryption,
  and the per-line sprite budget.

Component status:

| Component | Status |
|---|---|
| Workspace, `xtask`, CI, crate-boundary enforcement | done |
| `core-common` — scheduler, bus, CPU/system traits | done |
| `savestate` — versioned format, `Savable`, rewind ring buffer | done; rewind verified against a running machine |
| `cpu-sm83` — Game Boy CPU | complete; passes all Blargg CPU and timing suites |
| `cpu-arm7tdmi` — GBA / DS ARM7 CPU | complete; passes `gba-suite`'s ARM and Thumb instruction ROMs |
| `cpu-arm946e` — DS ARM9 CPU (ARMv5TE, CP15, TCM) | complete, unit-tested; **accuracy ROMs not yet run** |
| `cart-common` — headers, MBC1/2/3/5, SRAM/Flash/EEPROM, RTCs | done for GB and GBA save chips |
| `system-gb` memory map | done (WRAM/VRAM banking, echo RAM, boot ROM) |
| `system-gb` timing — timer, PPU mode machine, APU sequencer | done as scheduled events; **Mooneye timer ROMs not yet run** |
| `ppu-tile2d` — tile decode, palettes, scanline compositing | done for GB/GBC/GBA formats; both sprite-priority rules, both tile-mapping arrangements, and per-sprite bit depth |
| `system-gb` PPU — background, window, sprites | done, scanline-accurate; dmg-acid2 validated pixel-exact |
| `apu-shared` — square/wave/noise channels, envelope, sweep, mixer | done; 9 of 12 Blargg `dmg_sound` sub-tests pass |
| `frontend-core` audio pipeline — lock-free ring, resampler | done and bound to a `cpal` device; fast-forward is pitch-shifted rather than dropped |
| `system-gb` APU — NR10-NR52 register layer | done; DMG wave-RAM window is machine-cycle accurate, not t-cycle |
| `frontend-core` input — keybinds, conflict rule, delivery | done and driven from the window; keyboard only, gamepads are future work |
| `system-gb` assembly — `System` impl, joypad, OAM DMA, boot | done; save-state round-trip is frame-exact |
| `system-gb` CGB blocks — palette RAM, `KEY1`, VRAM DMA, tile attributes | done and driven by the bus |
| `system-gbc` — `System` impl, model selection, compatibility boot | done; 11 of 12 `cgb_sound` sub-tests pass, cgb-acid2 validated pixel-exact; **`OPRI` not modelled** |
| `testing/harness` — accuracy runner, fetch automation | done; drives the GB suite end to end |
| `frontend-headless` — CLI driver, framebuffer hashing, determinism check | done for the Game Boy family |
| `library` — SQLite index, watched folders, reconciliation | done; a moved file is recognised by content hash and keeps its row |
| `frontend-core` session — emulation thread, commands/events, frame pipe, rewind, save-RAM flush | done; lifecycle driven end to end by tests against a real thread |
| `frontend-core` settings — TOML config, keybinds, presentation, rewind depth | done; a malformed file falls back to defaults and is left on disk |
| `frontend-native` — window, `wgpu` presentation, `egui` chrome, library browser, HUD, keybind editor | done; **no native file dialog — drag-and-drop or a pasted path** |
| `system-gba` memory map — regions, mirroring, open bus, 8-bit write quirk | done |
| `system-gba` interrupt controller, timers, 4-channel DMA | done and tested, and driven by real games; a sound-FIFO transfer's shape is fixed by hardware rather than read from the channel |
| `system-gba` video timing and bitmap modes 3/4/5 | done and tested, and driven by real games |
| `system-gba` text backgrounds — four layers, map decode, draw order | done and tested, and driven by real games |
| `system-gba` sprites — OAM decode, sizes, per-line selection, matrices | done and tested; 16- and 256-colour, with the depth carried per sprite because one line can hold both |
| `system-gba` affine transform — backgrounds and sprites | done and tested, and driven by real games |
| `system-gba` direct sound — two DMA-fed FIFO channels | done and tested, and audible — the FIFO write path was missing entirely until 2026-08-10; **PSG mixing still not wired** |
| `system-gba` wait states — `WAITCNT`, per-region access cost | done; charged once per access, and only for the cycles the access waited beyond the one the CPU core already counts |
| `system-gba` compositor — layers, priority, palette, sprites, affine | text, bitmap, and affine backgrounds at every map size, sprites at both depths with priority resolved against the backgrounds, affine sprites through their matrices, and index 0 treated as transparent |
| `system-gba` keypad — `KEYINPUT`, `KEYCNT`, combination interrupt | done and driven |
| `system-gba` windows and colour blending | both rectangular windows and the object window; alpha blending composes the real lower layer in a second pass and only blends where that layer is a declared second target; the window colour-effect enable is honoured |
| `system-gba` cartridge — three ROM windows, SRAM/Flash detection | done; a real cartridge's chip and size are detected from how the game talks to it, and in-game saving is confirmed working by play. **EEPROM reported absent rather than emulated** |
| `system-gba` assembly — `System` impl, bus routing, HLE interrupt entry | done; runs a ROM headlessly |
| `system-gba` HLE BIOS — arithmetic, `CpuSet`, the waiting calls, `RegisterRamReset`, the affine setters, and all five decompressors (LZ77, RLE, Huffman, and both difference filters) | done in **both** ARM and Thumb; an unhandled call still changes nothing but now says so in the log |
| `system-gba` HLE interrupt wrapper — register save, handler call, `subs pc, lr, #4` return | done; the return is what puts `CPSR` back, and without it a machine takes exactly one interrupt |
| `debugger` — breakpoints, watchpoints, conditions | done; execution breakpoints and watchpoints both halt a running machine; **no GDB server or tracing yet** |
| `debugger` — snapshot capture (registers, disassembly, memory) | done against `DebugTarget`, so no branch per system |
| `core-common` — `DebugTarget`, `System::step_instruction`, `AccessLog` | done; all four systems implement introspection and access recording |
| `system-nds` memory map — two views over one store, shared-WRAM split, open bus per core | done and tested |
| `system-nds` VRAM — nine banks, every `VRAMCNT` mapping, overlap, precomputed page table | done and tested |
| `system-nds` IPC — `IPCSYNC`, both FIFOs, edge-triggered interrupts | done and tested; driven end to end by two real programs on the two cores |
| `system-nds` interrupt controllers, timers, DMA | done and tested; per-core source masks, 21-bit ARM9 DMA counts |
| `system-nds` video timing — 263 lines, two `DISPSTAT`s, one `VCOUNT` | done and tested |
| `system-nds` 2D engines — modes 0-5, sprites, windows, blending, master brightness | done and tested; **mosaic, mode 6, and display mode 3 not implemented** |
| `system-nds` input — keypad, `EXTKEYIN`, touchscreen over SPI | done and tested; firmware and power-management SPI devices are stubs |
| `system-nds` cartridge — header, direct boot, card transfers | done and tested; **no KEY1 encryption** |
| `system-nds` save chip — EEPROM and FLASH over auxiliary SPI, six sizes, type detection | done and tested; write timing is not modelled, and a cartridge that only ever writes partial pages cannot be identified |
| `system-nds` audio — sixteen channels, PCM8/PCM16/ADPCM/PSG/noise, panning, loop modes | done and tested; `SOUNDBIAS`, the output filter, and sound capture are not modelled |
| `system-nds` 3D matrices — four stacks, push/pop/store/restore, the clip matrix | done and tested |
| `system-nds` 3D geometry — command FIFO, vertex assembly, lighting, frustum clipping | done and tested; shininess table and `BOX_TEST` deferred |
| `system-nds` 3D rasteriser — perspective-correct spans, depth buffer, all seven texture formats | done and tested; **no fog, edge marking, anti-aliasing, shadow polygons, or toon table** |
| `system-nds` assembly — `System` impl, two bus views, dual-core frame loop | done; boots a ROM, draws both screens, and produces audio |
| `system-nds` debugger support — `DebugTarget`, access log, region list | done and tested; **the ARM9 only** — the ARM7 is not reachable from the debugger |
| `system-nds` diagnostics — VRAM bank map, per-layer decode, dual-core state dump | done and tested; side-effect free, so a dump can be taken from anywhere |
| `frontend-native` debugger panel | registers, disassembly with PC highlight and click-to-toggle breakpoints, hex viewer, read/write watchpoints, instruction stepping |
| `frontend-native` GBA PPU debugger panel | layer isolation (hide/solo BG0-3, OBJ, both windows), 256-entry palette viewers, character-block tile viewer at both bit depths, a 128-row OAM table with current-scanline highlighting, and decoded `DISPCNT`/`DISPSTAT`/`BGxCNT`/scroll/window/blend registers; layer toggles are a render-only override, excluded from save states |
| Everything else | not started |

### Accuracy suite

The harness is in place and the Game Boy suite runs. Fetch the ROMs with
`cargo xtask fetch-test-roms`, then `cargo xtask test --accuracy`. Nothing is vendored; the
corpus directory is gitignored and tests skip cleanly when it is empty.

Current Game Boy results:

| Test ROM | Result |
|---|---|
| Blargg `cpu_instrs`, all 11 sub-tests individually | **pass** |
| Blargg `instr_timing` | **passes** |
| Blargg `mem_timing` | **passes** |
| Blargg `dmg_sound` sub-tests 01–08, 11 | **pass** |
| Blargg `cgb_sound` sub-tests 01–08, 10–12 | **pass** |
| Blargg `cpu_instrs` (combined ROM) | hangs — see below |
| Blargg `dmg_sound` sub-tests 09, 10, 12 | fail — see below |
| Blargg `cgb_sound` sub-test 09 | fails — see below |
| `gba-suite` arm, thumb, memory | **pass** by register, but see below for `thumb` |
| `gba-suite` save/none | **pass** |
| `gba-suite` bios | fails — see below |
| `gba-suite` save/sram, save/flash64, save/flash128 | fail — see below |
| `gba-suite` ppu/hello, shades, stripes | render plausibly; unvalidated — see below |
| dmg-acid2, cgb-acid2 | **pass** — pixel-exact against the published reference images |

**Nintendo DS accuracy coverage is zero, and that is reported rather than papered over.** Prompt 13
asks for whatever test-ROM coverage exists at implementation time and for an explicit statement of
what is verified only by other means. Nothing DS-shaped is in the corpus: the community's DS test
ROMs are far fewer than the Game Boy family's, most target hardware this build does not model (3D,
wifi, the firmware), and several are distributed only as parts of emulator repositories rather than
as fetchable artifacts. What the DS *is* verified by instead:

- **339 unit tests in `system-nds`**, covering every module against the register behaviour it
  implements — including the VRAM bank table, both cores' interrupt source masks, the DMA start-
  timing decode that differs per core, and the IPC FIFO's edge-triggered interrupts.
- **End-to-end tests that assemble ARM by hand** and run it on the real machine: the ARM9 executing
  code direct boot loaded, the ARM7 writing where only it can see, the two cores exchanging a word
  through the FIFO with each side spinning on its own status flag, a vblank interrupt reaching a
  handler with no BIOS present, a program that maps a VRAM bank and puts a colour on screen, and an
  ARM7 program that starts a sound channel and is heard, and an ARM9 program that feeds a display
  list through `GXFIFO` and gets a triangle on the top screen.
- **A determinism test**: two machines given the same ROM and input agree byte for byte after four
  frames, which is prompt 13's dual-CPU constraint checked rather than asserted.
- **A save-state round trip** that is a fixed point and that continues identically from a restore.
- **Manual smoke test**: a hand-built homebrew that fills a VRAM framebuffer, run through
  `frontend-headless run --frames 5 --save-frame`, produces the expected gradient on the top screen
  and white on the bottom.

This is a lower bar than the Game Boy family's and is meant to read as one.

Open gaps, each tracked with the specific reason:

- **Combined `cpu_instrs`** executes `STOP` inside the runner it copies into work RAM. Not an
  instruction bug — all eleven sub-tests pass standalone and MBC1 bank reads are verified
  against that exact ROM by a dedicated test. `STOP` now releases correctly when a joypad line
  goes low, so this ROM reaches `STOP` for some earlier reason still to be found.
- **`dmg_sound` 09, 10, 12** exercise wave RAM while channel 3 is playing. The DMG access
  window *is* modelled — the CPU sees the byte the channel just fetched, and `0xFF` at any
  other time — but only to machine-cycle resolution, and these ROMs resolve it to single
  t-cycles. Closing them means stepping the APU finer than one machine cycle. Test 10 also
  needs the wave-RAM corruption a mid-playback trigger causes, which is not modelled at all.
- **`cgb_sound` 09** fails for exactly the reason its DMG counterpart does, and the other
  eleven pass. The two suites deliberately test *opposite* expectations for three behaviours —
  whether powering off clears the length counters, whether `NRx1` length writes land while the
  APU is off, and whether the CPU may reach wave RAM mid-playback — so each is gated on the
  model rather than fixed one way.
- **dmg-acid2 now passes**, compared pixel-for-pixel against its published reference: all 23 040
  matched, so the DMG background, window, sprite priority, and both tile-mapping arrangements are
  validated end to end rather than argued for. The hash is recorded in the corpus along with the
  commands to redo the comparison; the reference image is fetched, never committed, same rule as
  the ROMs.
- **cgb-acid2 now passes too**, which makes it the only end-to-end check of CGB tile attributes,
  the second VRAM bank, OBJ palettes, and CGB sprite priority. Getting there found two real bugs,
  both the same shape — a CGB read the DMG way, producing a complete and plausible wrong picture
  rather than an error. Sprite attributes were decoded with the DMG's rule, so every sprite drew
  through OBJ palette 0 with bank 0 tiles; and sprites were ordered by X coordinate, which is the
  DMG rule where a CGB orders by OAM index. Neither would have been caught by anything except a
  reference comparison.

Blargg's sound ROMs report through cartridge RAM rather than the serial port, and the message
they leave there names the exact rule that failed. `cargo test -p harness --release --
--ignored --nocapture dmg_sound_results` prints all twelve.

All are tracked as known failures in the corpus, so the suite stays green for *regressions*
while they are open — and fails loudly if one starts passing, which means the marker needs
removing.

**`gba-suite`'s ARM, Thumb, and memory ROMs pass** — the whole instruction set in both states, and
the memory suite. Between them they found two real bugs that no amount of unit testing would have
caught: the ARM7TDMI's legacy "P" form, and 32-bit writes to palette RAM and VRAM being decomposed
into bytes and so corrupted by the 16-bit bus quirk that applies to genuine byte writes.

**`bios` and three of the four `save` ROMs fail, and each is a specific, diagnosed finding, not a
mystery:**

- **`bios` sub-test 1.** This project's BIOS is fully HLE'd (`crates/system-gba/src/bios.rs`): an
  `swi` traps straight to native Rust that computes the answer, so no real BIOS instruction is
  ever fetched. The test reads the unmapped BIOS region right after startup and expects the last
  opcode a *real* BIOS left latched on the bus — a value that only exists if BIOS code actually
  ran. Sub-tests 2–4 check the same open-bus artifact after an `swi`, during an IRQ, and after an
  IRQ return, and are never reached because the suite exits at its first mismatch, but they fail
  for the same reason.
- **`save/sram` sub-test 1.** `Sram::new` (`crates/cart-common/src/save.rs`) deliberately
  zero-fills fresh save memory rather than the `0xFF` a real chip reads before anything has been
  written, so a fresh save is reproducible. The test reads a byte of untouched SRAM and requires
  `0xFF`.
- **`save/flash64` and `save/flash128` sub-test 4.** SRAM and Flash are 8-bit-wide devices, so a
  16- or 32-bit CPU access to the save window must read the single addressed byte mirrored into
  every byte lane. `GbaSystemBus::read16_routed` (`crates/system-gba/src/system.rs`) has no case
  for `Region::Sram` and falls through to the generic path, reading two independent bytes
  instead — the write side already gets this right, writing only the addressed byte. The test
  writes one byte and reads it back as a halfword, expecting it duplicated.
- **`save/none` passes**: with no save chip at all, a read of the save window correctly returns
  `0xFF`, which is a different code path from `Sram::new`'s.

All four are recorded as `expected_failure` in `testing/corpus/src/lib.rs`, so the suite stays
green for *regressions* while they are open and fails loudly if one starts passing.

### The GBA golden manifest

Until now, not one GBA frame in this repository had been compared against a reference —
`Convention::GbaSuite` checks a register, and every GBA rendering bug this project had found
(colour index 0 drawn opaque, the backwards alpha blend, Game-Boy-shaped sprite priority) was
found by looking at a picture, not by a test failing. `testing/golden/gba.toml`, run by
`testing/harness/src/golden.rs` as part of `cargo xtask test --accuracy`, is the mechanism that
replaces the looking: each case hashes a ROM's framebuffer at several frames — `[1, 2, 5, 30,
60]`, not just the last one, so a mismatch names *when* a run diverged — and a mismatch writes
the actual frame to `target/golden-fail/<name>-<frame>.png`, which CI uploads as an artifact on
failure.

**Building it found a bug immediately, before the mechanism was even finished.** `gba_suite_arm`
and `gba_suite_memory` render a byte-identical "All tests passed" picture, matched by eye against
the pass/fail report screenshot in jsmolka/gba-tests' own README (rendered there by eggvance) —
both are validated golden entries. `gba_suite_thumb` does not: its *rendered* report reads
"Failed test 229" from frame 2 onward, but `run_gba_suite`'s register check reads `r12=0` at
settle and reports a pass — so the table above still says thumb passes, because that convention's
own verdict says so, and `expected_failure` is checked against that verdict specifically (marking
it failed there would make the suite panic with "expected failure but passed"). The likely cause
is in `gba-suite`'s own failure-report routine (`lib/macros.inc`'s `m_test_eval`): it pushes
`r0-r12`, computes the failing test's digits via two BIOS `Div` calls and a `bl` to a text
routine, then restores `r0-r12` with a final `ldmfd` before idling — and something in that
sequence is not putting r12 back. Full diagnosis is in `testing/corpus/src/lib.rs`'s
`gba_suite_thumb` entry and `testing/golden/gba.toml`'s `gba_suite_arm` case; fixing it is
separate work from building the mechanism that found it.

The three jsmolka `ppu/` ROMs (`hello`, `shades`, `stripes`) are in the corpus now too, using
`Convention::Framebuffer`, and in the golden manifest as pending cases. Each renders exactly what
its own source says it should — readable "Hello world!" text, a sixteen-step blue gradient,
alternating stripes in the two configured colours — but `ppu/` ships no reference image the way
dmg-acid2 and cgb-acid2 do, and no independent GBA renderer was available to make one: mGBA 0.10.5
was installed for exactly that, but its Qt frontend has no scriptable headless screenshot path,
and driving its GUI would mean opening a window on someone's desktop for a background job to
control, which was judged worse than an honestly pending entry. `hashes = []` and a provenance
line saying precisely that is what's committed instead — a guess with authority is worse than an
admitted gap. See `testing/golden/gba.toml` for what was and was not checked on each.

The Game Boy Color's colour *rendering* is now validated against a reference — that is what
cgb-acid2 covers. Its speed switch and VRAM DMA are still checked against hardware documentation
and unit tests only, `OPRI` is not modelled at all, and the Mooneye CGB suite is not in the corpus
yet.

There is now a GBA to run them on, which there was not before.

## Installing

There are no published releases yet. When there is a tag, CI builds `alpha-emulator` (the windowed
application) and `alpha-headless` (the CLI driver) for Linux, macOS on both architectures, and
Windows, and attaches them to a draft GitHub release. Nothing is code-signed or notarised, so macOS
and Windows will warn on first run — expected for an unsigned build, not a sign the archive is wrong.

Until then, build it: see below.

## Setup

You need a Rust toolchain ([rustup](https://rustup.rs) — the pinned version is selected
automatically) and a ROM file you own. On Linux you also need two sets of development headers,
which `cargo xtask setup` names for you.

```sh
cargo xtask setup                       # checks your machine, prints anything missing
cargo xtask dev -- path/to/rom.gba      # build, then open that cartridge
cargo xtask dev                         # …or open with no cartridge and drag one in
```

The first build takes a few minutes; later ones take seconds. `cargo xtask setup` never downloads
or vendors a binary into the repository — if a system package is missing it prints the exact
`apt`/`dnf`/`pacman` command and exits non-zero.

**[SETUP.md](SETUP.md) is the full guide**: per-OS packages, controls, where your saves go, and what
to do when something goes wrong.

## Developer tasks

Everything goes through `xtask`, which is a Rust program and therefore behaves identically on
Linux, macOS, and Windows:

| Command | What it does |
|---|---|
| `cargo xtask setup` | Verify host toolchain and system packages |
| `cargo xtask dev` | Run the native frontend (`-- <rom>` to open a cartridge) |
| `cargo xtask build --release` | Build the workspace optimized |
| `cargo xtask test` | `cargo test --workspace` (`--accuracy` adds the test-ROM suite) |
| `cargo xtask bench` | Criterion benchmarks (`--quick`, `--filter`, `--save-baseline`, `--baseline`) |
| `cargo xtask profile <rom>` | Build the release driver and print the flamegraph command |
| `cargo xtask fetch-test-roms` | Download the accuracy test-ROM corpus (never committed) |
| `cargo xtask lint` | `rustfmt --check` + `clippy -D warnings`, exactly as CI runs them |

### What CI checks

`ci.yml` runs on every push and pull request: `rustfmt` and `clippy`, the crate-boundary rule via
`cargo deny`, unit tests and the full accuracy suite (including the GBA golden manifest) on Linux,
macOS, and Windows, `cargo doc` with warnings denied, a release-profile build of the two shipped
binaries, and `cargo bench --no-run`. A golden-manifest mismatch uploads the rendered frame(s)
from `target/golden-fail/` as a build artifact, so the wrong picture is one click away rather than
something you have to reproduce locally.

The last two are there because both fail in ways nothing else catches. `panic = "abort"` and thin LTO
apply only to the release profile, so a release-only compile error is real and would otherwise be
discovered while cutting a tag. And nothing references the benchmarks, so they would rot silently
until the next person needed a measurement — CI compiles them but never times them, because timings
on a shared runner are noise and a check people learn to ignore is worse than no check.

Every job pins the same toolchain `rust-toolchain.toml` gives a fresh clone. A CI that quietly ran a
newer compiler would let a lint land that nobody local sees, or reject one everybody local passes.

`docs.yml` publishes the rendered API documentation to GitHub Pages from `main`. Most of what a
contributor needs here is `//!` prose beside the code it describes, so it is worth reading rendered.

### Playing a game

`cargo xtask dev` opens the application. Drop a `.gb`, `.gbc`, `.gba`, or `.nds` file onto the window — or
paste its path into the library panel's import box — and it is indexed and starts playing. A ROM
named on the command line does the same in one step. On a DS game the lower screen is the
touchscreen: click and drag on it with the mouse.

The library is a SQLite index, not a directory scan, and that distinction is load-bearing: a title
you correct, a play count, and a save-state list all survive the file being moved to another folder,
because reconciliation recognises it again by content hash. Files that have genuinely gone are
greyed out and keep their history rather than vanishing.

| Default key | Action |
|---|---|
| `W` `A` `S` `D` | D-pad |
| `Space` / `R` | A / B |
| `T` / `G` / `Q` / `E` | X / Y / L / R (GBA and DS) |
| `Enter` / `Left Shift` | Start / Select |
| `P` | Pause |
| `Tab` (held) | Fast-forward |
| `Backspace` (held) | Rewind |
| `F1` | HUD |
| `F2` / `F3` | Quicksave / quickload (slot 0) |
| `F9` | Debugger panel |
| `F11` / `F12` | Fullscreen / screenshot |
| `Escape` | Reset |

All of them are rebindable in the **Keys** panel, and the bindings are physical key *positions*
rather than letters, so a non-QWERTY layout gets the keys under the same fingers. Settings and
keybinds are written as hand-editable TOML; run with `--data-dir <path>` to keep a whole separate
library, saves, and config, which is what to use when trying something out.

Everything the emulator writes goes to the OS-appropriate local-app-data directory — the paths are
printed at startup.

### Running a ROM without a GUI

`frontend-headless` runs a ROM with no window, no audio device, and no GPU:

```sh
cargo run -p frontend-headless -- run path/to/rom.gb --frames 600
cargo run -p frontend-headless -- run path/to/rom.gb --frames 600 --trace-every 60
cargo run -p frontend-headless -- run path/to/rom.gb --frames 120 --save-frame out.png
cargo run -p frontend-headless -- run path/to/rom.gba --frames 5000 --press start@4400 --press a@4750
cargo run -p frontend-headless -- run path/to/rom.gba --frames 1 --state slot1.ast --save-frame out.png
cargo run -p frontend-headless -- check-determinism path/to/rom.gb --frames 600
cargo run -p frontend-headless -- identify path/to/rom.gb
```

A `.gb` file runs on Game Boy hardware and a `.gbc` file on Game Boy Color hardware; what the
cartridge header says decides whether a colour machine runs in full colour or in
DMG-compatibility mode.

`--save-frame` writes the final framebuffer as a PNG, which is how a rendering test ROM gets
*looked at* rather than reduced to a hash. `identify` runs the same probe the library importer
does, so the title and content hash it prints are the ones that would be indexed.

`--press <button>@<frame>[:<frames>]` holds a button, and can be repeated for a sequence. Without
it nothing past a title screen is reachable without a window, which is most of what a commercial
game does — pressing start, loading a save, and walking around are all on the other side of it. The
frame is the first one the button is down and the count defaults to 10, which is long enough for a
game polling once a frame to see the press and short enough that it still sees the release.

`--state` loads a save state before running and `--save-state` writes one after, in the format the
window reads. Together they are what makes a bug report reproducible: a picture that is wrong only
after an hour of play cannot be reached by a press schedule, but the state file from the moment it
went wrong can be re-rendered and dumped here instead of in a window. A state resumes byte-exactly,
so running five frames past a loaded state gives the same framebuffer hash as running the whole way
from a reset.

`run` prints a framebuffer hash — the same FNV-1a the accuracy corpus records, so a hash
printed here can be pasted straight into a corpus entry. `--trace-every` prints one per N
frames, which is how you locate the frame where two builds diverge rather than just learning
that they did. `check-determinism` runs the same ROM twice from a fresh machine and compares:
determinism is what save states, rewind, and replay all rest on, and it is cheap to check and
easy to lose.

## Performance

Measured on an Apple M3, `bench` profile. Full numbers, the per-frame apportionment, and the
dynamic-recompilation decision live in `testing/harness/benches/systems.rs` — beside the benchmarks
that produced them rather than in prose that can drift from them.

| workload | frame time | speed |
|---|---|---|
| Game Boy, rendering | 246 µs | 68x |
| Game Boy, rendering + four APU channels | 361 µs | 46x |
| Game Boy Advance, ARM instructions straight from ROM | 1 372 µs | 12.2x |
| Game Boy Advance, **a commercial game** (measured outside the bench suite) | ≈3 050 µs | ≈5x |
| dmg-acid2 / cgb-acid2 | 243 / 258 µs | 69x / 65x |
| Nintendo DS, both cores spinning, displays off | 5 161 µs | 3.2x |
| Nintendo DS, engine A reading a VRAM framebuffer | 5 276 µs | 3.2x |
| Nintendo DS, the same with the sound hardware wired in | ≈5 440 µs | ≈3.1x |
| Nintendo DS 3D rasteriser, three screen-filling quads with overdraw | ≈730 µs | — |

Three findings worth surfacing:

- **The APU costs more than the PPU on a Game Boy frame** — about a third of it against a sixth. Not
  what you would guess for a machine whose job is drawing a picture, and the first place to look if
  the Game Boy ever needs to be faster.
- **No dynamic recompiler, for either CPU core**, on the evidence rather than by preference. A dynarec
  replaces dispatch and nothing else, and the worst measured workload on each system already runs at
  46x and 12.2x real time — though a *real* GBA game is about 5x, and that is the figure to hold.

  The GBA figure was 11.3x until 2026-07-31 and that number was not real: the machine charged every
  memory access three to six times over, so the same benchmark ROM got through a quarter of the
  instructions it should have. The emulator was not fast, the emulated machine was slow, and a frame
  is a fixed number of cycles either way. A real game at 5x still clears the 4x fast-forward target,
  but this is the one system where more per-instruction work would change the answer.
- **The Nintendo DS has a fifth of the margin the GBA does, and prompt 18 was right about it.** At
  3.2x real time against the other systems' 11x to 80x, it is by a wide margin the tightest — and
  that is *without* the 3D core. The dynarec question stays open for it rather than being inherited
  from the two answers above; it needs re-asking once the 3D rasteriser exists, since that is the
  workload prompt 18 expects to be the real problem.

  The first measurement was 15.0 ms against a 16.71 ms budget — 1.1x, barely real time. The cause
  was not the two 2D engines: turning both displays off saved 1.5%. It was that `NdsBus` composed
  every halfword and word access out of byte accesses, so each instruction fetch cost four region
  decodes. Reading and writing RAM at its real width dropped a frame from 15.2 ms to 5.28 ms, a
  **65% reduction**, measured before and after with `cargo bench -p harness --bench systems`. That
  is the only optimisation in the project so far, and it had a measured problem behind it.
- **The debugger's watchpoint recorder is not free**: +1.7% of a Game Boy frame, +4.5% of a GBA one,
  and +3.7% of a DS one, even disarmed, because it is a branch on every bus access. That fails the "zero measurable
  overhead" constraint it was written against, and it is kept anyway — a Cargo feature would either
  leave the shipped build paying it or leave the shipped build without watchpoints. Documented as a
  deliberate deviation with the number, not as compliance.
- **The GBA PPU debugger's layer-isolation check, unlike the watchpoint recorder above, does meet
  that constraint.** It is one `LayerOverrides` field consulted at the very end of compositing —
  three flag checks per scanline, not per bus access — measured at +2.0% on `gba/spin` and +2.1% on
  a hand-built `gba/layer_overrides` benchmark ROM with `cargo xtask bench --filter gba/`. Both are
  smaller than this project's own documented ±2x thermal-variance noise floor (see below), so the
  honest reading is "indistinguishable from zero," not "2% slower."

Two things have been optimised, each with a profiled problem behind it and a before/after to show
for it — the DS bus composing wide accesses out of byte accesses (−65% of a frame), and the GBA's
per-instruction scheduler poll, which a `sample` profile of a real game put at **39% of a frame**
against 7% for all of rendering (−8.2% of `gba/spin`, hash unchanged). Nothing else has been:
every system meets its target with 5x to 80x of margin, and an optimisation with no problem behind
it is not worth its own risk.

**Benchmark figures here move by up to 2x with the machine's thermal state.** `gba/spin` measured
2 665 µs during a long working session and 1 372 µs on identical code cooled down. Make a
before/after claim from one `--baseline` run, never against a number written down on another day.

## Architecture

The full design rationale — why this stack, what the crate boundaries mean, and what was
deliberately learned from the predecessor project — lives in
[`docs/successor-emulator/`](docs/successor-emulator/), starting with
[`00-INDEX-AND-ARCHITECTURE.md`](docs/successor-emulator/00-INDEX-AND-ARCHITECTURE.md).

Two rules are load-bearing and enforced mechanically rather than by review:

1. **Crate boundaries.** No crate under `crates/system-*`, `crates/cpu-*`, `crates/ppu-*`,
   `crates/apu-*`, `savestate`, or `core-common` may depend on `winit`, `wgpu`, `egui`, or
   `cpal`. Enforced by `cargo deny check bans` in CI.
2. **`Savable` at creation time.** Every stateful struct implements save/load when it is
   written, never bolted on later by reaching into another module's private fields.

See [CONTRIBUTING.md](CONTRIBUTING.md), and [AGENTS.md](AGENTS.md) if you are an AI agent
picking this up — it carries the conventions and the paid-for mistakes that the code itself
cannot tell you.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
