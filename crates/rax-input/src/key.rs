//! Named keys and mouse buttons, as written in profiles.

use serde::{Deserialize, Deserializer, de};

/// A keyboard key, stored as its Windows virtual-key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key(pub u16);

const NAMED: [(&str, u16); 11] = [
    ("Backspace", 0x08),
    ("Tab", 0x09),
    ("Enter", 0x0d),
    ("Shift", 0x10),
    ("Ctrl", 0x11),
    ("Alt", 0x12),
    ("CapsLock", 0x14),
    ("Esc", 0x1b),
    ("Space", 0x20),
    ("Backquote", 0xc0),
    ("Grave", 0xc0),
];

impl Key {
    /// Parses a profile key name: `A`, `7`, `F5`, `Space`, and so on.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        if let [byte @ (b'A'..=b'Z' | b'0'..=b'9')] = name.as_bytes() {
            return Some(Self(u16::from(*byte)));
        }
        if let Some(number) = name.strip_prefix('F').and_then(|n| n.parse::<u16>().ok()) {
            return (1..=12).contains(&number).then(|| Self(0x6f + number));
        }
        NAMED
            .iter()
            .find(|(known, _)| *known == name)
            .map(|&(_, code)| Self(code))
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::parse(&name).ok_or_else(|| de::Error::custom(format!("unknown key: {name}")))
    }
}

/// A mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum MouseButton {
    /// Left button.
    Left,
    /// Right button.
    Right,
    /// Middle button.
    Middle,
    /// Back side button.
    Back,
    /// Forward side button.
    Forward,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits_use_their_ascii_codes() {
        assert_eq!(Key::parse("W"), Some(Key(0x57)));
        assert_eq!(Key::parse("7"), Some(Key(0x37)));
    }

    #[test]
    fn function_keys_map_in_order() {
        assert_eq!(Key::parse("F1"), Some(Key(0x70)));
        assert_eq!(Key::parse("F12"), Some(Key(0x7b)));
    }

    #[test]
    fn named_keys_parse() {
        assert_eq!(Key::parse("Space"), Some(Key(0x20)));
        assert_eq!(Key::parse("Shift"), Some(Key(0x10)));
    }

    #[test]
    fn bad_names_are_rejected() {
        for name in ["", "w", "F0", "F13", "Fx", "Spacebar", "AB"] {
            assert_eq!(Key::parse(name), None, "{name}");
        }
    }
}
