//! Input: physical keys in, [`InputState`] out.
//!
//! # Two layers, one owner per key
//!
//! Physical input (a key on a keyboard) and logical input (an emulated button, or a frontend
//! action like "save state") are separate layers joined by an explicit, user-editable map.
//!
//! That structure exists to prevent a specific failure. The predecessor project had the GBA's
//! SELECT button and its HUD toggle both wanting the same key, and the behavior depended on
//! which event handler happened to run first — a conflict discovered at runtime rather than
//! rejected at configuration time. Here **a physical key maps to exactly one action**, and
//! [`KeybindMap::bind`] refuses a binding that would take a key already claimed by the other
//! category, naming both sides. There is no precedence rule to get wrong because there is
//! never more than one claimant.
//!
//! # Where `winit` is
//!
//! Not here. [`PhysicalKey`] is this crate's own neutral enum, and `frontend-native`
//! translates `winit`'s key codes into it. That keeps the crate-boundary rule intact and,
//! more usefully, makes the whole mapping layer testable by feeding it synthetic events with
//! no window open.
//!
//! # Gamepads
//!
//! Not implemented. Adding them means a second physical-input source (`gilrs` or similar)
//! with its own device-hotplug lifecycle and its own axis-to-button thresholds — a
//! meaningfully larger scope than translating keys. The layering here accommodates it without
//! rework: a gamepad button becomes another `PhysicalKey`-like source feeding the same
//! [`InputTracker`]. Recorded as explicit future work rather than left ambiguous.

use core_common::{Buttons, InputState, TouchPoint};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};

/// A key on the keyboard, independent of any windowing library.
///
/// A subset large enough to bind comfortably; extend it as needed rather than reaching for a
/// raw scancode, which would be neither portable nor readable in a config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum PhysicalKey {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Space,
    Enter,
    Tab,
    Backspace,
    Escape,
    ShiftLeft,
    ShiftRight,
    ControlLeft,
    ControlRight,
    AltLeft,
    AltRight,
    Comma,
    Period,
    Slash,
    Semicolon,
    Quote,
    BracketLeft,
    BracketRight,
    Backslash,
    Minus,
    Equal,
    Backquote,
}

/// A frontend action, as opposed to an emulated button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChromeAction {
    TogglePause,
    ToggleHud,
    ToggleFullscreen,
    ToggleDebugger,
    SaveState,
    LoadState,
    Screenshot,
    Reset,
    /// Held, not pressed.
    FastForward,
    /// Held, not pressed.
    Rewind,
}

impl ChromeAction {
    /// Whether the action is active for as long as the key is held, rather than firing once
    /// per press.
    ///
    /// Fast-forward and rewind are the only two: you hold them. Everything else is a discrete
    /// command, and repeating it every frame the key stays down would, for instance, write a
    /// save state sixty times a second.
    pub const fn is_held(self) -> bool {
        matches!(self, ChromeAction::FastForward | ChromeAction::Rewind)
    }

    pub const fn name(self) -> &'static str {
        match self {
            ChromeAction::TogglePause => "toggle pause",
            ChromeAction::ToggleHud => "toggle HUD",
            ChromeAction::ToggleFullscreen => "toggle fullscreen",
            ChromeAction::ToggleDebugger => "toggle debugger",
            ChromeAction::SaveState => "save state",
            ChromeAction::LoadState => "load state",
            ChromeAction::Screenshot => "screenshot",
            ChromeAction::Reset => "reset",
            ChromeAction::FastForward => "fast-forward",
            ChromeAction::Rewind => "rewind",
        }
    }
}

/// What a physical key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// An emulated console button. Systems ignore buttons they do not have.
    Button(Buttons),
    Chrome(ChromeAction),
}

/// The button names a config file uses.
const BUTTON_NAMES: &[(&str, Buttons)] = &[
    ("button_a", Buttons::A),
    ("button_b", Buttons::B),
    ("button_x", Buttons::X),
    ("button_y", Buttons::Y),
    ("button_l", Buttons::L),
    ("button_r", Buttons::R),
    ("button_start", Buttons::START),
    ("button_select", Buttons::SELECT),
    ("button_up", Buttons::UP),
    ("button_down", Buttons::DOWN),
    ("button_left", Buttons::LEFT),
    ("button_right", Buttons::RIGHT),
];

