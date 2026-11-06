//! Key-mapping profiles, loaded from JSON.

use std::collections::HashSet;
use std::fmt;

use serde::Deserialize;

use crate::key::{Key, MouseButton};

/// Fingers a profile may hold down at once. Android devices commonly track
/// ten, and one slot is kept for the desktop-style pointer.
pub const MAX_SLOTS: usize = 9;

/// A screen position, from `0.0` to `1.0` on each axis, so one profile works
/// at any resolution.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct Point {
    /// Horizontal position.
    pub x: f32,
    /// Vertical position.
    pub y: f32,
}

impl Point {
    fn is_on_screen(self) -> bool {
        (0.0..=1.0).contains(&self.x) && (0.0..=1.0).contains(&self.y)
    }
}

/// One mapping from a physical control to a virtual finger.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Binding {
    /// Holding the key holds a finger at `at`.
    Key {
        /// Physical key.
        key: Key,
        /// Where the finger goes.
        at: Point,
    },
    /// Holding the button holds a finger at `at`.
    Button {
        /// Physical button.
        button: MouseButton,
        /// Where the finger goes.
        at: Point,
    },
    /// Four keys steer a finger around `center`, like a movement stick.
    Joystick {
        /// Up key.
        up: Key,
        /// Down key.
        down: Key,
        /// Left key.
        left: Key,
        /// Right key.
        right: Key,
        /// Stick centre.
        center: Point,
        /// Stick travel, in screen widths.
        radius: f32,
    },
    /// Mouse movement drags a finger, for aiming.
    Look {
        /// Where each drag starts.
        origin: Point,
        /// Screen width travelled per mouse count.
        sensitivity: f32,
        /// Distance from `origin` at which the finger lifts and re-centres.
        range: f32,
    },
}

/// A complete key map for one game.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Display name.
    pub name: String,
    /// Switches between game controls and a normal pointer.
    pub pointer_toggle: Key,
    /// The mappings.
    pub bindings: Vec<Binding>,
}

/// Why a profile was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// The JSON could not be parsed.
    Parse(String),
    /// A key or button is bound twice.
    DuplicateControl(String),
    /// A position is off screen.
    OffScreen,
    /// More bindings than fingers.
    TooManyBindings(usize),
    /// A joystick radius, sensitivity or range is not a positive number.
    InvalidMagnitude,
    /// More than one `look` binding.
    MultipleLook,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(reason) => write!(f, "invalid profile: {reason}"),
            Self::DuplicateControl(control) => write!(f, "{control} is bound twice"),
            Self::OffScreen => write!(f, "a position is outside 0.0 to 1.0"),
            Self::TooManyBindings(count) => {
                write!(
                    f,
                    "{count} bindings, but only {MAX_SLOTS} fingers are available"
                )
            }
            Self::InvalidMagnitude => {
                write!(f, "radius, sensitivity and range must be positive numbers")
            }
            Self::MultipleLook => write!(f, "only one look binding is allowed"),
        }
    }
}

impl std::error::Error for ProfileError {}

fn positive(value: f32) -> Result<(), ProfileError> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(ProfileError::InvalidMagnitude)
    }
}

fn on_screen(point: Point) -> Result<(), ProfileError> {
    if point.is_on_screen() {
        Ok(())
    } else {
        Err(ProfileError::OffScreen)
    }
}

