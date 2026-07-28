//! Frontend-agnostic session runtime.
//!
//! Owns everything a frontend needs that is not windowing or GPU work: the emulation thread,
//! the audio pipeline, input routing, and the rewind buffer. Consumed by both
//! `frontend-native` and `frontend-headless`, which is why nothing here may touch `winit`,
//! `wgpu`, `egui`, or `cpal`.
//!
//! Currently this crate provides the audio pipeline (see [`audio`]) and the input layer
//! (see [`input`]). The emulation thread and rewind buffer arrive with prompts 14 and 16.

#![deny(unsafe_code)]

pub mod audio;
pub mod input;

pub use audio::{channel, AudioConsumer, AudioProducer, Resampler, DEFAULT_CAPACITY};
pub use input::{
    input_channel, Action, BindError, ChromeAction, InputReceiver, InputSender, InputTracker,
    KeybindMap, PhysicalInputEvent, PhysicalKey,
};
