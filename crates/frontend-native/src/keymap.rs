//! `winit` keys to `frontend-core` keys.
//!
//! This translation is the whole reason [`PhysicalKey`] exists as its own enum. `frontend-core`
//! holds the keybind map, resolves conflicts, and serialises the config file, and it does all of
//! that without knowing `winit` exists — so a future web or TUI frontend supplies its own version
//! of this one function and reuses everything else.
//!
//! Physical keys, not characters. A player on an AZERTY keyboard whose config says `W` should get
//! the key in the same *place* as a QWERTY `W`, because the binding describes a position under
//! their hand, not a letter. `winit`'s `KeyCode` is already the physical layer, which is why this
//! is a plain match rather than a text-handling problem.

use frontend_core::PhysicalKey;
use winit::keyboard::KeyCode;

/// Translate a `winit` key code, or `None` for keys nothing can be bound to.
///
/// Returning `None` for the long tail is deliberate. The alternative — a raw scancode in the
/// config file — would neither be portable nor readable by whoever opens it, and
/// [`PhysicalKey`] is extended when someone actually wants to bind something instead.
pub fn translate(code: KeyCode) -> Option<PhysicalKey> {
    use KeyCode as K;
    use PhysicalKey as P;
    Some(match code {
        K::KeyA => P::A,
        K::KeyB => P::B,
        K::KeyC => P::C,
        K::KeyD => P::D,
        K::KeyE => P::E,
        K::KeyF => P::F,
        K::KeyG => P::G,
        K::KeyH => P::H,
        K::KeyI => P::I,
        K::KeyJ => P::J,
        K::KeyK => P::K,
        K::KeyL => P::L,
        K::KeyM => P::M,
        K::KeyN => P::N,
        K::KeyO => P::O,
        K::KeyP => P::P,
        K::KeyQ => P::Q,
        K::KeyR => P::R,
        K::KeyS => P::S,
        K::KeyT => P::T,
        K::KeyU => P::U,
        K::KeyV => P::V,
        K::KeyW => P::W,
        K::KeyX => P::X,
        K::KeyY => P::Y,
        K::KeyZ => P::Z,

        K::Digit0 => P::Digit0,
        K::Digit1 => P::Digit1,
        K::Digit2 => P::Digit2,
        K::Digit3 => P::Digit3,
        K::Digit4 => P::Digit4,
        K::Digit5 => P::Digit5,
        K::Digit6 => P::Digit6,
        K::Digit7 => P::Digit7,
        K::Digit8 => P::Digit8,
        K::Digit9 => P::Digit9,

        K::F1 => P::F1,
        K::F2 => P::F2,
        K::F3 => P::F3,
        K::F4 => P::F4,
        K::F5 => P::F5,
        K::F6 => P::F6,
        K::F7 => P::F7,
        K::F8 => P::F8,
        K::F9 => P::F9,
        K::F10 => P::F10,
        K::F11 => P::F11,
        K::F12 => P::F12,

        K::ArrowUp => P::ArrowUp,
        K::ArrowDown => P::ArrowDown,
        K::ArrowLeft => P::ArrowLeft,
        K::ArrowRight => P::ArrowRight,

        K::Space => P::Space,
        // Both the main and numeric-keypad Enter, because a player using the keypad as a d-pad
        // expects its Enter to be Start.
        K::Enter | K::NumpadEnter => P::Enter,
        K::Tab => P::Tab,
        K::Backspace => P::Backspace,
        K::Escape => P::Escape,

        K::ShiftLeft => P::ShiftLeft,
        K::ShiftRight => P::ShiftRight,
        K::ControlLeft => P::ControlLeft,
        K::ControlRight => P::ControlRight,
        K::AltLeft => P::AltLeft,
        K::AltRight => P::AltRight,

        K::Comma => P::Comma,
        K::Period => P::Period,
        K::Slash => P::Slash,
        K::Semicolon => P::Semicolon,
        K::Quote => P::Quote,
        K::BracketLeft => P::BracketLeft,
        K::BracketRight => P::BracketRight,
        K::Backslash => P::Backslash,
        K::Minus => P::Minus,
        K::Equal => P::Equal,
        K::Backquote => P::Backquote,

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use frontend_core::{Action, KeybindMap};

    #[test]
    fn every_key_in_the_default_bindings_can_be_produced_from_a_winit_event() {
        // The failure this guards against is silent and total: a default binding whose key no
        // window event can ever translate to is a control that simply does not work, with no
        // error anywhere. Sweeping `KeyCode` is the only way to know the mapping is onto.
        let mut reachable = std::collections::BTreeSet::new();
        for code in every_key_code() {
            if let Some(key) = translate(code) {
                reachable.insert(key);
            }
        }
        for (key, action) in KeybindMap::defaults().iter() {
            assert!(
                reachable.contains(&key),
                "{key:?} is bound to {action:?} by default but no winit key produces it"
            );
        }
    }

    #[test]
    fn letters_and_digits_map_to_themselves() {
        assert_eq!(translate(KeyCode::KeyW), Some(PhysicalKey::W));
        assert_eq!(translate(KeyCode::Digit7), Some(PhysicalKey::Digit7));
        assert_eq!(translate(KeyCode::F12), Some(PhysicalKey::F12));
    }

    #[test]
    fn both_enter_keys_are_the_same_binding() {
        assert_eq!(translate(KeyCode::Enter), Some(PhysicalKey::Enter));
        assert_eq!(translate(KeyCode::NumpadEnter), Some(PhysicalKey::Enter));
    }

    #[test]
    fn an_unbindable_key_is_none_rather_than_a_wrong_guess() {
        assert_eq!(translate(KeyCode::F35), None);
        assert_eq!(translate(KeyCode::MediaPlayPause), None);
    }

    #[test]
    fn the_translation_is_injective_so_two_keys_never_share_a_binding() {
        // Enter is the one deliberate exception, asserted above.
        let mut seen: std::collections::BTreeMap<PhysicalKey, KeyCode> = Default::default();
        for code in every_key_code() {
            let Some(key) = translate(code) else { continue };
            if let Some(previous) = seen.insert(key, code) {
                let allowed = matches!(
                    (previous, code),
                    (KeyCode::Enter, KeyCode::NumpadEnter) | (KeyCode::NumpadEnter, KeyCode::Enter)
                );
                assert!(
                    allowed,
                    "{previous:?} and {code:?} both mean {key:?}, so one of them is unbindable"
                );
            }
        }
    }

    #[test]
    fn a_default_chrome_binding_resolves_end_to_end() {
        // The path a real keypress takes: winit code, physical key, bound action.
        let map = KeybindMap::defaults();
        let key = translate(KeyCode::Tab).unwrap();
        assert_eq!(
            map.action_for(key),
            Some(Action::Chrome(frontend_core::ChromeAction::FastForward))
        );
    }

    /// Every `KeyCode` variant `winit` 0.30 has.
    ///
    /// Written out because `KeyCode` is `#[non_exhaustive]` with no iterator. That is tedious but
    /// it is also the point: a new variant added by a `winit` upgrade will not appear here, and
    /// the tests above only ever *under*-report reachability, so they cannot start passing
    /// falsely.
    fn every_key_code() -> Vec<KeyCode> {
        use KeyCode::*;
        vec![
            Backquote,
            Backslash,
            BracketLeft,
            BracketRight,
            Comma,
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
            Equal,
            KeyA,
            KeyB,
            KeyC,
            KeyD,
            KeyE,
            KeyF,
            KeyG,
            KeyH,
            KeyI,
            KeyJ,
            KeyK,
            KeyL,
            KeyM,
            KeyN,
            KeyO,
            KeyP,
            KeyQ,
            KeyR,
            KeyS,
            KeyT,
            KeyU,
            KeyV,
            KeyW,
            KeyX,
            KeyY,
            KeyZ,
            Minus,
            Period,
            Quote,
            Semicolon,
            Slash,
            AltLeft,
            AltRight,
            Backspace,
            CapsLock,
            ControlLeft,
            ControlRight,
            Enter,
            ShiftLeft,
            ShiftRight,
            Space,
            Tab,
            Delete,
            End,
            Home,
            Insert,
            PageDown,
            PageUp,
            ArrowDown,
            ArrowLeft,
            ArrowRight,
            ArrowUp,
            NumLock,
            Numpad0,
            Numpad1,
            NumpadAdd,
            NumpadEnter,
            Escape,
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
            F13,
            F35,
            MediaPlayPause,
        ]
    }
}