impl Profile {
    /// Parses and validates a profile.
    pub fn from_json(text: &str) -> Result<Self, ProfileError> {
        let profile: Self =
            serde_json::from_str(text).map_err(|error| ProfileError::Parse(error.to_string()))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Rejects anything that would misbehave at run time.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.bindings.len() > MAX_SLOTS {
            return Err(ProfileError::TooManyBindings(self.bindings.len()));
        }
        let mut keys = HashSet::from([self.pointer_toggle]);
        let mut buttons = HashSet::new();
        let mut look_seen = false;
        let mut claim = |key: Key| {
            if keys.insert(key) {
                Ok(())
            } else {
                Err(ProfileError::DuplicateControl(format!("key {:#x}", key.0)))
            }
        };
        for binding in &self.bindings {
            match binding {
                Binding::Key { key, at } => {
                    claim(*key)?;
                    on_screen(*at)?;
                }
                Binding::Button { button, at } => {
                    if !buttons.insert(*button) {
                        return Err(ProfileError::DuplicateControl(format!("{button:?}")));
                    }
                    on_screen(*at)?;
                }
                Binding::Joystick {
                    up,
                    down,
                    left,
                    right,
                    center,
                    radius,
                } => {
                    for key in [up, down, left, right] {
                        claim(*key)?;
                    }
                    on_screen(*center)?;
                    positive(*radius)?;
                }
                Binding::Look {
                    origin,
                    sensitivity,
                    range,
                } => {
                    if std::mem::replace(&mut look_seen, true) {
                        return Err(ProfileError::MultipleLook);
                    }
                    on_screen(*origin)?;
                    positive(*sensitivity)?;
                    positive(*range)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_bindings(bindings: &str) -> Result<Profile, ProfileError> {
        Profile::from_json(&format!(
            r#"{{"name":"t","pointer_toggle":"Alt","bindings":[{bindings}]}}"#
        ))
    }

    #[test]
    fn the_example_profile_is_valid() {
        let text = include_str!("../../../assets/profiles/example-shooter.json");
        let profile = Profile::from_json(text).unwrap();
        assert_eq!(profile.bindings.len(), 7);
    }

    #[test]
    fn parses_each_binding_type() {
        let profile = with_bindings(
            r#"{"type":"key","key":"R","at":{"x":0.5,"y":0.5}},
               {"type":"button","button":"Left","at":{"x":0.9,"y":0.6}},
               {"type":"joystick","up":"W","down":"S","left":"A","right":"D",
                "center":{"x":0.2,"y":0.7},"radius":0.08},
               {"type":"look","origin":{"x":0.7,"y":0.4},"sensitivity":0.001,"range":0.2}"#,
        )
        .unwrap();
        assert_eq!(profile.bindings.len(), 4);
    }

    #[test]
    fn duplicate_keys_are_rejected() {
        let error = with_bindings(
            r#"{"type":"key","key":"R","at":{"x":0.5,"y":0.5}},
               {"type":"key","key":"R","at":{"x":0.1,"y":0.1}}"#,
        )
        .unwrap_err();
        assert!(matches!(error, ProfileError::DuplicateControl(_)));
    }

    #[test]
    fn a_key_cannot_overlap_the_joystick_or_the_toggle() {
        let overlap = with_bindings(
            r#"{"type":"key","key":"W","at":{"x":0.5,"y":0.5}},
               {"type":"joystick","up":"W","down":"S","left":"A","right":"D",
                "center":{"x":0.2,"y":0.7},"radius":0.08}"#,
        );
        assert!(matches!(overlap, Err(ProfileError::DuplicateControl(_))));
        let toggle = with_bindings(r#"{"type":"key","key":"Alt","at":{"x":0.5,"y":0.5}}"#);
        assert!(matches!(toggle, Err(ProfileError::DuplicateControl(_))));
    }

    #[test]
    fn duplicate_buttons_are_rejected() {
        let error = with_bindings(
            r#"{"type":"button","button":"Left","at":{"x":0.5,"y":0.5}},
               {"type":"button","button":"Left","at":{"x":0.1,"y":0.1}}"#,
        )
        .unwrap_err();
        assert!(matches!(error, ProfileError::DuplicateControl(_)));
    }

    #[test]
    fn off_screen_positions_are_rejected() {
        let error =
            with_bindings(r#"{"type":"key","key":"R","at":{"x":1.5,"y":0.5}}"#).unwrap_err();
        assert_eq!(error, ProfileError::OffScreen);
    }

    #[test]
    fn non_positive_magnitudes_are_rejected() {
        let error = with_bindings(
            r#"{"type":"look","origin":{"x":0.7,"y":0.4},"sensitivity":0.0,"range":0.2}"#,
        )
        .unwrap_err();
        assert_eq!(error, ProfileError::InvalidMagnitude);
    }

    #[test]
    fn two_look_bindings_are_rejected() {
        let look = r#"{"type":"look","origin":{"x":0.7,"y":0.4},"sensitivity":0.1,"range":0.2}"#;
        assert_eq!(
            with_bindings(&format!("{look},{look}")).unwrap_err(),
            ProfileError::MultipleLook
        );
    }

    #[test]
    fn too_many_bindings_are_rejected() {
        let keys: Vec<String> = ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"]
            .iter()
            .map(|k| format!(r#"{{"type":"key","key":"{k}","at":{{"x":0.5,"y":0.5}}}}"#))
            .collect();
        assert_eq!(
            with_bindings(&keys.join(",")).unwrap_err(),
            ProfileError::TooManyBindings(10)
        );
    }

    #[test]
    fn unknown_keys_and_fields_are_rejected() {
        assert!(with_bindings(r#"{"type":"key","key":"Nope","at":{"x":0.5,"y":0.5}}"#).is_err());
        assert!(
            with_bindings(r#"{"type":"key","key":"R","at":{"x":0.5,"y":0.5},"repeat":true}"#)
                .is_err()
        );
    }

    #[test]
    fn broken_json_is_rejected() {
        assert!(matches!(
            Profile::from_json("{"),
            Err(ProfileError::Parse(_))
        ));
    }
}
