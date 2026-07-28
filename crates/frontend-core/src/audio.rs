//! The cross-thread audio pipeline: emulation thread → lock-free ring → audio callback.
//!
//! # The threading model, which is the point of this module
//!
//! The emulation thread produces samples as part of stepping a frame and pushes them into a
//! single-producer single-consumer ring. The OS audio callback, running on its own thread,
//! pulls from the other end. **Neither side ever blocks on the other, and neither ever takes
//! a lock.**
//!
//! That is not a performance nicety. An audio callback that blocks — on a mutex, on a
//! channel, on anything — misses its deadline, and a missed deadline is an audible click. The
//! predecessor project ran emulation, video, and audio on one thread and had no answer for
//! this; it is the concrete fix.
//!
//! It also means the emulation thread must never wait for the audio thread to drain. If
//! emulation runs ahead — fast-forward, or simply a fast machine — the excess is dropped
//! deliberately at the producer rather than allowed to grow a buffer without bound.
//!
//! # Where `cpal` is
//!
//! Not here. The crate-boundary rule confines `cpal` to `frontend-native`, and this module
//! respects it: everything here is pure logic over [`AudioSample`], fully testable with no
//! audio device present. `frontend-native` opens the output stream and does nothing in the
//! callback but call [`AudioConsumer::fill`].
//!
//! The split is also what makes the failure modes testable at all. Underrun and overrun
//! behavior are exercised by unit tests below rather than by listening for clicks.

use core_common::AudioSample;
use rtrb::{Consumer, Producer, RingBuffer};

/// Default ring capacity, in stereo samples.
///
/// About 85 ms at 48 kHz. Large enough that an ordinary scheduling hiccup on either thread
/// does not empty it, small enough that the latency between an emulated sound and hearing it
/// stays well under a frame's worth of perceptual slack.
pub const DEFAULT_CAPACITY: usize = 4096;

/// Create a connected producer and consumer pair.
pub fn channel(capacity: usize) -> (AudioProducer, AudioConsumer) {
    let (producer, consumer) = RingBuffer::new(capacity.max(2));
    (
        AudioProducer {
            inner: producer,
            dropped: 0,
        },
        AudioConsumer {
            inner: consumer,
            last: AudioSample::SILENCE,
            underruns: 0,
        },
    )
}

/// The emulation thread's end.
pub struct AudioProducer {
    inner: Producer<AudioSample>,
    dropped: u64,
}

impl AudioProducer {
    /// Push as many samples as fit, returning how many were accepted.
    ///
    /// Anything that does not fit is **dropped**, not buffered. When emulation runs faster
    /// than real time the excess has nowhere to go: holding it would grow memory without
    /// bound and add latency that never recovers, and blocking would stall emulation on the
    /// audio thread. Dropping is the only option that stays bounded, and it is what makes
    /// fast-forward sound sped-up rather than delayed.
    pub fn push(&mut self, samples: &[AudioSample]) -> usize {
        let mut written = 0;
        for &sample in samples {
            if self.inner.push(sample).is_err() {
                self.dropped += (samples.len() - written) as u64;
                return written;
            }
            written += 1;
        }
        written
    }

    /// Samples discarded because the consumer could not keep up.
    ///
    /// Expected to climb during fast-forward and to stay flat otherwise; a rising count at
    /// normal speed means the output rate is wrong.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn reset_counters(&mut self) {
        self.dropped = 0;
    }

    /// Free space, in samples.
    pub fn free(&self) -> usize {
        self.inner.slots()
    }
}

/// The audio callback's end.
pub struct AudioConsumer {
    inner: Consumer<AudioSample>,
    /// The most recent real sample, held through an underrun.
    last: AudioSample,
    underruns: u64,
}

