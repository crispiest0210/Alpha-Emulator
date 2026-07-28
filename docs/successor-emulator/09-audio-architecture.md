# Prompt 09 — Audio Architecture (`apu-shared` + output pipeline)

Read `00-INDEX-AND-ARCHITECTURE.md`, `02-core-framework.md`, and `07-scheduler-timing.md` first.

## Objective

Implement `crates/apu-shared` (shared PSG channel primitives reused across GB/GBC/GBA) and the
GB APU backend, plus the cross-thread audio delivery pipeline (emulation thread → lock-free ring
buffer → `cpal` output callback) that `frontend-core`/`frontend-native` (prompt 14) consume.

## Context

The predecessor mixed audio via the Web Audio API directly from the same JS execution context
that ran the CPU/PPU, with no dedicated audio thread — a direct instance of predecessor lesson
§3 (single-thread-does-everything). This prompt is where that gets fixed architecturally, and it
matters more for audio than almost anything else in the system: audio glitches (underruns,
buffer-timing jitter) are the most immediately, viscerally noticeable correctness failure to a
user, more so than a slightly wrong pixel.

## Architectural Decisions

- `apu-shared` provides square-wave (with duty cycle + optional frequency sweep), wave/custom-
  waveform, and noise (LFSR-based) channel primitives, plus a DAC/mixing abstraction — these
  three channel types plus a 4-bit DAC model cover GB's 4 channels and are the shared basis GBC
  reuses unmodified and GBA extends with its two additional PCM/FIFO channels (GBA-specific,
  implemented in `system-gba`, not `apu-shared`, since PCM/FIFO-via-DMA is not shared with
  GB/GBC).
- Sample generation cadence is decoupled from the frame-sequencer *timing* events (prompt 07):
  the frame sequencer schedules length/envelope/sweep updates at 512 Hz; actual sample synthesis
  runs at the emulation core's internal sample rate (commonly matched to the system's audio
  hardware, e.g. GB's ~2^20 Hz-derived rate) and is resampled to the *output* device's sample rate
  (whatever `cpal` negotiates with the OS, commonly 44.1/48 kHz) by an explicit resampler — do not
  assume the emulated system's native rate matches the host output device's rate.
- **Threading model (this is the architecturally load-bearing decision in this prompt):** the
  emulation thread (owned by `frontend-core`, prompt 14) produces audio samples as a normal part
  of `System::step_frame` / `take_audio_samples()`, pushes them into a lock-free SPSC ring buffer
  (`rtrb` or equivalent), and the `cpal` output callback (running on the OS's own audio thread)
  pulls from that ring buffer, resampling/interpolating if the buffer runs slightly ahead/behind
  rather than blocking. Never call into emulation-core code from the `cpal` callback directly, and
  never block the emulation thread waiting on the audio thread — this is precisely the coupling
  the predecessor didn't have a firm answer for.
- Buffer underrun/overrun handling must be graceful and explicit: on underrun, repeat/interpolate
  rather than produce silence-with-a-click where reasonably cheap to avoid; on overrun (emulation
  running fast, e.g. fast-forward), drop or resample down deliberately, not by unbounded buffer
  growth.

## Responsibilities

1. `crates/apu-shared`: square/wave/noise channel primitives, DAC/mixing abstraction, unit-
   testable independent of any specific system's register layout.
2. GB APU backend (`system-gb`): register-level behavior (`NR1x`–`NR5x`), hooked to prompt 07's
   frame-sequencer scheduling, producing samples via `apu-shared` primitives, exposed through
   `System::take_audio_samples()`.
3. The ring-buffer + `cpal` output pipeline, implemented in `frontend-core` (this prompt owns the
   audio-pipeline *design and implementation*; prompt 14 wires it into the running application
   alongside video/input, but the actual ring-buffer/resampler code belongs here since it's audio-
   architecture work, not general frontend orchestration — coordinate crate placement so it's not
   duplicated between the two prompts).

## Interfaces

```rust
// apu-shared
pub struct SquareChannel { /* duty, sweep, envelope, length */ }
pub struct WaveChannel { /* custom waveform RAM, volume shift */ }
pub struct NoiseChannel { /* LFSR, envelope, length */ }
pub trait Dac { fn mix(&self, channels: &[ChannelSample]) -> AudioSample; }
```
```rust
// frontend-core audio pipeline
pub struct AudioPipeline {
    pub fn producer(&self) -> RingBufferProducer<AudioSample>; // used by emulation thread
    pub fn start_output(&mut self, target_sample_rate: u32) -> Result<(), AudioError>; // cpal setup
}
```
Exact types are the implementer's call; the contract is: emulation-thread-side push, audio-
callback-side pull, no shared mutex on the hot path.

## Constraints

- `apu-shared` has no dependency on `cpal` (it's pure sample-synthesis logic); the ring-buffer/
  `cpal` pipeline code, wherever it lives, has no dependency on any `system-*` crate — it only
  knows about `AudioSample` from `core-common`.
- No blocking synchronization primitives (mutex, channel with blocking recv) on either side of
  the ring buffer's hot path.

## Deliverables

- `crates/apu-shared` fully implemented and unit-tested.
- GB APU backend, register-accurate, hooked to the frame sequencer.
- Working ring-buffer + `cpal` output pipeline, demonstrably glitch-free under normal play and
  under fast-forward (2x+) in manual testing.

## Acceptance Criteria

- Passes Blargg's `dmg_sound` test ROM suite via the accuracy harness (prompt 17) — this is the
  concrete correctness bar for GB APU behavior, covering exactly the kind of envelope/sweep/
  length-counter edge cases that are easy to get subtly wrong.
- Manual test: sustained gameplay audio with no audible clicks/underrun artifacts under normal
  speed and fast-forward, verified by the implementer (or noted explicitly as unverified if no
  audio hardware is available in the build/test environment).

## Testing Requirements

- `apu-shared` unit tests: duty-cycle waveform correctness, envelope timing, sweep frequency
  calculation (including the documented GB sweep-overflow-disables-channel quirk), noise LFSR
  sequence correctness (7-bit and 15-bit modes), length-counter behavior.
- Integration: Blargg `dmg_sound` suite via `testing/harness`.
- Ring buffer: unit/stress test for producer-faster-than-consumer and consumer-faster-than-
  producer scenarios, verifying no panics/deadlocks/unbounded growth.

## Future Compatibility

GBC (prompt 11) reuses this APU backend with double-speed-mode timing adjustments handled the
same way CPU double-speed is (prompt 03's note: speed-mode awareness lives in `system-gbc`'s
scheduler wiring, not in `apu-shared` or the base channel logic). GBA (prompt 12) reuses
`apu-shared`'s four PSG channels for GBA's backward-compatible sound and adds two independent
PCM/FIFO channels on top, fed by DMA (coordinate with prompt 06's DMA-as-scheduled-event
pattern). NDS (prompt 13) has yet another, larger PCM/mixing hardware set — evaluate at that
point whether it warrants its own shared-primitives split or is different enough to implement
directly in `system-nds`.

## Notes

Audio underrun is the failure mode most likely to make the whole project feel unpolished even
when CPU/PPU accuracy is excellent — do not treat the ring-buffer/resampler work as a minor
plumbing detail relative to APU register accuracy; both matter for the end result.