const CHROME_NAMES: &[(&str, ChromeAction)] = &[
    ("toggle_pause", ChromeAction::TogglePause),
    ("toggle_hud", ChromeAction::ToggleHud),
    ("toggle_fullscreen", ChromeAction::ToggleFullscreen),
    ("toggle_debugger", ChromeAction::ToggleDebugger),
    ("save_state", ChromeAction::SaveState),
    ("load_state", ChromeAction::LoadState),
    ("screenshot", ChromeAction::Screenshot),
    ("reset", ChromeAction::Reset),
    ("fast_forward", ChromeAction::FastForward),
    ("rewind", ChromeAction::Rewind),
];

impl Action {
    fn describe(self) -> String {
        match self {
            Action::Button(buttons) => format!("emulated button {buttons:?}"),
            Action::Chrome(action) => format!("the {} action", action.name()),
        }
    }

    /// The name this action takes in a config file.
    pub fn config_name(self) -> &'static str {
        match self {
            Action::Button(buttons) => BUTTON_NAMES
                .iter()
                .find(|(_, b)| *b == buttons)
                .map(|(name, _)| *name)
                .unwrap_or("button_unknown"),
            Action::Chrome(action) => CHROME_NAMES
                .iter()
                .find(|(_, a)| *a == action)
                .map(|(name, _)| *name)
                .unwrap_or("unknown"),
        }
    }

    pub fn from_config_name(name: &str) -> Option<Self> {
        if let Some((_, buttons)) = BUTTON_NAMES.iter().find(|(n, _)| *n == name) {
            return Some(Action::Button(*buttons));
        }
        CHROME_NAMES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, action)| Action::Chrome(*action))
    }
}

/// Serialized as its config name, so a keybind file reads `W = "button_up"` rather than
/// carrying a bit pattern that means nothing to whoever opens it.
impl Serialize for Action {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.config_name())
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Action::from_config_name(&name)
            .ok_or_else(|| D::Error::custom(format!("unknown input action {name:?}")))
    }
}

/// Why a binding was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    #[error("{key:?} is already bound to {existing}; unbind it first")]
    AlreadyBound { key: PhysicalKey, existing: String },
}

/// The physical-to-logical map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeybindMap {
    /// Ordered so a serialized config is stable rather than reshuffling on every save.
    bindings: BTreeMap<PhysicalKey, Action>,
}

impl Default for KeybindMap {
    fn default() -> Self {
        Self::defaults()
    }
}

impl KeybindMap {
    pub fn empty() -> Self {
        Self {
            bindings: BTreeMap::new(),
        }
    }

    /// The out-of-the-box layout.
    ///
    /// A starting point chosen for familiarity rather than an architectural commitment; the
    /// frontend is free to ship a different default set, and users rebind freely. The d-pad is
    /// on WASD with the face buttons under the right hand, and every frontend action is on a
    /// function key or a modifier so it cannot collide with a natural gameplay reach.
    pub fn defaults() -> Self {
        let mut map = Self::empty();
        let mut bind = |key, action| {
            map.bindings.insert(key, action);
        };

        bind(PhysicalKey::W, Action::Button(Buttons::UP));
        bind(PhysicalKey::S, Action::Button(Buttons::DOWN));
        bind(PhysicalKey::A, Action::Button(Buttons::LEFT));
        bind(PhysicalKey::D, Action::Button(Buttons::RIGHT));
        bind(PhysicalKey::Space, Action::Button(Buttons::A));
        bind(PhysicalKey::R, Action::Button(Buttons::B));
        // GBA and DS only; harmless on a Game Boy, which simply never reads them.
        bind(PhysicalKey::T, Action::Button(Buttons::X));
        bind(PhysicalKey::G, Action::Button(Buttons::Y));
        bind(PhysicalKey::Q, Action::Button(Buttons::L));
        bind(PhysicalKey::E, Action::Button(Buttons::R));
        bind(PhysicalKey::Enter, Action::Button(Buttons::START));
        bind(PhysicalKey::ShiftLeft, Action::Button(Buttons::SELECT));

        bind(PhysicalKey::P, Action::Chrome(ChromeAction::TogglePause));
        bind(PhysicalKey::F1, Action::Chrome(ChromeAction::ToggleHud));
        bind(PhysicalKey::F2, Action::Chrome(ChromeAction::SaveState));
        bind(PhysicalKey::F3, Action::Chrome(ChromeAction::LoadState));
        bind(
            PhysicalKey::F9,
            Action::Chrome(ChromeAction::ToggleDebugger),
        );
        bind(
            PhysicalKey::F11,
            Action::Chrome(ChromeAction::ToggleFullscreen),
        );
        bind(PhysicalKey::F12, Action::Chrome(ChromeAction::Screenshot));
        bind(PhysicalKey::Tab, Action::Chrome(ChromeAction::FastForward));
        bind(PhysicalKey::Backspace, Action::Chrome(ChromeAction::Rewind));
        bind(PhysicalKey::Escape, Action::Chrome(ChromeAction::Reset));

        map
    }