impl AudioConsumer {
    /// Fill an interleaved stereo `f32` buffer, the shape every host audio API wants.
    ///
    /// Returns how many samples were real. On underrun the remainder holds the **last real
    /// sample** rather than zeroes: jumping to silence puts a step discontinuity in the
    /// waveform, which is heard as a click, while holding a level is at worst a brief
    /// flatness. Neither is correct, but one is far less objectionable, and underruns happen
    /// on any machine occasionally.
    pub fn fill(&mut self, out: &mut [f32]) -> usize {
        let mut filled = 0;
        for frame in out.chunks_exact_mut(2) {
            match self.inner.pop() {
                Ok(sample) => {
                    self.last = sample;
                    filled += 1;
                }
                Err(_) => {
                    self.underruns += 1;
                }
            }
            frame[0] = self.last.left;
            frame[1] = self.last.right;
        }
        filled
    }

    /// Samples the callback wanted but the producer had not supplied.
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    pub fn reset_counters(&mut self) {
        self.underruns = 0;
    }

    /// Samples ready to be consumed.
    pub fn available(&self) -> usize {
        self.inner.slots()
    }
}

/// Linear resampler between the core's fixed rate and whatever the host negotiated.
///
/// The emulated systems all resample internally to
/// [`AUDIO_SAMPLE_RATE`](core_common::AUDIO_SAMPLE_RATE), but the output device gets whatever
/// the OS offers — commonly 44100, sometimes 48000, occasionally neither. Assuming they match
/// produces audio that is subtly the wrong pitch and slowly drifts out of sync, which is
/// harder to diagnose than an obvious failure.
///
/// Linear interpolation is chosen over anything higher-order deliberately: the source material
/// is already band-limited square and noise waveforms, the ratio is close to 1, and a
/// windowed-sinc filter would cost far more than the artefacts it removes are worth here.
#[derive(Debug, Clone)]
pub struct Resampler {
    source_rate: u32,
    target_rate: u32,
    /// Fractional read position within the input, carried across calls so successive blocks
    /// join seamlessly instead of clicking at every boundary.
    position: f64,
    /// The last input sample of the previous block, needed to interpolate across the seam.
    previous: AudioSample,
    primed: bool,
}

impl Resampler {
    pub fn new(source_rate: u32, target_rate: u32) -> Self {
        Self {
            source_rate: source_rate.max(1),
            target_rate: target_rate.max(1),
            position: 0.0,
            previous: AudioSample::SILENCE,
            primed: false,
        }
    }

    pub fn set_target_rate(&mut self, target_rate: u32) {
        self.target_rate = target_rate.max(1);
    }

    pub fn source_rate(&self) -> u32 {
        self.source_rate
    }

    pub fn target_rate(&self) -> u32 {
        self.target_rate
    }

    /// Input samples consumed per output sample.
    #[inline]
    fn step(&self) -> f64 {
        self.source_rate as f64 / self.target_rate as f64
    }

    /// Resample `input`, appending to `out`.
    ///
    /// `out` is appended to rather than replaced so a caller can accumulate several blocks
    /// before pushing them to the ring.
    ///
    /// # One sample of latency
    ///
    /// Interpolating *to* the final input sample requires the sample after it, which has not
    /// arrived yet. So each call holds its last input back and emits it at the start of the
    /// next call. A block of `n` samples therefore yields `n` outputs in steady state and
    /// `n - 1` on the very first call. This is inherent to streaming interpolation, not a
    /// rounding artefact — the alternative is to duplicate the final sample, which puts a
    /// small flat step at every block boundary.
    pub fn process(&mut self, input: &[AudioSample], out: &mut Vec<AudioSample>) {
        if input.is_empty() {
            return;
        }

        // The working sequence is the sample carried over from last time followed by this
        // block. Indexing through a closure avoids allocating to concatenate them.
        let carried = self.previous;
        let has_carry = self.primed;
        let len = input.len() + has_carry as usize;
        let at = |index: usize| -> AudioSample {
            match (has_carry, index) {
                (true, 0) => carried,
                (true, i) => input[i - 1],
                (false, i) => input[i],
            }
        };

        let step = self.step();
        // Stop one short of the end: the last sample has no successor to interpolate toward.
        while (self.position.floor() as usize) + 1 < len {
            let index = self.position.floor() as usize;
            let fraction = (self.position - index as f64) as f32;
            let a = at(index);
            let b = at(index + 1);
            out.push(AudioSample::stereo(
                a.left + (b.left - a.left) * fraction,
                a.right + (b.right - a.right) * fraction,
            ));
            self.position += step;
        }

        // Carry the leftover fraction rather than rounding it away, or the output rate drifts
        // by a fraction of a sample per block and slowly desynchronizes.
        self.position -= (len - 1) as f64;
        self.previous = input[input.len() - 1];
        self.primed = true;
    }

