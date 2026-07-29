//! The `cpal` output stream — the only place in the workspace that touches an audio device.
//!
//! # What the callback does, and what it must not
//!
//! It calls [`AudioConsumer::fill`] and nothing else. No locks, no allocation, no channel, no
//! logging. The callback runs on a real-time-priority thread with a hard deadline of a few
//! milliseconds; anything that can *wait* in there will eventually wait, and a missed deadline is
//! an audible click. That is why `frontend-core`'s ring is lock-free and why the volume control
//! lives on the emulation thread rather than here.
//!
//! The consumer therefore lives inside the callback closure and is unreachable from anywhere else
//! by construction — the type system enforcing the rule instead of a comment asking for it.
//!
//! # Starting without audio is not a failure
//!
//! A machine with no output device, a container with no sound server, a device that refuses every
//! format: all of these produce [`Audio::silent`] and a warning. Refusing to start an emulator
//! because the speakers are unavailable would be a strictly worse product, and the accuracy suite
//! runs headless for exactly this reason.

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use frontend_core::{AudioConsumer, AudioProducer};

/// The open output stream, held alive for as long as the application runs.
pub struct Audio {
    /// Dropping this stops the stream, so it is kept even though nothing reads it.
    stream: Option<cpal::Stream>,
    /// What the device negotiated. The emulation thread resamples to this.
    output_rate: u32,
    channels: u16,
    device_name: String,
}

impl Audio {
    /// Open the default output device and start pulling from `consumer`.
    ///
    /// Returns the producing end for the session, along with the rate the device actually
    /// negotiated — which is frequently not 48 kHz, and assuming otherwise gives audio that is
    /// subtly the wrong pitch and drifts out of sync over minutes.
    pub fn open(capacity: usize) -> (Self, AudioProducer) {
        let (producer, consumer) = frontend_core::channel(capacity);
        match Self::try_open(consumer) {
            Ok(audio) => {
                tracing::info!(
                    "audio: {} at {} Hz, {} channels",
                    audio.device_name,
                    audio.output_rate,
                    audio.channels
                );
                (audio, producer)
            }
            Err(e) => {
                tracing::warn!("no audio output ({e:#}); running silently");
                (Self::silent(), producer)
            }
        }
    }

    fn try_open(mut consumer: AudioConsumer) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("the host reports no default output device")?;
        let device_name = device
            .id()
            .map(|id| format!("{id:?}"))
            .unwrap_or_else(|_| "unnamed device".to_string());

        let supported = device
            .default_output_config()
            .context("the device has no default output configuration")?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let output_rate = config.sample_rate;
        let channels = config.channels;

        // Only `f32` is built. Every mainstream host offers it, converting in the callback would
        // add work to the one place that must do least, and a device that genuinely cannot do
        // `f32` falls back to silence with a message naming the format — which is a better
        // outcome than five more copies of the same callback.
        if sample_format != cpal::SampleFormat::F32 {
            anyhow::bail!("the device wants {sample_format} samples; only f32 is supported");
        }

        // Interleaved stereo is what the ring produces. A mono or surround device gets the same
        // frames spread across its channels rather than silence in the extras.
        let channel_count = channels as usize;
        let mut scratch: Vec<f32> = Vec::new();
        let stream = device
            .build_output_stream(
                config,
                move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if channel_count == 2 {
                        consumer.fill(output);
                        return;
                    }
                    let frames = output.len() / channel_count.max(1);
                    scratch.resize(frames * 2, 0.0);
                    consumer.fill(&mut scratch);
                    for (frame, stereo) in output
                        .chunks_mut(channel_count)
                        .zip(scratch.chunks_exact(2))
                    {
                        // Mono gets the average rather than the left channel, so a centred sound
                        // does not come out quieter than a panned one.
                        let mixed = (stereo[0] + stereo[1]) * 0.5;
                        for (index, sample) in frame.iter_mut().enumerate() {
                            *sample = match index {
                                0 => stereo[0],
                                1 => stereo[1],
                                _ => mixed,
                            };
                        }
                        if channel_count == 1 {
                            frame[0] = mixed;
                        }
                    }
                },
                |e| tracing::warn!("audio stream error: {e}"),
                None,
            )
            .context("the device refused to build an output stream")?;
        stream
            .play()
            .context("the output stream refused to start")?;

        Ok(Self {
            stream: Some(stream),
            output_rate,
            channels,
            device_name,
        })
    }

    /// A stand-in for when no device could be opened.
    ///
    /// Reports the core's own rate, so the emulation thread's resampler becomes a pass-through and
    /// does no work for output nobody will hear.
    pub fn silent() -> Self {
        Self {
            stream: None,
            output_rate: core_common::AUDIO_SAMPLE_RATE,
            channels: 2,
            device_name: "none".to_string(),
        }
    }

    pub fn output_rate(&self) -> u32 {
        self.output_rate
    }

    /// One line for the settings panel, so a user with no sound can see whether a device was found
    /// at all before they start looking for a volume bug.
    pub fn describe(&self) -> String {
        if self.stream.is_some() {
            format!(
                "{} — {} Hz, {} ch",
                self.device_name, self.output_rate, self.channels
            )
        } else {
            "no output device".to_string()
        }
    }
}