    /// Bind a key, refusing to take one that is already claimed.
    ///
    /// This is the conflict rule, and it is deliberately symmetric: a chrome action cannot
    /// steal an emulated button's key and an emulated button cannot steal a chrome action's.
    /// Rebinding the *same* action to a key it already holds is allowed, so re-applying a
    /// config is idempotent.
    pub fn bind(&mut self, key: PhysicalKey, action: Action) -> Result<(), BindError> {
        match self.bindings.get(&key) {
            Some(existing) if *existing != action => Err(BindError::AlreadyBound {
                key,
                existing: existing.describe(),
            }),
            _ => {
                self.bindings.insert(key, action);
                Ok(())
            }
        }
    }

    /// Bind a key, displacing whatever held it.
    ///
    /// For a rebinding UI where the user has already been shown the conflict and confirmed.
    /// Returns the action that was displaced.
    pub fn rebind(&mut self, key: PhysicalKey, action: Action) -> Option<Action> {
        self.bindings.insert(key, action)
    }

    pub fn unbind(&mut self, key: PhysicalKey) -> Option<Action> {
        self.bindings.remove(&key)
    }

    pub fn action_for(&self, key: PhysicalKey) -> Option<Action> {
        self.bindings.get(&key).copied()
    }

    /// Every key bound to `action`. An action may have several keys.
    pub fn keys_for(&self, action: Action) -> Vec<PhysicalKey> {
        self.bindings
            .iter()
            .filter(|(_, bound)| **bound == action)
            .map(|(key, _)| *key)
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (PhysicalKey, Action)> + '_ {
        self.bindings.iter().map(|(k, v)| (*k, *v))
    }

    /// Serialize to TOML for the user's config file.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

/// One physical key transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalInputEvent {
    pub key: PhysicalKey,
    pub pressed: bool,
}

/// Accumulates physical events into the state the emulation thread consumes.
///
/// Lives on the thread where window events arrive. Buttons are *level*-triggered, so what
/// matters is which keys are held at the frame boundary; discrete chrome actions are
/// *edge*-triggered and queue up so a press is never missed between frames, even if the key
/// is released again before the next one.
#[derive(Debug, Clone, Default)]
pub struct InputTracker {
    held: BTreeSet<PhysicalKey>,
    /// Discrete actions pressed since the last drain.
    pending_chrome: Vec<ChromeAction>,
    touch: Option<TouchPoint>,
}

impl InputTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one key transition.
    pub fn apply(&mut self, map: &KeybindMap, event: PhysicalInputEvent) {
        if event.pressed {
            // Ignore auto-repeat: the key is already down, and a repeat is not a new press.
            if !self.held.insert(event.key) {
                return;
            }
            if let Some(Action::Chrome(action)) = map.action_for(event.key) {
                if !action.is_held() {
                    self.pending_chrome.push(action);
                }
            }
        } else {
            self.held.remove(&event.key);
        }
    }

    /// Clear all held keys.
    ///
    /// Called when the window loses focus. Without it a key held at the moment of
    /// alt-tabbing stays down forever, because the release event goes to another window —
    /// the classic "character keeps walking into a wall" bug.
    pub fn release_all(&mut self) {
        self.held.clear();
    }

    /// Set or clear the touch point, from mouse or stylus input.
    pub fn set_touch(&mut self, touch: Option<TouchPoint>) {
        self.touch = touch;
    }

    /// The logical state to hand to the emulation thread.
    pub fn input_state(&self, map: &KeybindMap) -> InputState {
        let mut buttons = Buttons::empty();
        for key in &self.held {
            if let Some(Action::Button(button)) = map.action_for(*key) {
                buttons |= button;
            }
        }
        InputState {
            buttons,
            touch: self.touch,
        }
    }

    /// Whether a held chrome action is currently active.
    pub fn is_held(&self, map: &KeybindMap, action: ChromeAction) -> bool {
        self.held
            .iter()
            .any(|key| map.action_for(*key) == Some(Action::Chrome(action)))
    }

    /// Take the discrete chrome actions pressed since the last call.
    pub fn take_chrome_actions(&mut self) -> Vec<ChromeAction> {
        std::mem::take(&mut self.pending_chrome)
    }
}