    /// Discard the interpolation state, for a reset or a state load.
    pub fn reset(&mut self) {
        self.position = 0.0;
        self.previous = AudioSample::SILENCE;
        self.primed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(count: usize) -> Vec<AudioSample> {
        (0..count).map(|i| AudioSample::mono(i as f32)).collect()
    }

    #[test]
    fn samples_come_out_in_the_order_they_went_in() {
        let (mut producer, mut consumer) = channel(64);
        let input = ramp(8);
        assert_eq!(producer.push(&input), 8);

        let mut out = vec![0.0f32; 16];
        assert_eq!(consumer.fill(&mut out), 8);
        for (i, frame) in out.chunks_exact(2).enumerate() {
            assert_eq!(frame[0], i as f32);
            assert_eq!(frame[1], i as f32);
        }
    }

    #[test]
    fn a_producer_running_ahead_drops_rather_than_growing() {
        // Fast-forward. The excess has nowhere to go: buffering it would grow without bound
        // and add latency that never recovers.
        let (mut producer, consumer) = channel(16);
        let accepted = producer.push(&ramp(100));

        assert!(accepted <= 16, "only what fits is accepted");
        assert_eq!(producer.dropped(), (100 - accepted) as u64);
        assert_eq!(consumer.available(), accepted, "and nothing grew");
    }

    #[test]
    fn repeated_overruns_stay_bounded() {
        let (mut producer, mut consumer) = channel(32);
        for _ in 0..1000 {
            producer.push(&ramp(64));
            // The consumer takes only a little each round.
            let mut out = vec![0.0f32; 8];
            consumer.fill(&mut out);
        }
        assert!(
            consumer.available() <= 32,
            "the ring never exceeded capacity"
        );
        assert!(producer.dropped() > 0);
    }

    #[test]
    fn an_underrun_holds_the_last_sample_instead_of_dropping_to_silence() {
        // Jumping to zero is a step discontinuity, which is heard as a click. Holding a level
        // is at worst a brief flatness.
        let (mut producer, mut consumer) = channel(64);
        producer.push(&[AudioSample::stereo(0.5, -0.5)]);

        let mut out = vec![0.0f32; 8];
        let real = consumer.fill(&mut out);
        assert_eq!(real, 1);
        assert_eq!(consumer.underruns(), 3);

        // Every frame, real or held, carries the same value.
        for frame in out.chunks_exact(2) {
            assert_eq!(frame[0], 0.5);
            assert_eq!(frame[1], -0.5);
        }
    }

    #[test]
    fn an_underrun_before_any_audio_produces_silence_not_noise() {
        let (_producer, mut consumer) = channel(16);
        let mut out = vec![1.0f32; 8];
        assert_eq!(consumer.fill(&mut out), 0);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(consumer.underruns(), 4);
    }

    #[test]
    fn a_consumer_running_ahead_never_panics_or_deadlocks() {
        let (mut producer, mut consumer) = channel(32);
        for round in 0..1000 {
            if round % 10 == 0 {
                producer.push(&ramp(4));
            }
            let mut out = vec![0.0f32; 64];
            consumer.fill(&mut out);
        }
        assert!(consumer.underruns() > 0, "it did starve, and survived");
    }

    #[test]
    fn the_two_ends_work_across_threads() {
        use std::thread;

        let (mut producer, mut consumer) = channel(256);
        let writer = thread::spawn(move || {
            let mut sent = 0usize;
            for _ in 0..500 {
                sent += producer.push(&ramp(16));
                std::thread::yield_now();
            }
            sent
        });

        let reader = thread::spawn(move || {
            let mut received = 0usize;
            let mut out = vec![0.0f32; 32];
            for _ in 0..500 {
                received += consumer.fill(&mut out);
                std::thread::yield_now();
            }
            received
        });

        let sent = writer.join().unwrap();
        let received = reader.join().unwrap();
        assert!(sent > 0 && received > 0);
        assert!(received <= sent, "nothing was invented");
    }

    // -- Resampling ----------------------------------------------------------

    #[test]
    fn matching_rates_pass_samples_through_unchanged() {
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        // The first call holds its last sample back, so eight in gives seven out.
        r.process(&ramp(8), &mut out);
        assert_eq!(out.len(), 7);

        // A second block releases it, and the stream is continuous across the seam.
        r.process(&[AudioSample::mono(8.0)], &mut out);
        assert_eq!(out.len(), 8);
        for (i, sample) in out.iter().enumerate() {
            assert!(
                (sample.left - i as f32).abs() < 1e-4,
                "sample {i} was {}",
                sample.left
            );
        }
    }

    #[test]
    fn downsampling_produces_proportionally_fewer_samples() {
        // 48000 into 44100 is the common case, and getting it wrong makes everything play at
        // the wrong pitch and drift out of sync.
        let mut r = Resampler::new(48_000, 44_100);
        let mut out = Vec::new();
        r.process(&ramp(4800), &mut out);

        let expected = 4800.0 * 44_100.0 / 48_000.0;
        assert!(
            (out.len() as f64 - expected).abs() < 2.0,
            "expected about {expected}, got {}",
            out.len()
        );
    }

    #[test]
    fn upsampling_produces_proportionally_more() {
        let mut r = Resampler::new(22_050, 44_100);
        let mut out = Vec::new();
        r.process(&ramp(1000), &mut out);
        assert!((out.len() as i64 - 2000).abs() <= 3, "got {}", out.len());
    }

    #[test]
    fn the_fractional_position_carries_across_blocks_so_the_rate_does_not_drift() {
        // Rounding the leftover away each block would lose a fraction of a sample every
        // time, which accumulates into audible drift over minutes.
        let mut r = Resampler::new(48_000, 44_100);
        let mut total = 0usize;
        for _ in 0..100 {
            let mut out = Vec::new();
            r.process(&ramp(48), &mut out);
            total += out.len();
        }
        // Within a sample or two of the ideal ratio, and crucially not accumulating.
        let expected = 4800.0 * 44_100.0 / 48_000.0;
        assert!(
            (total as f64 - expected).abs() < 3.0,
            "expected about {expected} over 100 blocks, got {total}"
        );
    }

    #[test]
    fn interpolation_is_continuous_across_a_block_boundary() {
        // A discontinuity at every block edge would be a periodic click at the block rate.
        let mut r = Resampler::new(48_000, 96_000);
        let mut out = Vec::new();
        r.process(&[AudioSample::mono(0.0), AudioSample::mono(1.0)], &mut out);
        let before = out.len();
        r.process(&[AudioSample::mono(2.0), AudioSample::mono(3.0)], &mut out);

        // Values must rise monotonically through the seam rather than jumping back.
        for pair in out.windows(2).skip(before.saturating_sub(2)) {
            assert!(
                pair[1].left >= pair[0].left - 1e-4,
                "discontinuity: {} then {}",
                pair[0].left,
                pair[1].left
            );
        }
    }

    #[test]
    fn an_empty_block_is_a_no_op() {
        let mut r = Resampler::new(48_000, 44_100);
        let mut out = Vec::new();
        r.process(&[], &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn resetting_clears_the_interpolation_state() {
        let mut r = Resampler::new(48_000, 44_100);
        let mut out = Vec::new();
        r.process(&ramp(100), &mut out);
        r.reset();

        let mut fresh = Vec::new();
        r.process(&ramp(100), &mut fresh);

        let mut reference = Resampler::new(48_000, 44_100);
        let mut expected = Vec::new();
        reference.process(&ramp(100), &mut expected);
        assert_eq!(fresh.len(), expected.len());
    }

    #[test]
    fn a_degenerate_rate_does_not_divide_by_zero() {
        let mut r = Resampler::new(0, 0);
        assert_eq!(r.source_rate(), 1);
        assert_eq!(r.target_rate(), 1);
        let mut out = Vec::new();
        r.process(&ramp(4), &mut out);
        assert_eq!(out.len(), 3, "four in, one held back");
    }
}