/// Non-blocking, latest-wins delivery of input to the emulation thread.
///
/// A single shared slot holding the whole [`InputState`] packed into one atomic word. The UI
/// thread stores; the emulation thread loads. Neither blocks, neither locks, and there is no
/// queue to fall behind.
///
/// A bounded channel would be the obvious choice and is the wrong one here: when it fills,
/// the *newest* state is the one rejected, so a busy emulation thread would end up reading
/// stale input while fresh input was discarded. Latest-wins is what input needs — old input
/// is not worth delivering late — and a single atomic gives exactly that.
///
/// Latency is at most one frame by construction: the UI thread stores once per frame and the
/// emulation thread loads at its own frame boundary.
pub fn input_channel() -> (InputSender, InputReceiver) {
    let slot = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(pack(
        InputState::default(),
    )));
    (InputSender { slot: slot.clone() }, InputReceiver { slot })
}

/// Pack an input state into one word.
///
/// Twelve button bits, a touch-present flag, and two 16-bit coordinates come to 49 bits, which
/// fits comfortably. Packing is what lets the whole state move atomically — publishing buttons
/// and touch separately could hand the emulation thread a frame that never existed.
fn pack(state: InputState) -> u64 {
    let (present, x, y) = match state.touch {
        Some(point) => (1u64, point.x as u64, point.y as u64),
        None => (0, 0, 0),
    };
    state.buttons.bits() as u64 | (present << 16) | (x << 17) | (y << 33)
}

fn unpack(word: u64) -> InputState {
    InputState {
        buttons: Buttons::from_bits_truncate(word as u16),
        touch: if word & (1 << 16) != 0 {
            Some(TouchPoint {
                x: (word >> 17) as u16,
                y: (word >> 33) as u16,
            })
        } else {
            None
        },
    }
}

/// The UI thread's end.
#[derive(Debug, Clone)]
pub struct InputSender {
    slot: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl InputSender {
    /// Publish the current state, replacing whatever was there.
    pub fn send(&self, state: InputState) {
        self.slot
            .store(pack(state), std::sync::atomic::Ordering::Release);
    }
}

/// The emulation thread's end.
#[derive(Debug, Clone)]
pub struct InputReceiver {
    slot: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl InputReceiver {
    /// The most recently published state.
    ///
    /// A frame with no new input still reports the same buttons, because the slot keeps its
    /// value — which is correct: no events means the player is still holding what they held.
    pub fn latest(&mut self) -> InputState {
        unpack(self.slot.load(std::sync::atomic::Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(key: PhysicalKey) -> PhysicalInputEvent {
        PhysicalInputEvent { key, pressed: true }
    }

    fn release(key: PhysicalKey) -> PhysicalInputEvent {
        PhysicalInputEvent {
            key,
            pressed: false,
        }
    }

    #[test]
    fn held_keys_become_pressed_buttons() {
        let map = KeybindMap::defaults();
        let mut tracker = InputTracker::new();

        tracker.apply(&map, press(PhysicalKey::W));
        tracker.apply(&map, press(PhysicalKey::Space));
        let state = tracker.input_state(&map);
        assert!(state.is_pressed(Buttons::UP));
        assert!(state.is_pressed(Buttons::A));
        assert!(!state.is_pressed(Buttons::DOWN));

        tracker.apply(&map, release(PhysicalKey::W));
        assert!(!tracker.input_state(&map).is_pressed(Buttons::UP));
        assert!(tracker.input_state(&map).is_pressed(Buttons::A));
    }

    #[test]
    fn several_buttons_combine_rather_than_replacing() {
        let map = KeybindMap::defaults();
        let mut tracker = InputTracker::new();
        for key in [
            PhysicalKey::W,
            PhysicalKey::D,
            PhysicalKey::Space,
            PhysicalKey::Enter,
        ] {
            tracker.apply(&map, press(key));
        }
        let state = tracker.input_state(&map);
        assert_eq!(
            state.buttons,
            Buttons::UP | Buttons::RIGHT | Buttons::A | Buttons::START
        );
    }

    #[test]
    fn a_key_bound_to_nothing_does_nothing() {
        let map = KeybindMap::empty();
        let mut tracker = InputTracker::new();
        tracker.apply(&map, press(PhysicalKey::W));
        assert_eq!(tracker.input_state(&map).buttons, Buttons::empty());
    }

    #[test]
    fn buttons_a_system_does_not_have_are_simply_unused() {
        // The map is a superset; a Game Boy never reads the shoulder bits.
        let map = KeybindMap::defaults();
        let mut tracker = InputTracker::new();
        tracker.apply(&map, press(PhysicalKey::Q));
        assert!(tracker.input_state(&map).is_pressed(Buttons::L));
    }

    // -- The conflict rule ---------------------------------------------------

    #[test]
    fn binding_a_key_that_a_chrome_action_holds_is_refused() {
        // The exact predecessor bug: SELECT and the HUD toggle on one key, resolved by
        // whichever handler ran first. Here it never gets configured in the first place.
        let mut map = KeybindMap::defaults();
        let result = map.bind(PhysicalKey::F1, Action::Button(Buttons::SELECT));

        match result {
            Err(BindError::AlreadyBound { key, existing }) => {
                assert_eq!(key, PhysicalKey::F1);
                assert!(existing.contains("HUD"), "{existing}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        // And the original binding is intact.
        assert_eq!(
            map.action_for(PhysicalKey::F1),
            Some(Action::Chrome(ChromeAction::ToggleHud))
        );
    }

    #[test]
    fn the_conflict_rule_is_symmetric() {
        // A chrome action cannot steal an emulated button's key either.
        let mut map = KeybindMap::defaults();
        assert!(map
            .bind(PhysicalKey::Space, Action::Chrome(ChromeAction::SaveState))
            .is_err());
        assert_eq!(
            map.action_for(PhysicalKey::Space),
            Some(Action::Button(Buttons::A))
        );
    }

    #[test]
    fn rebinding_a_key_to_the_action_it_already_holds_is_idempotent() {
        // Re-applying a saved config must not fail against itself.
        let mut map = KeybindMap::defaults();
        assert!(map
            .bind(PhysicalKey::W, Action::Button(Buttons::UP))
            .is_ok());
        assert!(map
            .bind(PhysicalKey::W, Action::Button(Buttons::UP))
            .is_ok());
    }

    #[test]
    fn an_explicit_rebind_displaces_and_reports_what_it_took() {
        // For a UI that has already shown the conflict and had it confirmed.
        let mut map = KeybindMap::defaults();
        let displaced = map.rebind(PhysicalKey::F1, Action::Button(Buttons::SELECT));
        assert_eq!(displaced, Some(Action::Chrome(ChromeAction::ToggleHud)));
        assert_eq!(
            map.action_for(PhysicalKey::F1),
            Some(Action::Button(Buttons::SELECT))
        );
    }

    #[test]
    fn unbinding_frees_the_key_for_the_other_category() {
        let mut map = KeybindMap::defaults();
        assert!(map
            .bind(PhysicalKey::F1, Action::Button(Buttons::SELECT))
            .is_err());
        map.unbind(PhysicalKey::F1);
        assert!(map
            .bind(PhysicalKey::F1, Action::Button(Buttons::SELECT))
            .is_ok());
    }

    #[test]
    fn no_default_binding_is_claimed_twice() {
        // A BTreeMap cannot hold a duplicate key, so this checks the intent instead: every
        // default action is reachable, and the map is as large as the bindings listed.
        let map = KeybindMap::defaults();
        assert_eq!(map.iter().count(), 22);
        assert!(!map.keys_for(Action::Button(Buttons::A)).is_empty());
        assert!(!map
            .keys_for(Action::Chrome(ChromeAction::SaveState))
            .is_empty());
    }

    #[test]
    fn one_action_may_have_several_keys() {
        let mut map = KeybindMap::defaults();
        map.bind(PhysicalKey::ArrowUp, Action::Button(Buttons::UP))
            .unwrap();
        let keys = map.keys_for(Action::Button(Buttons::UP));
        assert_eq!(keys.len(), 2);

        let mut tracker = InputTracker::new();
        tracker.apply(&map, press(PhysicalKey::ArrowUp));
        assert!(tracker.input_state(&map).is_pressed(Buttons::UP));
    }

    // -- Chrome actions ------------------------------------------------------

    #[test]
    fn a_discrete_chrome_action_fires_once_per_press() {
        // Repeating every frame the key is down would write sixty save states a second.
        let map = KeybindMap::defaults();
        let mut tracker = InputTracker::new();

        tracker.apply(&map, press(PhysicalKey::F2));
        assert_eq!(tracker.take_chrome_actions(), vec![ChromeAction::SaveState]);
        assert!(tracker.take_chrome_actions().is_empty(), "not again");

        tracker.apply(&map, release(PhysicalKey::F2));
        tracker.apply(&map, press(PhysicalKey::F2));
        assert_eq!(tracker.take_chrome_actions(), vec![ChromeAction::SaveState]);
    }

    #[test]
    fn auto_repeat_does_not_produce_extra_actions() {
        let map = KeybindMap::defaults();
        let mut tracker = InputTracker::new();
        for _ in 0..10 {
            tracker.apply(&map, press(PhysicalKey::F2));
        }
        assert_eq!(tracker.take_chrome_actions().len(), 1);
    }

    #[test]
    fn a_held_chrome_action_is_queried_rather_than_queued() {
        let map = KeybindMap::defaults();
        let mut tracker = InputTracker::new();

        assert!(!tracker.is_held(&map, ChromeAction::FastForward));
        tracker.apply(&map, press(PhysicalKey::Tab));
        assert!(tracker.is_held(&map, ChromeAction::FastForward));
        assert!(
            tracker.take_chrome_actions().is_empty(),
            "held actions do not queue"
        );

        tracker.apply(&map, release(PhysicalKey::Tab));
        assert!(!tracker.is_held(&map, ChromeAction::FastForward));
    }

    #[test]
    fn a_chrome_press_between_frames_is_not_lost() {
        // Pressed and released within one frame still fires: the action queued on the press.
        let map = KeybindMap::defaults();
        let mut tracker = InputTracker::new();
        tracker.apply(&map, press(PhysicalKey::F3));
        tracker.apply(&map, release(PhysicalKey::F3));
        assert_eq!(tracker.take_chrome_actions(), vec![ChromeAction::LoadState]);
    }

    #[test]
    fn losing_focus_releases_everything() {
        // Otherwise a key held while alt-tabbing stays down forever, because its release goes
        // to another window.
        let map = KeybindMap::defaults();
        let mut tracker = InputTracker::new();
        tracker.apply(&map, press(PhysicalKey::D));
        tracker.apply(&map, press(PhysicalKey::Tab));
        assert!(tracker.input_state(&map).is_pressed(Buttons::RIGHT));

        tracker.release_all();
        assert_eq!(tracker.input_state(&map).buttons, Buttons::empty());
        assert!(!tracker.is_held(&map, ChromeAction::FastForward));
    }

    // -- Touch ---------------------------------------------------------------

    #[test]
    fn touch_travels_alongside_the_buttons() {
        let map = KeybindMap::defaults();
        let mut tracker = InputTracker::new();
        assert_eq!(tracker.input_state(&map).touch, None);

        tracker.set_touch(Some(TouchPoint { x: 120, y: 90 }));
        tracker.apply(&map, press(PhysicalKey::W));
        let state = tracker.input_state(&map);
        assert_eq!(state.touch, Some(TouchPoint { x: 120, y: 90 }));
        assert!(state.is_pressed(Buttons::UP));

        tracker.set_touch(None);
        assert_eq!(tracker.input_state(&map).touch, None);
    }

    // -- Config --------------------------------------------------------------

    #[test]
    fn the_map_round_trips_through_toml() {
        let map = KeybindMap::defaults();
        let text = map.to_toml().unwrap();
        let restored = KeybindMap::from_toml(&text).unwrap();
        assert_eq!(restored, map);
    }

    #[test]
    fn serialization_is_stable_across_saves() {
        // An ordered map, so re-saving an unchanged config produces an identical file rather
        // than a reshuffled diff.
        let map = KeybindMap::defaults();
        assert_eq!(map.to_toml().unwrap(), map.to_toml().unwrap());
    }

    // -- Cross-thread delivery -----------------------------------------------

    #[test]
    fn the_receiver_sees_the_most_recent_state() {
        let (sender, mut receiver) = input_channel();
        sender.send(InputState {
            buttons: Buttons::A,
            touch: None,
        });
        sender.send(InputState {
            buttons: Buttons::B,
            touch: None,
        });
        sender.send(InputState {
            buttons: Buttons::START,
            touch: None,
        });

        // Latest wins; the emulation thread does not work through a backlog of stale input.
        assert_eq!(receiver.latest().buttons, Buttons::START);
    }

    #[test]
    fn the_receiver_holds_the_last_state_when_nothing_new_arrives() {
        // A frame with no input events is a frame where the player is still holding the same
        // buttons, not a frame with nothing pressed.
        let (sender, mut receiver) = input_channel();
        sender.send(InputState {
            buttons: Buttons::RIGHT,
            touch: None,
        });
        assert_eq!(receiver.latest().buttons, Buttons::RIGHT);
        assert_eq!(receiver.latest().buttons, Buttons::RIGHT);
    }

    #[test]
    fn sending_never_blocks_even_when_nothing_reads() {
        let (sender, _receiver) = input_channel();
        for i in 0..10_000 {
            sender.send(InputState {
                buttons: if i % 2 == 0 { Buttons::A } else { Buttons::B },
                touch: None,
            });
        }
    }

    #[test]
    fn a_dropped_receiver_does_not_panic_the_sender() {
        let (sender, receiver) = input_channel();
        drop(receiver);
        sender.send(InputState::default());
    }

    #[test]
    fn touch_survives_the_trip_through_the_packed_slot() {
        // Buttons and touch move together in one word, so the emulation thread can never see
        // a frame that mixes one update's buttons with another's touch point.
        let (sender, mut receiver) = input_channel();
        sender.send(InputState {
            buttons: Buttons::A | Buttons::LEFT,
            touch: Some(TouchPoint { x: 250, y: 190 }),
        });
        let state = receiver.latest();
        assert_eq!(state.buttons, Buttons::A | Buttons::LEFT);
        assert_eq!(state.touch, Some(TouchPoint { x: 250, y: 190 }));

        sender.send(InputState {
            buttons: Buttons::A,
            touch: None,
        });
        assert_eq!(receiver.latest().touch, None);
    }

    #[test]
    fn the_packed_representation_round_trips_every_field() {
        for state in [
            InputState::default(),
            InputState {
                buttons: Buttons::all(),
                touch: Some(TouchPoint {
                    x: u16::MAX,
                    y: u16::MAX,
                }),
            },
            InputState {
                buttons: Buttons::SELECT,
                touch: Some(TouchPoint { x: 0, y: 1 }),
            },
        ] {
            assert_eq!(unpack(pack(state)), state);
        }
    }

    #[test]
    fn the_two_ends_work_across_threads() {
        use std::thread;

        let (sender, mut receiver) = input_channel();
        let writer = thread::spawn(move || {
            for _ in 0..1000 {
                sender.send(InputState {
                    buttons: Buttons::A,
                    touch: None,
                });
                std::thread::yield_now();
            }
        });
        let reader = thread::spawn(move || {
            let mut seen = 0;
            for _ in 0..1000 {
                if receiver.latest().buttons.contains(Buttons::A) {
                    seen += 1;
                }
                std::thread::yield_now();
            }
            seen
        });

        writer.join().unwrap();
        let seen = reader.join().unwrap();
        assert!(seen > 0, "the reader saw the writer's input");
    }
}
